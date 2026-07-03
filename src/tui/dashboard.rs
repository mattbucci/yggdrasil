use crate::db::DbPool;
use chrono::{Duration as CDuration, Utc};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentWorkflow};

const PRESSURE_TTL: Duration = Duration::from_secs(5);

use super::ctx_usage::{
    BAR_REF, HARD_DANGER, SOFT_HARD_WARN, agent_context_usage, ctx_color, humanize_tokens,
};

/// Dashboard view — system pulse at top, agents in the middle,
/// meaningful locks table at bottom.
pub struct DashboardView {
    agents: Vec<AgentWorkflow>,
    locks: Vec<crate::lock::ResourceLock>,
    selected: usize,
    agent_name_by_id: HashMap<String, String>,

    // Pulse numbers
    prompts_1h: i64,
    digests_1h: i64,
    cache_hits_24h: i64,
    cache_total_24h: i64,
    redactions_24h: i64,
    prompts_hourly: Vec<u64>, // global, last 24h, oldest→newest
    per_agent_hourly: HashMap<String, [u64; 24]>, // per agent, last 24h
    per_agent_tokens: HashMap<String, (i64, i64)>, // (input, output) from agent_stats
    recent_transitions: Vec<(
        chrono::DateTime<chrono::Utc>,
        String,
        String,
        String,
        Option<String>,
    )>,
    /// Live workers. Separate agent / persona / state so the panel can
    /// render them as proper columns instead of one squashed label.
    /// (task_ref, agent, persona, state_glyph, state_color, title, started_at, tmux_window)
    workers: Vec<WorkerRow>,
    /// Cursor position in the Workers panel — used by the Enter-to-attach
    /// keybind. Only moves when Workers has focus (see DashboardFocus).
    pub worker_sel: usize,
    pub focus: DashboardFocus,

    /// Per-agent context-tokens cache: (tokens, hard_cap). Re-scanning
    /// ~/.claude/projects/*.jsonl on every 500ms render is expensive,
    /// so cache for PRESSURE_TTL.
    pressure_cache: HashMap<String, (Instant, Option<(i64, i64)>)>,

    /// When on, pulse queries filter to the most-recent cc_session_id.
    /// Toggled with `S` on the dashboard pane.
    pub session_scoped: bool,
    /// The session id the pulse is currently scoped to, populated on refresh.
    pub current_session_id: Option<String>,
    /// Live session count per agent, refreshed with the rest of the state.
    pub live_sessions_by_agent: HashMap<String, i64>,

    /// Corpus totals for the system-pulse DB line. Refreshed every tick —
    /// cheap COUNTs on indexed tables.
    pub db_tasks_open: i64,
    pub db_tasks_total: i64,
    pub db_learnings: i64,
    pub db_locks_active: i64,

    /// Inline rename buffer on the agents panel. When `Some`, typed keys
    /// append to the buffer; Enter commits; Esc cancels. Tuple:
    /// `(agent_id_being_renamed, buffer)`.
    pub rename: Option<(String, String)>,
    /// Message input buffer. When `Some`, typed keys append to the body;
    /// Enter sends via msg_cmd::send; Esc cancels. (recipient, body).
    pub msg: Option<(String, String)>,
    /// Short-lived status message for the agents panel (archived / renamed /
    /// rename-failed). Rendered under the agents table; cleared on next
    /// action.
    pub flash: Option<String>,

    /// Throttle for the orphan auto-archiver. Skips the sweep unless at
    /// least `YGG_AUTO_ARCHIVE_INTERVAL_SECS` have passed since the last
    /// run (default 300s).
    orphan_last_check: Option<Instant>,

    // Task runs activity tile
    runs_succeeded: i64,
    runs_failed: i64,
    runs_running: i64,
    runs_queued: i64,
    runs_total: i64,
    runs_recent: Vec<RunSnap>,
    run_spark: Vec<u64>,
    runs_top_loop: i64,
    runs_window: RunsWindow,

    /// When true, only show agents belonging to the current user_id.
    /// Default false (show all users). Persisted in ~/.config/ygg/dashboard.json.
    pub filter_my_agents: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DashboardFocus {
    Agents,
    Workers,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunsWindow {
    Hour1,
    Hour6,
    Hour24,
}

impl RunsWindow {
    fn next(self) -> Self {
        match self {
            Self::Hour1 => Self::Hour6,
            Self::Hour6 => Self::Hour24,
            Self::Hour24 => Self::Hour1,
        }
    }

    fn interval_sql(&self) -> &'static str {
        match self {
            Self::Hour1 => "1 hour",
            Self::Hour6 => "6 hours",
            Self::Hour24 => "24 hours",
        }
    }

    fn bucket_count(&self) -> usize {
        match self {
            Self::Hour1 => 12,
            Self::Hour6 => 24,
            Self::Hour24 => 24,
        }
    }

    fn bucket_seconds(&self) -> i64 {
        match self {
            Self::Hour1 => 300,
            Self::Hour6 => 900,
            Self::Hour24 => 3600,
        }
    }

    fn spark_title(&self, max: u64) -> String {
        let (rate, window) = match self {
            Self::Hour1 => ("5min", "1h"),
            Self::Hour6 => ("15min", "6h"),
            Self::Hour24 => ("hour", "24h"),
        };
        format!(" Runs/{rate} — {window} · peak {max} ")
    }
}

#[derive(Debug, Clone)]
pub struct WorkerRow {
    pub task_ref: String,
    pub agent: String,
    pub persona: Option<String>,
    pub state: String,
    pub title: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub tmux_session: String,
    pub tmux_window: String,
    pub branch_pushed: bool,
    pub branch_merged: bool,
    pub pr_url: Option<String>,
    pub intent: Option<String>,
}

#[derive(Debug, Clone)]
struct RunSnap {
    task_ref: String,
    state: String,
    ended_at: chrono::DateTime<chrono::Utc>,
}

impl Default for DashboardView {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardView {
    pub fn new() -> Self {
        Self {
            agents: vec![],
            locks: vec![],
            selected: 0,
            agent_name_by_id: HashMap::new(),
            prompts_1h: 0,
            digests_1h: 0,
            cache_hits_24h: 0,
            cache_total_24h: 0,
            redactions_24h: 0,
            prompts_hourly: vec![0; 24],
            per_agent_hourly: HashMap::new(),
            per_agent_tokens: HashMap::new(),
            recent_transitions: Vec::new(),
            workers: Vec::new(),
            worker_sel: 0,
            focus: DashboardFocus::Agents,
            pressure_cache: HashMap::new(),
            session_scoped: false,
            current_session_id: None,
            live_sessions_by_agent: HashMap::new(),
            db_tasks_open: 0,
            db_tasks_total: 0,
            db_learnings: 0,
            db_locks_active: 0,
            rename: None,
            msg: None,
            flash: None,
            orphan_last_check: None,
            runs_succeeded: 0,
            runs_failed: 0,
            runs_running: 0,
            runs_queued: 0,
            runs_total: 0,
            runs_recent: Vec::new(),
            run_spark: vec![0; 24],
            runs_top_loop: 0,
            runs_window: RunsWindow::Hour1,
            filter_my_agents: load_filter_my_agents(),
        }
    }

    /// Sweep for orphaned agents and archive them. An agent is orphaned
    /// when its most-recent worker's worktree_path no longer exists on
    /// disk and it hasn't been updated for `YGG_AUTO_ARCHIVE_STALE_SECS`
    /// (default 3600s). Runs at most once per `YGG_AUTO_ARCHIVE_INTERVAL_SECS`
    /// (default 300s). Flashes a status note per archival so the action
    /// is visible in the panel title.
    ///
    /// Disabled by setting `YGG_AUTO_ARCHIVE_ORPHANS=0`.
    async fn sweep_orphans(&mut self, pool: &DbPool) {
        let enabled = std::env::var("YGG_AUTO_ARCHIVE_ORPHANS")
            .map(|v| {
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true);
        if !enabled {
            return;
        }

        let interval = std::env::var("YGG_AUTO_ARCHIVE_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(300);
        let now = Instant::now();
        if let Some(last) = self.orphan_last_check
            && now.duration_since(last).as_secs() < interval
        {
            return;
        }
        self.orphan_last_check = Some(now);

        let stale_secs = std::env::var("YGG_AUTO_ARCHIVE_STALE_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(3600);

        let candidates = match AgentRepo::new(pool, crate::db::user_id())
            .list_orphan_candidates(stale_secs)
            .await
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let repo = AgentRepo::new(pool, crate::db::user_id());
        let mut archived: Vec<String> = Vec::new();
        for (agent_id, agent_name, worktree_path) in candidates {
            if !std::path::Path::new(&worktree_path).exists()
                && repo.archive(&agent_id).await.is_ok()
            {
                archived.push(agent_name);
            }
        }
        if !archived.is_empty() {
            self.flash = Some(format!(
                "auto-archived {} orphan(s): {}",
                archived.len(),
                archived.join(", ")
            ));
        }
    }

    /// Agents-panel rename lifecycle. Mirrors the DAG add-mode pattern.
    pub fn rename_mode(&self) -> bool {
        self.rename.is_some()
    }
    pub fn rename_begin(&mut self) {
        if let Some(a) = self.agents.get(self.selected) {
            self.rename = Some((a.agent_id.clone(), a.agent_name.clone()));
            self.flash = None;
        }
    }
    pub fn rename_cancel(&mut self) {
        self.rename = None;
    }
    pub fn rename_push(&mut self, c: char) {
        if let Some((_, buf)) = self.rename.as_mut() {
            buf.push(c);
        }
    }
    pub fn rename_pop(&mut self) {
        if let Some((_, buf)) = self.rename.as_mut() {
            buf.pop();
        }
    }
    pub fn rename_buffer(&self) -> Option<&str> {
        self.rename.as_ref().map(|(_, b)| b.as_str())
    }

    /// Commit the in-flight rename. On success, refreshes the agents list so
    /// the new name is visible immediately. Sets `flash` regardless of outcome.
    pub async fn rename_commit(&mut self, pool: &DbPool) {
        let Some((agent_id, buf)) = self.rename.take() else {
            return;
        };
        let new_name = buf.trim();
        if new_name.is_empty() {
            self.flash = Some("rename cancelled (empty)".into());
            return;
        }
        let repo = AgentRepo::new(pool, crate::db::user_id());
        match repo.rename(&agent_id, new_name).await {
            Ok(()) => {
                self.flash = Some(format!("renamed → {new_name}"));
                let _ = self.refresh(pool).await;
            }
            Err(e) => {
                let is_unique = e
                    .as_database_error()
                    .is_some_and(|d| d.is_unique_violation());
                self.flash = Some(if is_unique {
                    format!("rename failed: '{new_name}' already exists")
                } else {
                    format!("rename failed: {e}")
                });
            }
        }
    }

    pub fn msg_mode(&self) -> bool {
        self.msg.is_some()
    }
    pub fn msg_begin(&mut self) {
        if let Some(a) = self.agents.get(self.selected) {
            self.msg = Some((a.agent_name.clone(), String::new()));
            self.flash = None;
        }
    }
    pub fn msg_cancel(&mut self) {
        self.msg = None;
    }
    pub fn msg_push(&mut self, c: char) {
        if let Some((_, buf)) = self.msg.as_mut() {
            buf.push(c);
        }
    }
    pub fn msg_pop(&mut self) {
        if let Some((_, buf)) = self.msg.as_mut() {
            buf.pop();
        }
    }
    pub async fn msg_commit(&mut self, pool: &DbPool, from_agent: &str) {
        let Some((to, body)) = self.msg.take() else {
            return;
        };
        let body = body.trim().to_string();
        if body.is_empty() {
            self.flash = Some("message cancelled (empty)".into());
            return;
        }
        match crate::cli::msg_cmd::send(pool, from_agent, &to, &body, true).await {
            Ok(()) => {
                self.flash = Some(format!("sent to {to}"));
            }
            Err(e) => {
                self.flash = Some(format!("send failed: {e}"));
            }
        }
    }

    /// Archive the currently-selected agent. Fires on `a` when no rename is
    /// in flight; refreshes the list so the archived row disappears.
    pub async fn archive_selected(&mut self, pool: &DbPool) {
        let Some(a) = self.agents.get(self.selected).cloned() else {
            return;
        };
        let repo = AgentRepo::new(pool, crate::db::user_id());
        match repo.archive(&a.agent_id).await {
            Ok(()) => {
                self.flash = Some(format!("archived '{}'", a.agent_name));
                let _ = self.refresh(pool).await;
                if self.selected >= self.agents.len() {
                    self.selected = self.agents.len().saturating_sub(1);
                }
            }
            Err(e) => self.flash = Some(format!("archive failed: {e}")),
        }
    }

    pub fn toggle_session_scope(&mut self) {
        self.session_scoped = !self.session_scoped;
    }

    pub fn toggle_user_filter(&mut self) {
        self.filter_my_agents = !self.filter_my_agents;
        save_filter_my_agents(self.filter_my_agents);
    }

    pub fn cycle_runs_window(&mut self) {
        self.runs_window = self.runs_window.next();
    }

    fn cached_pressure(&mut self, agent_name: &str) -> Option<(i64, i64)> {
        let now = Instant::now();
        if let Some((t, v)) = self.pressure_cache.get(agent_name)
            && now.duration_since(*t) < PRESSURE_TTL
        {
            return *v;
        }
        let v = agent_context_usage(agent_name);
        self.pressure_cache.insert(agent_name.to_string(), (now, v));
        v
    }

    pub async fn refresh(&mut self, pool: &DbPool) -> Result<(), anyhow::Error> {
        // Throttled orphan sweep — runs before the list query so any just-
        // archived agents disappear on this same tick.
        self.sweep_orphans(pool).await;

        // Pin selection across re-sorts. self.selected is a positional
        // index, but we sort by updated_at below — without this, the
        // user's cursor would drift to whichever row landed at that
        // index on this tick.
        let pinned_id = self.agents.get(self.selected).map(|a| a.agent_id.clone());

        let agent_repo = AgentRepo::new(pool, crate::db::user_id());
        let mut agents = if self.filter_my_agents {
            agent_repo.list().await?
        } else {
            agent_repo.list_all_users().await?
        };
        // Most-recently-active first. The repo orders by created_at,
        // which buries hot sessions under months-old identities in a
        // 50+ agent fleet.
        agents.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        self.agents = agents;
        self.agent_name_by_id = self
            .agents
            .iter()
            .map(|a| (a.agent_id.clone(), a.agent_name.clone()))
            .collect();

        // Restore selection to the same agent_id it pointed to before
        // the re-sort. Falls back to clamping if that agent vanished.
        if let Some(id) = pinned_id {
            if let Some(idx) = self.agents.iter().position(|a| a.agent_id == id) {
                self.selected = idx;
            } else if self.selected >= self.agents.len() {
                self.selected = self.agents.len().saturating_sub(1);
            }
        }

        let lock_mgr = LockManager::new(pool, 300, crate::db::user_id());
        self.locks = lock_mgr.list_all().await?;

        // Live session counts per agent — surfaces when one identity has
        // multiple concurrent CC sessions racing on the same state.
        self.live_sessions_by_agent = crate::models::session::SessionRepo::new(pool)
            .live_counts()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

        // When session-scoped, lock queries to the most-recent CC session
        // that's emitted events in the last 6h. Avoids showing a freshly-
        // closed session forever; stays sticky while one is active.
        self.current_session_id = if self.session_scoped {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT cc_session_id FROM events
                 WHERE cc_session_id IS NOT NULL
                   AND created_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-6 hours')
                 ORDER BY created_at DESC LIMIT 1",
            )
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten()
        } else {
            None
        };

        // Pulse queries — single roundtrip for the small counters. When a
        // session is in scope we replace the time window with a session
        // match, so the numbers reflect "this session" instead of "last hour
        // globally".
        let since_1h = Utc::now() - CDuration::hours(1);
        let since_24h = Utc::now() - CDuration::hours(24);
        let sid = self.current_session_id.clone();

        // Prompts come from the UserPromptSubmit hook fire — survives
        // ADR 0015's YGG_INJECT=off default. Digests still emit.
        // Recalls (`similarity_hit`) are retrieval-only and dropped per
        // ADR 0015.
        let (p1h, d1h): (i64, i64) = if let Some(sid) = sid.as_deref() {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind = 'hook_fired' AND payload->>'hook' = 'UserPromptSubmit'),
                     COUNT(*) FILTER (WHERE event_kind = 'digest_written')
                   FROM events WHERE cc_session_id = $1"#,
            )
            .bind(sid)
            .fetch_one(pool)
            .await
            .unwrap_or((0, 0))
        } else {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind = 'hook_fired' AND payload->>'hook' = 'UserPromptSubmit'),
                     COUNT(*) FILTER (WHERE event_kind = 'digest_written')
                   FROM events WHERE created_at >= $1"#,
            )
            .bind(since_1h)
            .fetch_one(pool)
            .await
            .unwrap_or((0, 0))
        };
        self.prompts_1h = p1h;
        self.digests_1h = d1h;

        let (ch24, cc24, r24): (i64, i64, i64) = if let Some(sid) = sid.as_deref() {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind = 'embedding_cache_hit'),
                     COUNT(*) FILTER (WHERE event_kind = 'embedding_call'),
                     COUNT(*) FILTER (WHERE event_kind = 'redaction_applied')
                   FROM events WHERE cc_session_id = $1"#,
            )
            .bind(sid)
            .fetch_one(pool)
            .await
            .unwrap_or((0, 0, 0))
        } else {
            sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE event_kind = 'embedding_cache_hit'),
                     COUNT(*) FILTER (WHERE event_kind = 'embedding_call'),
                     COUNT(*) FILTER (WHERE event_kind = 'redaction_applied')
                   FROM events WHERE created_at >= $1"#,
            )
            .bind(since_24h)
            .fetch_one(pool)
            .await
            .unwrap_or((0, 0, 0))
        };
        self.cache_hits_24h = ch24;
        self.cache_total_24h = ch24 + cc24;
        self.redactions_24h = r24;

        // DB corpus totals for the pulse footer line. Single roundtrip —
        // cheap at current scale (all target tables indexed). locks_active
        // excludes expired rows since a held lock is ttl-bound, not just a
        // row existence.
        let (tasks_open, tasks_total, learnings, locks_active): (i64, i64, i64, i64) =
            sqlx::query_as(
                r#"SELECT
                 (SELECT COUNT(*) FROM tasks WHERE status <> 'closed'),
                 (SELECT COUNT(*) FROM tasks),
                 (SELECT COUNT(*) FROM learnings),
                 (SELECT COUNT(*) FROM locks WHERE expires_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))"#,
            )
            .fetch_one(pool)
            .await
            .unwrap_or((0, 0, 0, 0));
        self.db_tasks_open = tasks_open;
        self.db_tasks_total = tasks_total;
        self.db_learnings = learnings;
        self.db_locks_active = locks_active;

        // Prompts per hour sparkline, 24h — global across agents.
        // Uses hook_fired/UserPromptSubmit which fires on every user turn.
        let sparkline: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT (24 - CAST((julianday('now') - julianday(created_at)) * 24 AS INTEGER)),
                    COUNT(*)
             FROM events
             WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-24 hours')
               AND event_kind = 'hook_fired'
               AND payload->>'hook' = 'UserPromptSubmit'
             GROUP BY 1 ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        let mut series = vec![0u64; 24];
        for (b, n) in sparkline {
            let idx = (b - 1).clamp(0, 23) as usize;
            series[idx] = n as u64;
        }
        self.prompts_hourly = series;

        // Per-agent prompts-per-hour sparkline, 24h — one row per (agent, bucket).
        let per_agent: Vec<(String, i32, i64)> = sqlx::query_as(
            "SELECT agent_id,
                    (24 - CAST((julianday('now') - julianday(created_at)) * 24 AS INTEGER)),
                    COUNT(*)
             FROM events
             WHERE created_at >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-24 hours')
               AND event_kind = 'hook_fired'
               AND payload->>'hook' = 'UserPromptSubmit'
               AND agent_id IS NOT NULL
             GROUP BY 1, 2",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        self.per_agent_hourly.clear();
        for (aid, b, n) in per_agent {
            let idx = (b - 1).clamp(0, 23) as usize;
            let entry = self.per_agent_hourly.entry(aid).or_insert([0u64; 24]);
            entry[idx] = n as u64;
        }

        // Per-agent cumulative token totals from agent_stats.
        let agent_tokens: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT agent_id,
                    COALESCE(SUM(input_tokens + cache_read + cache_write), 0),
                    COALESCE(SUM(output_tokens), 0)
             FROM agent_stats GROUP BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        self.per_agent_tokens.clear();
        for (aid, inp, out) in agent_tokens {
            self.per_agent_tokens.insert(aid, (inp, out));
        }

        // Last 5 agent state transitions for the timeline widget.
        let trans: Vec<(chrono::DateTime<chrono::Utc>, String, serde_json::Value)> =
            sqlx::query_as(
                "SELECT created_at, agent_name, payload FROM events
                 WHERE event_kind = 'agent_state_changed'
                 ORDER BY created_at DESC LIMIT 2",
            )
            .fetch_all(pool)
            .await
            .unwrap_or_default();
        self.recent_transitions = trans
            .into_iter()
            .map(|(ts, name, p)| {
                let from = p
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let to = p
                    .get("to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let tool = p.get("tool").and_then(|v| v.as_str()).map(String::from);
                (ts, name, from, to, tool)
            })
            .collect();

        // Piggyback a tmux-window check on every refresh. The external
        // `ygg watch` observer does this too, but not everyone runs it —
        // without this call, workers whose tmux window was killed stay
        // "running" in the panel forever. Cheap: one list-windows per
        // unique tmux session, one UPDATE per absent worker.
        let _ = super::app::reconcile_workers(pool).await;

        // Workers panel reads from the workers table. Observer
        // (yggdrasil-51) maintains state; we just show it.
        let worker_rows: Vec<(
            String,
            i32,
            String,
            Option<String>,
            Option<String>,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
            String,
            String,
            bool,
            bool,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            r#"SELECT r.task_prefix, t.seq, t.title,
                          a.agent_name, a.persona,
                          w.state, w.started_at, w.last_seen_at,
                          w.tmux_session, w.tmux_window,
                          w.branch_pushed, w.branch_merged, w.pr_url,
                          w.intent
                     FROM workers w
                     JOIN tasks t ON t.task_id = w.task_id
                     JOIN repos r ON r.repo_id = t.repo_id
                     LEFT JOIN agents a ON a.agent_id = t.assignee
                    WHERE w.ended_at IS NULL
                       OR (w.ended_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-24 hours')
                           AND (w.branch_pushed = false OR w.branch_merged = false))
                    ORDER BY (w.ended_at IS NULL) DESC, w.started_at DESC
                    LIMIT 15"#,
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        self.workers = worker_rows
            .into_iter()
            .map(
                |(
                    prefix,
                    seq,
                    title,
                    agent,
                    persona,
                    state,
                    started,
                    last_seen,
                    ts_sess,
                    ts_win,
                    pushed,
                    merged,
                    pr,
                    intent,
                )| {
                    WorkerRow {
                        task_ref: format!("{prefix}-{seq}"),
                        agent: agent.unwrap_or_else(|| "unassigned".into()),
                        persona,
                        state,
                        title,
                        started_at: started,
                        last_seen_at: last_seen,
                        tmux_session: ts_sess,
                        tmux_window: ts_win,
                        branch_pushed: pushed,
                        branch_merged: merged,
                        pr_url: pr,
                        intent,
                    }
                },
            )
            .collect();
        if self.worker_sel >= self.workers.len() {
            self.worker_sel = self.workers.len().saturating_sub(1);
        }

        // Task runs activity tile: state breakdown + recent terminal runs + sparkline.
        let (succ, fail, running, queued, total): (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT
               COUNT(*) FILTER (WHERE state = 'succeeded'),
               COUNT(*) FILTER (WHERE state = 'failed'),
               COUNT(*) FILTER (WHERE state = 'running'),
               COUNT(*) FILTER (WHERE state IN ('scheduled', 'ready', 'retrying')),
               COUNT(*)
             FROM task_runs",
        )
        .fetch_one(pool)
        .await
        .unwrap_or((0, 0, 0, 0, 0));
        self.runs_succeeded = succ;
        self.runs_failed = fail;
        self.runs_running = running;
        self.runs_queued = queued;
        self.runs_total = total;

        let recent_rows: Vec<(String, String, chrono::DateTime<chrono::Utc>, String)> =
            sqlx::query_as(
                "SELECT r.state, r.reason, r.ended_at,
                    COALESCE(rp.prefix || '-' || t.seq, LEFT(t.task_id, 8)) AS task_ref
                 FROM task_runs r
                 JOIN tasks t ON t.task_id = r.task_id
                 LEFT JOIN repos rp ON rp.repo_id = t.repo_id
                 WHERE r.state IN ('succeeded', 'failed')
                 ORDER BY r.ended_at DESC NULLS LAST
                 LIMIT 5",
            )
            .fetch_all(pool)
            .await
            .unwrap_or_default();
        self.runs_recent = recent_rows
            .into_iter()
            .map(|(state, _reason, ended_at, task_ref)| RunSnap {
                task_ref,
                state,
                ended_at,
            })
            .collect();

        // Sparkline from run_terminal events, bucketed by runs_window.
        let now = Utc::now();
        let bucket_count = self.runs_window.bucket_count();
        let bucket_secs = self.runs_window.bucket_seconds();
        let interval_sql = self.runs_window.interval_sql();
        let query = format!(
            "SELECT created_at, 1 FROM events \
             WHERE event_kind = 'run_terminal' \
               AND created_at >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-{interval_sql}') \
             ORDER BY created_at"
        );
        let spark_rows: Vec<(chrono::DateTime<chrono::Utc>, i64)> = sqlx::query_as(&query)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
        let mut spark = vec![0u64; bucket_count];
        let window_start = now - CDuration::seconds(bucket_secs * bucket_count as i64);
        for (ts, _) in &spark_rows {
            let elapsed = (*ts - window_start).num_seconds().max(0);
            let idx = (elapsed / bucket_secs) as usize;
            if idx < bucket_count {
                spark[idx] += 1;
            }
        }
        self.run_spark = spark;

        let top_loop: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) c FROM task_runs
              WHERE fingerprint IS NOT NULL
                AND ended_at >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-1 hour')
              GROUP BY fingerprint
              ORDER BY c DESC LIMIT 1",
        )
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
        self.runs_top_loop = top_loop.unwrap_or(0);

        Ok(())
    }

    pub fn select_next(&mut self) {
        if !self.agents.is_empty() {
            self.selected = (self.selected + 1).min(self.agents.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_agent(&self) -> Option<String> {
        self.agents.get(self.selected).map(|a| a.agent_name.clone())
    }

    /// Full selected-agent record (needed to set DAG filter by agent_id).
    pub fn selected_agent_full(&self) -> Option<&AgentWorkflow> {
        self.agents.get(self.selected)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Layout: alerts · pulse · runs · agents (stretch) · workers · locks
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // alerts
                Constraint::Length(6), // system pulse + latest transition
                Constraint::Length(5), // runs tile
                Constraint::Min(8),    // agents
                Constraint::Length(6), // workers
                Constraint::Length(4), // locks summary
            ])
            .split(area);

        self.render_alerts(frame, chunks[0]);
        self.render_pulse(frame, chunks[1]);
        self.render_runs_tile(frame, chunks[2]);
        self.render_agents_table(frame, chunks[3]);
        self.render_workers(frame, chunks[4]);
        self.render_locks_table(frame, chunks[5]);

        if self.rename.is_some() {
            render_rename_overlay(frame, chunks[3], self);
        }
        if self.msg.is_some() {
            render_msg_overlay(frame, chunks[3], self);
        }
    }

    fn render_runs_tile(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        let max = *self.run_spark.iter().max().unwrap_or(&0);
        let spark = Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(self.runs_window.spark_title(max)),
            )
            .data(&self.run_spark)
            .max(max.max(1))
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(spark, cols[0]);

        let pass_pct = if self.runs_total > 0 {
            (self.runs_succeeded as f64 / self.runs_total as f64 * 100.0) as i64
        } else {
            0
        };

        // Line 1: live status
        let line1 = Line::from(vec![
            Span::styled("live      ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} running ", self.runs_running),
                if self.runs_running > 0 {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(
                format!("· {} queued ", self.runs_queued),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!(
                    "· loop-top {}",
                    if self.runs_top_loop > 1 {
                        self.runs_top_loop.to_string()
                    } else {
                        "—".into()
                    }
                ),
                if self.runs_top_loop >= 3 {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
        ]);

        // Line 2: all-time totals
        let line2 = Line::from(vec![
            Span::styled("all-time  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ✓ ", self.runs_succeeded),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!("· {} ✗ ", self.runs_failed),
                if self.runs_failed > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(
                format!("· {} total ", self.runs_total),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("· {pass_pct}% pass"),
                if pass_pct >= 80 {
                    Style::default().fg(Color::Green)
                } else if pass_pct >= 50 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Red)
                },
            ),
        ]);

        // Line 3: recent terminal runs
        let mut recent_spans: Vec<Span> = vec![Span::styled(
            "recent    ",
            Style::default().fg(Color::DarkGray),
        )];
        let now = Utc::now();
        for (i, snap) in self.runs_recent.iter().take(3).enumerate() {
            if i > 0 {
                recent_spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
            }
            let (glyph, color) = if snap.state == "succeeded" {
                ("✓", Color::Green)
            } else {
                ("✗", Color::Red)
            };
            let age_s = (now - snap.ended_at).num_seconds().max(0);
            let age = if age_s < 60 {
                format!("{age_s}s")
            } else if age_s < 3600 {
                format!("{}m", age_s / 60)
            } else if age_s < 86400 {
                format!("{}h", age_s / 3600)
            } else {
                format!("{}d", age_s / 86400)
            };
            let ref_label = if snap.task_ref.len() > 12 {
                &snap.task_ref[..12]
            } else {
                &snap.task_ref
            };
            recent_spans.push(Span::styled(
                format!("{ref_label} {glyph} {age}"),
                Style::default().fg(color),
            ));
        }
        if self.runs_recent.is_empty() {
            recent_spans.push(Span::styled(
                "no completed runs yet",
                Style::default().fg(Color::DarkGray),
            ));
        }
        let line3 = Line::from(recent_spans);

        let title = format!(" Runs · pass {pass_pct}% ");
        let para = Paragraph::new(vec![line1, line2, line3])
            .block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(para, cols[1]);
    }

    fn render_workers(&self, frame: &mut Frame, area: Rect) {
        if self.workers.is_empty() {
            let para = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  · no workers running — spawn one with `r` on DAG or Enter on Tasks",
                    Style::default().fg(Color::DarkGray),
                )),
            ])
            .block(Block::default().borders(Borders::ALL).title(" Workers "));
            frame.render_widget(para, area);
            return;
        }
        let now = Utc::now();
        let focused = self.focus == DashboardFocus::Workers;
        // Count rows that likely need attention — explicit needs_attention
        // OR idle-for-too-long (claude finished, awaiting operator). The
        // watcher's classify_pane only catches permission/trust dialogs;
        // "task complete, cursor parked at the prompt" looks identical to
        // "just paused mid-thought" from a pane capture, so we lean on the
        // last_seen_at age as a proxy.
        const IDLE_ATTN_SECS: i64 = 60;
        let needs_attn_count = self
            .workers
            .iter()
            .filter(|w| {
                let stale = (now - w.last_seen_at).num_seconds().max(0) > IDLE_ATTN_SECS;
                w.state == "needs_attention" || (w.state == "idle" && stale)
            })
            .count();
        let lines: Vec<Line> = self
            .workers
            .iter()
            .enumerate()
            .map(|(i, w)| {
                // Age = seconds since last seen by the observer, NOT since spawn.
                // Answers "is this still alive?" directly. Stale rows (>2m since
                // last seen) dim to DarkGray so abandoned workers stand out even
                // before the reconciler flips state.
                let seen_secs = (now - w.last_seen_at).num_seconds().max(0);
                let age = humanize_duration(seen_secs);
                let age_color = if seen_secs > 120 {
                    Color::DarkGray
                } else {
                    Color::Gray
                };
                let (g, c) = worker_state_style(&w.state);
                let is_cursor = focused && i == self.worker_sel;
                let cursor = if is_cursor { "▸ " } else { "  " };
                // Attention treatment fires on: (1) explicit needs_attention,
                // (2) idle + last_seen older than IDLE_ATTN_SECS. Case 2 is the
                // "task complete, awaiting operator" signal the classifier
                // can't distinguish from pane content alone.
                let idle_stale = w.state == "idle" && seen_secs > IDLE_ATTN_SECS;
                let needs_attn = w.state == "needs_attention" || idle_stale;
                let title_style = if needs_attn {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if is_cursor {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                // When the state is ambiguous (idle-stale), surface the reason
                // in the state cell itself so the operator sees WHY it's yellow.
                let state_label = if idle_stale {
                    format!("idle {age}")
                } else {
                    w.state.clone()
                };
                let state_color = if needs_attn { Color::Yellow } else { c };
                Line::from(vec![
                    Span::styled(cursor, Style::default().fg(Color::Cyan)),
                    Span::styled(
                        format!("{g} "),
                        Style::default().fg(c).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<16}", w.task_ref),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<16}", short_cell(&w.agent, 16)),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        format!(
                            "{:<12}",
                            w.persona
                                .as_deref()
                                .map(|p| short_cell(p, 12))
                                .unwrap_or_else(|| "—".into())
                        ),
                        Style::default().fg(Color::Magenta),
                    ),
                    Span::styled(
                        format!("{:<10}", short_cell(&state_label, 10)),
                        Style::default()
                            .fg(state_color)
                            .add_modifier(if needs_attn {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(
                        format!("{:<12}", delivery_badge(w)),
                        Style::default().fg(delivery_color(w)),
                    ),
                    Span::styled(format!("{age:<6}"), Style::default().fg(age_color)),
                    Span::raw(" "),
                    Span::styled(short_title(&w.title), title_style),
                    if let Some(ref intent) = w.intent {
                        let intent_color = if needs_attn {
                            Color::Yellow
                        } else {
                            Color::DarkGray
                        };
                        Span::styled(
                            format!("  · {}", short_cell(intent, 24)),
                            Style::default().fg(intent_color),
                        )
                    } else {
                        Span::raw("")
                    },
                ])
            })
            .collect();
        let attn_suffix = if needs_attn_count > 0 {
            format!("  ·  ⚠ {needs_attn_count} need attention")
        } else {
            String::new()
        };
        let title = format!(
            " Workers — {}{attn_suffix}  ·  age=since last seen  ·  w=focus  ↑↓  Enter=attach ",
            self.workers.len()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            });
        let para = Paragraph::new(lines).block(block);
        frame.render_widget(para, area);
    }

    pub fn workers_focus(&mut self) {
        self.focus = DashboardFocus::Workers;
        if self.worker_sel >= self.workers.len() {
            self.worker_sel = 0;
        }
    }
    pub fn agents_focus(&mut self) {
        self.focus = DashboardFocus::Agents;
    }

    pub fn worker_up(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        self.worker_sel = if self.worker_sel == 0 {
            self.workers.len() - 1
        } else {
            self.worker_sel - 1
        };
    }
    pub fn worker_down(&mut self) {
        if self.workers.is_empty() {
            return;
        }
        self.worker_sel = (self.worker_sel + 1) % self.workers.len();
    }
    pub fn selected_worker(&self) -> Option<&WorkerRow> {
        self.workers.get(self.worker_sel)
    }

    fn render_alerts(&mut self, frame: &mut Frame, area: Rect) {
        let mut alerts: Vec<Span> = Vec::new();

        // Context alerts use absolute knees only — cap detection isn't
        // 100% reliable across transcript shapes, so any percent-of-cap
        // math would lie. Block ≥ HARD_DANGER (500K), warn ≥ 300K.
        let agent_names: Vec<String> = self.agents.iter().map(|a| a.agent_name.clone()).collect();
        for name in &agent_names {
            if let Some((tokens, _hard)) = self.cached_pressure(name) {
                if tokens >= HARD_DANGER {
                    alerts.push(Span::styled(
                        format!(" ⛔ {name} {} ", humanize_tokens(tokens)),
                        Style::default()
                            .fg(Color::White)
                            .bg(Color::Red)
                            .add_modifier(Modifier::BOLD),
                    ));
                    alerts.push(Span::raw("  "));
                } else if tokens >= SOFT_HARD_WARN {
                    alerts.push(Span::styled(
                        format!(" ⚠ {name} {} ", humanize_tokens(tokens)),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ));
                    alerts.push(Span::raw("  "));
                }
            }
        }

        // Locks held "too long" — 30m+ is our threshold for "someone's probably
        // stuck". Expired locks are just stale state and not interesting.
        let now = Utc::now();
        let long_held = self
            .locks
            .iter()
            .filter(|l| (l.expires_at - now).num_seconds() > 0) // still live
            .filter(|l| (now - l.acquired_at).num_minutes() >= 30)
            .count();
        if long_held > 0 {
            alerts.push(Span::styled(
                format!(
                    " ⏳ {long_held} lock{} held 30m+ ",
                    if long_held == 1 { "" } else { "s" }
                ),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
            alerts.push(Span::raw("  "));
        }

        // Error-state agents
        let errored = self
            .agents
            .iter()
            .filter(|a| a.current_state == crate::models::agent::AgentState::Error)
            .count();
        if errored > 0 {
            alerts.push(Span::styled(
                format!(" ✗ {errored} agent(s) in error "),
                Style::default().fg(Color::White).bg(Color::Red),
            ));
            alerts.push(Span::raw("  "));
        }

        // Redactions in last 24h — informational, not a blocker.
        if self.redactions_24h > 0 {
            alerts.push(Span::styled(
                format!(" 🔒 {} redaction(s) 24h ", self.redactions_24h),
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
        }

        let line = if alerts.is_empty() {
            Line::from(vec![Span::styled(
                "  ✓ all clear — no pressure alerts, no expired locks, no errored agents",
                Style::default().fg(Color::Green),
            )])
        } else {
            Line::from(alerts)
        };

        let para = Paragraph::new(vec![line])
            .block(Block::default().borders(Borders::ALL).title(" Alerts "));
        frame.render_widget(para, area);
    }

    fn render_pulse(&self, frame: &mut Frame, area: Rect) {
        // Split: left = prompts sparkline (visual trend first), right = numeric snapshot
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        // Sparkline on the left
        let max = *self.prompts_hourly.iter().max().unwrap_or(&1);
        let spark = Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                " Prompts / hour — 24h  ·  peak {}  ·  ← old · now → ",
                max
            )))
            .data(&self.prompts_hourly)
            .max(max.max(1))
            .style(Style::default().fg(Color::Cyan));
        frame.render_widget(spark, cols[0]);

        let cache_rate = if self.cache_total_24h > 0 {
            (self.cache_hits_24h as f64 / self.cache_total_24h as f64 * 100.0) as i64
        } else {
            0
        };

        let active = self
            .agents
            .iter()
            .filter(|a| a.current_state == crate::models::agent::AgentState::Executing)
            .count();

        // Session-scope indicator lives in the labels — we re-use the same
        // counter fields but relabel "last 1h / last 24h" to just "session"
        // when scoped, so the numbers aren't misleading.
        let counter_label = if self.current_session_id.is_some() {
            "session   "
        } else {
            "last 1h   "
        };
        let totals_label = if self.current_session_id.is_some() {
            "session   "
        } else {
            "last 24h  "
        };

        let lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("agents    ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} total · {} active", self.agents.len(), active),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(counter_label, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    if self.prompts_1h == 0 && self.digests_1h == 0 {
                        "— prompts · — digests".to_string()
                    } else {
                        format!("{} prompts · {} digests", self.prompts_1h, self.digests_1h)
                    },
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled(totals_label, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "cache {}/{} ({}%) ",
                        self.cache_hits_24h, self.cache_total_24h, cache_rate
                    ),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· redacted {}", self.redactions_24h),
                    if self.redactions_24h > 0 {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().add_modifier(Modifier::BOLD)
                    },
                ),
            ]),
            // Most-recent agent state transition — replaces standalone
            // transitions panel.
            if let Some((ts, name, from, to, tool)) = self.recent_transitions.first() {
                let t = ts
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string();
                let to_color = state_color(to);
                let mut spans = vec![
                    Span::styled(t, Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::styled(name.clone(), Style::default().fg(Color::Cyan)),
                    Span::raw("  "),
                    Span::styled(from.clone(), Style::default().fg(Color::DarkGray)),
                    Span::raw(" → "),
                    Span::styled(
                        to.clone(),
                        Style::default().fg(to_color).add_modifier(Modifier::BOLD),
                    ),
                ];
                if let Some(t) = tool {
                    spans.push(Span::styled(
                        format!(" ({t})"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                Line::from(spans)
            } else {
                Line::from(Span::styled(
                    "          no recent transitions",
                    Style::default().fg(Color::DarkGray),
                ))
            },
        ];
        let title = match &self.current_session_id {
            Some(sid) => {
                let head: String = sid.chars().take(8).collect();
                format!(" System pulse — session {head}…  (S=global) ")
            }
            None => {
                let hint = if self.session_scoped {
                    " — no recent session  (S=global) "
                } else {
                    "  (S=session) "
                };
                format!(" System pulse{} ", hint)
            }
        };
        let pulse =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(pulse, cols[1]);
    }

    fn render_agents_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("NAME").style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Cell::from("STATE"),
            Cell::from("CTX"),
            Cell::from("TOKENS"),
            Cell::from("24H"),
            Cell::from("UPDATED"),
        ])
        .height(1);

        // Shared y-axis for comparable sparklines across agents.
        let global_max = self
            .per_agent_hourly
            .values()
            .flat_map(|s| s.iter().copied())
            .max()
            .unwrap_or(1)
            .max(1);

        // Pre-compute cached pressure per agent — we can't call &mut self
        // inside the iter closure below.
        let mut pressure_by_name: HashMap<String, Option<(i64, i64)>> = HashMap::new();
        let agent_names: Vec<String> = self.agents.iter().map(|a| a.agent_name.clone()).collect();
        for n in &agent_names {
            pressure_by_name.insert(n.clone(), self.cached_pressure(n));
        }

        let selected = self.selected;
        let per_agent = &self.per_agent_hourly;
        let per_tokens = &self.per_agent_tokens;
        let rows: Vec<Row> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let style = if i == selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                // Humanize: the enum names read like internals ("context_flush")
                // but most of them map to an intuitive one-word label.
                let (base_label, state_color): (&str, Color) = match agent.current_state {
                    crate::models::agent::AgentState::Idle => ("idle", Color::Gray),
                    crate::models::agent::AgentState::Planning => ("planning", Color::Cyan),
                    crate::models::agent::AgentState::Executing => ("working", Color::Green),
                    crate::models::agent::AgentState::WaitingTool => ("tool", Color::Yellow),
                    crate::models::agent::AgentState::ContextFlush => ("digesting", Color::Magenta),
                    crate::models::agent::AgentState::HumanOverride => ("paused", Color::Yellow),
                    crate::models::agent::AgentState::Mediation => ("mediating", Color::Cyan),
                    crate::models::agent::AgentState::Error => ("error", Color::Red),
                    crate::models::agent::AgentState::Shutdown => ("shutdown", Color::DarkGray),
                };
                // For WaitingTool, append the tool name (e.g. "tool: Bash") from metadata
                let mut state_label: String = if matches!(
                    agent.current_state,
                    crate::models::agent::AgentState::WaitingTool
                ) {
                    match agent.metadata.get("last_tool").and_then(|v| v.as_str()) {
                        Some(t) if !t.is_empty() => format!("{base_label}: {t}"),
                        _ => base_label.to_string(),
                    }
                } else {
                    base_label.to_string()
                };
                // Staleness: if the row says we're active but hasn't been
                // updated in 10+ minutes, the CC session is probably dead.
                // Visual-only — no mutation — dim color + "(stale)" suffix
                // so a dead "tool: Bash" doesn't read identical to a live one.
                let is_active_state = matches!(
                    agent.current_state,
                    crate::models::agent::AgentState::Executing
                        | crate::models::agent::AgentState::WaitingTool
                        | crate::models::agent::AgentState::Planning
                        | crate::models::agent::AgentState::ContextFlush
                );
                let idle_mins = (Utc::now() - agent.updated_at).num_minutes();
                let state_color = if is_active_state && idle_mins >= 10 {
                    state_label.push_str(" (stale)");
                    Color::DarkGray
                } else {
                    state_color
                };

                // CTX comes from the live `usage` block in the agent's CC
                // transcript (cache_read + cache_creation + input + output).
                // Bar fills against a fixed BAR_REF (1M) — each cell is
                // 100K — so position is meaningful even when the per-
                // session hard-cap detection misses. Color is keyed off
                // absolute degradation knees, not the cap.
                let (pressure_bar, pressure_color) =
                    match pressure_by_name.get(&agent.agent_name).copied().flatten() {
                        Some((tokens, _hard_cap)) => {
                            let blocks = ((tokens * 10) / BAR_REF).clamp(0, 10) as usize;
                            (
                                format!(
                                    "{}{} {}",
                                    "█".repeat(blocks),
                                    "░".repeat(10 - blocks),
                                    humanize_tokens(tokens),
                                ),
                                ctx_color(tokens),
                            )
                        }
                        _ => ("—".to_string(), Color::DarkGray),
                    };

                let sparkline = per_agent
                    .get(&agent.agent_id)
                    .map(|s| text_sparkline(s, global_max))
                    .unwrap_or_else(|| "        ".to_string());

                // Attach a "×N" badge when an agent has >1 live CC session —
                // means multiple windows are racing on the same identity.
                // Persona, when set, appears as " :role" so you can tell two
                // personas of the same repo apart at a glance.
                let live = self
                    .live_sessions_by_agent
                    .get(&agent.agent_id)
                    .copied()
                    .unwrap_or(0);
                let base = match &agent.persona {
                    Some(p) if !p.is_empty() => format!("{} :{p}", agent.agent_name),
                    _ => agent.agent_name.clone(),
                };
                let name_cell = if live > 1 {
                    format!("{base}  ({live} sessions)")
                } else {
                    base
                };

                // Dormant rows: explicitly Idle, OR an agent the watcher
                // hasn't seen update in 30+ minutes (regardless of what
                // state it claims). 30m is well past the digest cadence,
                // so anything quieter than that is a parked window. Mute
                // the whole row — name, state, CTX bar, sparkline — so a
                // bright bar on a long-cold session can't compete with a
                // live agent in trouble.
                let is_idle = matches!(agent.current_state, crate::models::agent::AgentState::Idle);
                let is_dormant = is_idle || idle_mins >= 30;
                // Selection bg is DarkGray; if we also painted dormant
                // text DarkGray the row would vanish under the cursor.
                // Lift to plain Gray when selected so the contrast stays.
                let dormant_fg = if i == selected {
                    Color::Gray
                } else {
                    Color::DarkGray
                };
                // Token totals from agent_stats (in + out).
                let token_label = match per_tokens.get(&agent.agent_id) {
                    Some((inp, out)) if *inp + *out > 0 => {
                        format!("{}↓ {}↑", humanize_tokens(*inp), humanize_tokens(*out))
                    }
                    _ => "—".to_string(),
                };

                let (name_color, state_fg, ctx_fg, tok_fg, spark_fg) = if is_dormant {
                    (dormant_fg, dormant_fg, dormant_fg, dormant_fg, dormant_fg)
                } else {
                    (
                        Color::Reset,
                        state_color,
                        pressure_color,
                        Color::White,
                        Color::Cyan,
                    )
                };

                Row::new(vec![
                    Cell::from(name_cell).style(Style::default().fg(name_color)),
                    Cell::from(state_label).style(Style::default().fg(state_fg)),
                    Cell::from(pressure_bar).style(Style::default().fg(ctx_fg)),
                    Cell::from(token_label).style(Style::default().fg(tok_fg)),
                    Cell::from(sparkline).style(Style::default().fg(spark_fg)),
                    Cell::from(humanize_since(agent.updated_at))
                        .style(Style::default().fg(name_color)),
                ])
                .style(style)
            })
            .collect();

        // Surface action status (rename/archive) in the panel title so the
        // user gets feedback without a separate status line.
        let scope_label = if self.filter_my_agents { "mine" } else { "all" };
        let title = match self.flash.as_deref() {
            Some(msg) => {
                format!(" Agents ({scope_label})  ·  {msg}  ·  u=filter r=rename a=archive m=msg")
            }
            None => format!(" Agents ({scope_label})  ·  u=filter r=rename a=archive m=msg"),
        };
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(20),
                Constraint::Percentage(13),
                Constraint::Percentage(22),
                Constraint::Percentage(15),
                Constraint::Percentage(12),
                Constraint::Percentage(18),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title));

        frame.render_widget(table, area);
    }

    fn render_locks_table(&self, frame: &mut Frame, area: Rect) {
        // Summary-only — detailed view lives on the Locks tab ([0]). The
        // dashboard surfaces (a) total live, (b) stale (>30m held), (c) the
        // longest-held single lock so a stuck resource is still obvious.
        let now = Utc::now();
        let live: Vec<&crate::lock::ResourceLock> = self
            .locks
            .iter()
            .filter(|l| (l.expires_at - now).num_seconds() > 0)
            .collect();
        let total = live.len();
        let stale = live
            .iter()
            .filter(|l| (now - l.acquired_at).num_seconds() > 1800)
            .count();
        let oldest = live
            .iter()
            .max_by_key(|l| (now - l.acquired_at).num_seconds())
            .map(|l| {
                let held = humanize_duration((now - l.acquired_at).num_seconds().max(0));
                let agent = self
                    .agent_name_by_id
                    .get(&l.agent_id)
                    .cloned()
                    .unwrap_or_else(|| format!("{}…", &l.agent_id.to_string()[..8]));
                format!("{held} by {agent} on {}", short_resource(&l.resource_key))
            });

        let stale_color = if stale > 0 {
            Color::Yellow
        } else {
            Color::Green
        };
        let lines = vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{total}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" live  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{stale}"),
                    Style::default()
                        .fg(stale_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" stale (>30m)  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "[9] Locks for detail",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]),
            match oldest {
                Some(s) => Line::from(vec![
                    Span::styled("  longest: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(s, Style::default().fg(Color::Gray)),
                ]),
                None => Line::from(""),
            },
        ];
        let para =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Locks "));
        frame.render_widget(para, area);
    }
}

/// Trim a resource key (usually an abs path) to the last two path
/// components plus an ellipsis prefix. Keeps the informative tail visible.
fn short_resource(s: &str) -> String {
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return s.to_string();
    }
    let tail = parts[parts.len() - 2..].join("/");
    format!("…/{tail}")
}

fn humanize_since(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0);
    humanize_duration(secs) + " ago"
}

fn state_color(s: &str) -> Color {
    match s {
        "idle" => Color::Gray,
        "executing" | "working" => Color::Green,
        "waiting_tool" | "tool" => Color::Yellow,
        "context_flush" | "digesting" => Color::Magenta,
        "error" => Color::Red,
        "human_override" | "paused" => Color::Yellow,
        "planning" | "mediation" => Color::Cyan,
        _ => Color::Gray,
    }
}

/// Render a 24-hour series as block-char sparkline. `max` is the shared y-axis
/// so values across rows are visually comparable.
fn text_sparkline(series: &[u64], max: u64) -> String {
    // 8-step bar chars, empty cell for zero so sparse agents look sparse.
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = max.max(1);
    series
        .iter()
        .map(|&v| {
            if v == 0 {
                ' '
            } else {
                let step = (v * 7).div_ceil(max).min(7) as usize;
                BARS[step]
            }
        })
        .collect()
}

fn short_title(s: &str) -> String {
    if s.chars().count() <= 60 {
        s.to_string()
    } else {
        s.chars().take(60).collect::<String>() + "…"
    }
}

fn short_cell(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn delivery_badge(w: &WorkerRow) -> &'static str {
    // Only meaningful for terminated workers.
    let terminated = matches!(w.state.as_str(), "completed" | "failed" | "abandoned");
    if !terminated {
        return "";
    }
    if w.branch_merged {
        "✓ merged"
    } else if w.pr_url.is_some() {
        "⏺ pr-open"
    } else if w.branch_pushed {
        "⬆ pushed"
    } else {
        "⬆ unpushed"
    }
}

fn delivery_color(w: &WorkerRow) -> Color {
    if w.branch_merged {
        Color::Green
    } else if w.pr_url.is_some() {
        Color::Cyan
    } else if w.branch_pushed {
        Color::Yellow
    } else if matches!(w.state.as_str(), "completed" | "failed" | "abandoned") {
        Color::Red
    } else {
        Color::DarkGray
    }
}

fn worker_state_style(state: &str) -> (&'static str, Color) {
    match state {
        "spawned" => ("◌", Color::DarkGray),
        "running" => ("▶", Color::Green),
        "idle" => ("•", Color::Gray),
        "needs_attention" => ("⚠", Color::Yellow),
        "completed" => ("✓", Color::DarkGray),
        "failed" => ("✗", Color::Red),
        "abandoned" => ("⊘", Color::DarkGray),
        _ => ("?", Color::Gray),
    }
}

fn humanize_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Centered popup on the agents pane area while a rename is in progress.
/// Clear + bordered block with the old name and an inline input buffer.
fn render_rename_overlay(frame: &mut Frame, area: Rect, dash: &DashboardView) {
    let Some((_, buf)) = dash.rename.as_ref() else {
        return;
    };
    let old = dash
        .agents
        .iter()
        .find(|a| a.agent_id == dash.rename.as_ref().unwrap().0)
        .map(|a| a.agent_name.as_str())
        .unwrap_or("?");
    let w = area.width.saturating_sub(8).min(70);
    let h = 6u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);

    let hint = format!(" rename '{old}'  ·  Enter=commit · Esc=cancel ");
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("▸ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                buf.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(hint)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    frame.render_widget(para, popup);
}

fn render_msg_overlay(frame: &mut Frame, area: Rect, dash: &DashboardView) {
    let Some((to, buf)) = dash.msg.as_ref() else {
        return;
    };
    let w = area.width.saturating_sub(8).min(70);
    let h = 5u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);

    let hint = format!(" message → {to}  ·  Enter=send · Esc=cancel ");
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("▸ ", Style::default().fg(Color::Yellow)),
            Span::styled(
                buf.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Yellow)),
        ]),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(hint)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    frame.render_widget(para, popup);
}

fn dashboard_settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/ygg/dashboard.json")
}

fn load_filter_my_agents() -> bool {
    let path = dashboard_settings_path();
    let Ok(data) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
        return false;
    };
    v.get("filter_my_agents")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn save_filter_my_agents(val: bool) {
    let path = dashboard_settings_path();
    let mut obj = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
    {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("filter_my_agents".into(), serde_json::Value::Bool(val));
    if let Ok(json) = serde_json::to_string_pretty(&obj) {
        let _ = std::fs::write(&path, json);
    }
}
