use crate::db::DbPool;
use chrono::Timelike;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::prelude::*;
use std::io;
use std::time::Duration;

use crate::config::AppConfig;

use super::chat_view::ChatView;
use super::dag_view::DagView;
use super::dashboard::DashboardView;
use super::locks_view::LocksView;
use super::log_view::LogView;
use super::run_grid::RunGridView;
use super::runs_view::RunsView;
use super::tasks_view::TasksView;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActiveView {
    Dashboard,
    Dag,
    Tasks,
    Logs,
    Locks,
    Runs,
    RunGrid,
    Nerdy,
    Chat,
}

pub struct App {
    pub active_view: ActiveView,
    pub should_quit: bool,
    pub dashboard: DashboardView,
    pub dag: DagView,
    pub tasks: TasksView,
    pub logs: LogView,
    pub locks: LocksView,
    pub runs: RunsView,
    pub run_grid: RunGridView,
    pub nerdy: super::nerdy::NerdyView,
    pub chat: ChatView,
    pub agent_name: String,
    /// Recent events shown in the global bottom status bar across all panes.
    pub status_tail: Vec<(String, String, String)>, // (hh:mm:ss, kind, one-line detail)
    /// Right-hand-side orchestration stats (filled each refresh tick).
    pub ops_stats: OpsStats,
    /// Per-cell flash state for value changes between refreshes (yggdrasil-152).
    pub flash: FlashState,
    /// When Enter on the Workers panel fires, we defer the actual tmux
    /// attach until after ratatui's alternate-screen teardown to avoid
    /// a half-mode-switched terminal. Set here, read in `run` post-loop.
    pub attach_pending: Option<(String, String)>, // (session, window)
    /// Rolling history of `agents_alive` over the last
    /// `SPARK_HISTORY_LEN` refreshes (yggdrasil-150). Rendered as a
    /// Unicode block sparkline in the orchestration panel so liveness
    /// trend is visible at a glance.
    pub alive_history: SparkBuffer,
    /// Pending toasts (yggdrasil-153). Each render expires stale ones.
    pub toasts: Vec<Toast>,
    /// k9s-style floating detail overlay (yggdrasil-151). `Some(_)` means
    /// the overlay is open and intercepts navigation keys; `None` means
    /// the underlying pane handles input normally.
    pub detail_overlay: Option<DetailOverlay>,
    /// Help overlay state (yggdrasil-132). Toggled by `?`; while open
    /// every key but `?` and Esc is intercepted.
    pub help: super::help::HelpOverlay,
    /// Repo-vs-all scope (yggdrasil-134). Many panes default to current
    /// repo; toggling to All exposes cross-repo state. Stored centrally
    /// so panes share the same scope rather than each tracking its own.
    pub scope: Scope,
    /// Cascade ripple queue (yggdrasil-166). When a task closes (or
    /// any signature event) fires, push a ripple keyed by task_ref;
    /// pane renderers consult `is_active_for_distance` to decide
    /// whether to paint REVERSED on a row offset.
    pub ripples: super::motion::RippleQueue,
    /// Set of run-terminal event ids already converted into ripples.
    /// Prevents the same `RunTerminal` showing up in successive
    /// `status_tail` snapshots from re-pushing.
    pub seen_terminal_keys: std::collections::HashSet<u64>,
}

/// Which slice of the database the panes filter to. `Repo` is the
/// current cwd's registered repo (the default — most users care only
/// about today's work); `All` widens every cwd-scoped query to global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    #[default]
    Repo,
    All,
}

impl Scope {
    pub fn toggle(&mut self) {
        *self = match self {
            Self::Repo => Self::All,
            Self::All => Self::Repo,
        };
    }

    /// Short label for the chrome ("repo" / "all").
    pub fn label(&self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::All => "all",
        }
    }
}

impl App {
    /// Open the detail overlay over the current pane.
    pub fn open_detail(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.detail_overlay = Some(DetailOverlay {
            title: title.into(),
            body: body.into(),
        });
    }

    /// Close the overlay if open. No-op when nothing is showing.
    pub fn close_detail(&mut self) {
        self.detail_overlay = None;
    }
}

/// yggdrasil-151: floating detail overlay (k9s `:describe` pattern). The
/// overlay floats over the current pane, dimming the background, and Esc
/// returns to the exact row. Distinct from drill-stack, which *replaces*
/// the pane.
#[derive(Debug, Clone)]
pub struct DetailOverlay {
    pub title: String,
    pub body: String,
}

/// How many recent samples to keep for in-panel sparklines. 30 samples
/// at the existing 500ms refresh tick = 15 s of history, the sweet spot
/// between "useful trend" and "fits on a single 10-glyph cell".
pub const SPARK_HISTORY_LEN: usize = 30;

/// yggdrasil-150: bounded ring buffer feeding a Unicode block sparkline.
/// Pushes drop the oldest sample when the cap is reached. `glyphs(width)`
/// renders the last `width` samples normalized against the largest value
/// in the buffer; an empty or all-zero buffer renders to spaces.
#[derive(Debug, Clone)]
pub struct SparkBuffer {
    pub samples: std::collections::VecDeque<u64>,
    pub cap: usize,
}

impl SparkBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, value: u64) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    /// Render the most recent `width` samples as Unicode block glyphs.
    /// Empty buffer returns `width` spaces so the sparkline never shifts
    /// the surrounding layout.
    pub fn glyphs(&self, width: usize) -> String {
        const LEVELS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        if width == 0 {
            return String::new();
        }
        if self.samples.is_empty() {
            return " ".repeat(width);
        }
        let take_n = width.min(self.samples.len());
        let pad_n = width - take_n;
        let max = *self.samples.iter().max().unwrap_or(&0).max(&1);
        let start = self.samples.len() - take_n;
        let mut out = " ".repeat(pad_n);
        for &v in self.samples.iter().skip(start) {
            // Map [0..=max] → [0..=8] glyph index. saturating_mul keeps
            // pathological max=u64::MAX cases from panicking.
            let idx = if max == 0 {
                0
            } else {
                ((v.saturating_mul(8)) / max).min(8) as usize
            };
            out.push(LEVELS[idx]);
        }
        out
    }
}

impl App {
    /// Push a transient toast onto the stack. Renders above the status
    /// strip until `ttl` elapses or `TOAST_MAX_VISIBLE` newer toasts have
    /// landed.
    pub fn push_toast(&mut self, msg: impl Into<String>, severity: ToastSeverity, ttl: Duration) {
        self.toasts.push(Toast {
            msg: msg.into(),
            severity,
            expires_at: std::time::Instant::now() + ttl,
        });
    }

    /// Drop expired toasts and clamp to the most recent
    /// `TOAST_MAX_VISIBLE`. Called inline before rendering and exposed for
    /// testability.
    pub fn prune_toasts(&mut self) {
        let now = std::time::Instant::now();
        self.toasts.retain(|t| t.expires_at > now);
        if self.toasts.len() > TOAST_MAX_VISIBLE {
            let drop = self.toasts.len() - TOAST_MAX_VISIBLE;
            self.toasts.drain(..drop);
        }
    }

    /// Number of toast rows the layout should reserve right now. 0 when
    /// nothing is pending; capped at TOAST_MAX_VISIBLE.
    pub fn visible_toast_rows(&self) -> u16 {
        self.toasts.len().min(TOAST_MAX_VISIBLE) as u16
    }
}

/// yggdrasil-153: transient toast strip above the persistent status bar.
/// Helix's two-row pattern — persistent state below, fading event echoes
/// above. Used for "lock acquired src/foo.rs", scheduler errors, hook
/// drift warnings — anything worth surfacing without stealing a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastSeverity {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub msg: String,
    pub severity: ToastSeverity,
    pub expires_at: std::time::Instant,
}

/// Default time-to-live for a toast. 3 s matches Helix's transient row
/// and is long enough to read a short line without lingering.
pub const TOAST_DEFAULT_TTL: Duration = Duration::from_secs(3);

/// Cap on simultaneously-visible toasts. Older ones fall off the top of
/// the queue. Keeps the strip from eating an unbounded number of rows
/// when many events fire in a tick.
pub const TOAST_MAX_VISIBLE: usize = 3;

/// Lightweight orchestration snapshot rendered on the right side of the
/// global status strip. Cheap queries — every 500ms tick is fine.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct OpsStats {
    pub agents_alive: i64,  // != idle and updated in last 10m
    pub agents_stuck: i64,  // active-state but updated > 10m ago
    pub tasks_running: i64, // tasks.status = in_progress
    pub live_sessions: i64, // sessions.ended_at IS NULL
    pub db_ms: u64,         // round-trip ping

    /// yggdrasil-148: rolling burn rate. `agent_stats.period` is
    /// hour-bucketed (see `src/stats/tracker.rs`), so the finest-grain
    /// rate we can derive is "current hour so far". Treat the partial
    /// hour as a rolling window: extrapolate to per-minute by dividing
    /// by minutes elapsed since the top of the hour.
    pub tokens_per_min: f64,
    pub cost_today_usd: f64,
    pub tokens_today: i64,

    /// yggdrasil-177: live DB-side signals. The status strip shows
    /// pool saturation + event throughput so "is anything broken" is
    /// answerable without leaving the dashboard.
    pub pool_used: u32,
    pub pool_max: u32,
    pub events_per_min: i64,
}

/// yggdrasil-148: hide cost displays during screencasts / demos. Honors
/// the same truthy values as the other YGG_TUI_NO_* knobs.
pub fn cost_hidden() -> bool {
    matches!(
        std::env::var("YGG_TUI_NO_COST").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Rate-from-bucketed-counter helper. The hour-bucketed `agent_stats`
/// table can only tell us "tokens since the top of the hour"; to get
/// tokens/min we divide by minutes elapsed since the bucket boundary.
/// Floor at 1 minute so a freshly-started session reads its first
/// hour's tokens as a per-minute rate without dividing by zero.
pub fn tokens_per_minute(hour_tokens: i64, minutes_into_hour: u32) -> f64 {
    let denom = minutes_into_hour.max(1) as f64;
    hour_tokens as f64 / denom
}

/// Compact "tokens/min" formatter. Rates above 1k humanize to `1.2k/min`
/// so the orchestration panel column stays narrow.
pub fn format_tokens_per_min(rate: f64) -> String {
    if rate >= 1000.0 {
        format!("{:.1}k tok/min", rate / 1000.0)
    } else {
        format!("{:.0} tok/min", rate)
    }
}

/// yggdrasil-152: cell-level "value changed" flash. After a refresh, any
/// number that moved between previous and current snapshots gets painted
/// REVERSED for `flash_frames` paint passes, then settles back. Cheap
/// per-pane fanout for any future panes that want this effect — start
/// with the ops panel, expand later.
///
/// Disable globally with `YGG_TUI_NO_FLASH=1`.
#[derive(Default, Clone)]
pub struct FlashState {
    pub agents_alive: u8,
    pub agents_stuck: u8,
    pub tasks_running: u8,
    pub live_sessions: u8,
}

impl FlashState {
    /// Bump the cells that differ from `prev`. Default 2 frames at the
    /// pre-existing 500ms tick = ~1s of inverted paint, the sweet spot
    /// where the eye catches it without it feeling laggy.
    pub fn mark_changes(&mut self, prev: &OpsStats, next: &OpsStats, frames: u8) {
        let frames = if flash_disabled() { 0 } else { frames };
        if prev.agents_alive != next.agents_alive {
            self.agents_alive = frames;
        }
        if prev.agents_stuck != next.agents_stuck {
            self.agents_stuck = frames;
        }
        if prev.tasks_running != next.tasks_running {
            self.tasks_running = frames;
        }
        if prev.live_sessions != next.live_sessions {
            self.live_sessions = frames;
        }
    }

    /// Saturating decrement on every paint pass; cells return to their
    /// quiet style once the counter hits zero.
    pub fn tick_paint(&mut self) {
        self.agents_alive = self.agents_alive.saturating_sub(1);
        self.agents_stuck = self.agents_stuck.saturating_sub(1);
        self.tasks_running = self.tasks_running.saturating_sub(1);
        self.live_sessions = self.live_sessions.saturating_sub(1);
    }

    pub fn is_flashing_alive(&self) -> bool {
        self.agents_alive > 0
    }
    pub fn is_flashing_stuck(&self) -> bool {
        self.agents_stuck > 0
    }
    pub fn is_flashing_tasks(&self) -> bool {
        self.tasks_running > 0
    }
    pub fn is_flashing_sessions(&self) -> bool {
        self.live_sessions > 0
    }
}

fn flash_disabled() -> bool {
    matches!(
        std::env::var("YGG_TUI_NO_FLASH").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

impl App {
    pub fn new(agent_name: String) -> Self {
        Self {
            active_view: ActiveView::Dashboard,
            should_quit: false,
            dashboard: DashboardView::new(),
            dag: DagView::new(),
            tasks: TasksView::new(),
            logs: LogView::new(),
            locks: LocksView::new(),
            runs: RunsView::new(),
            run_grid: RunGridView::new(),
            nerdy: super::nerdy::NerdyView::new(),
            chat: ChatView::new(),
            agent_name,
            status_tail: Vec::new(),
            ops_stats: OpsStats::default(),
            flash: FlashState::default(),
            attach_pending: None,
            alive_history: SparkBuffer::new(SPARK_HISTORY_LEN),
            toasts: Vec::new(),
            detail_overlay: None,
            help: super::help::HelpOverlay::default(),
            scope: Scope::default(),
            ripples: super::motion::RippleQueue::default(),
            seen_terminal_keys: std::collections::HashSet::new(),
        }
    }

    /// Pull the 3 most-recent events for the global bottom strip.
    pub async fn refresh_status_tail(&mut self, pool: &DbPool) {
        let rows: Vec<(chrono::DateTime<chrono::Utc>, String, serde_json::Value)> = sqlx::query_as(
            "SELECT created_at, event_kind, payload
                 FROM events ORDER BY created_at DESC LIMIT 3",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

        // yggdrasil-166: push a cascade ripple for any new run_terminal
        // event we haven't seen before. The seen-set is keyed on the
        // (created_at, task_ref) hash so a second tick that pulls the
        // same row doesn't double-fire.
        for (t, k, p) in &rows {
            if k == "run_terminal" {
                let task_ref = p["task_ref"].as_str().unwrap_or("?");
                let key = ripple_key(t, task_ref);
                if self.seen_terminal_keys.insert(key) {
                    self.ripples.push(key);
                }
            }
        }
        // Cap the seen-set so a long-running TUI doesn't accumulate
        // hashes forever. The window is the same as the rolling sparkline
        // (30) since events expire from status_tail much sooner.
        if self.seen_terminal_keys.len() > 256 {
            self.seen_terminal_keys.clear();
        }

        self.status_tail = rows
            .into_iter()
            .rev()
            .map(|(t, k, p)| {
                let ts = t
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string();
                (ts, k, short_status_detail(&p))
            })
            .collect();

        // Orchestration snapshot. One roundtrip: agents live/stuck, tasks
        // running, sessions live.
        let db_start = std::time::Instant::now();
        let (alive, stuck, running, sessions): (i64, i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT
              (SELECT COUNT(*) FROM agents
                WHERE archived_at IS NULL
                  AND current_state <> 'idle'
                  AND updated_at >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-10 minutes')),
              (SELECT COUNT(*) FROM agents
                WHERE archived_at IS NULL
                  AND current_state IN ('executing','waiting_tool','planning','context_flush')
                  AND updated_at <  strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-10 minutes')),
              (SELECT COUNT(*) FROM tasks WHERE status = 'in_progress'),
              (SELECT COUNT(*) FROM sessions WHERE ended_at IS NULL)
            "#,
        )
        .fetch_one(pool)
        .await
        .unwrap_or((0, 0, 0, 0));
        // yggdrasil-152: snapshot prev → next so the renderer flashes any
        // moved cells. Two paint passes ≈ ~1s of inverted highlight at the
        // existing 500ms refresh tick.
        let prev = self.ops_stats.clone();
        let next = OpsStats {
            agents_alive: alive,
            agents_stuck: stuck,
            tasks_running: running,
            live_sessions: sessions,
            db_ms: db_start.elapsed().as_millis() as u64,
            // Burn-rate fields are populated below once the cost-hidden
            // gate clears; carry forward the previous tick's values
            // until then so the flash decorator doesn't fire spuriously.
            tokens_per_min: self.ops_stats.tokens_per_min,
            cost_today_usd: self.ops_stats.cost_today_usd,
            tokens_today: self.ops_stats.tokens_today,
            // yggdrasil-177: pool / events-per-min get populated in the
            // dedicated query block further down. Carry forward to keep
            // flash markings stable.
            pool_used: self.ops_stats.pool_used,
            pool_max: self.ops_stats.pool_max,
            events_per_min: self.ops_stats.events_per_min,
        };
        self.flash.mark_changes(&prev, &next, 2);
        self.ops_stats = next;
        // yggdrasil-150: feed the in-panel sparkline.
        self.alive_history.push(alive.max(0) as u64);

        // yggdrasil-177: pool saturation + event-rate. sqlx::Pool exposes
        // size + num_idle synchronously; the event count is one round-trip.
        self.ops_stats.pool_used = pool.size().saturating_sub(pool.num_idle() as u32);
        self.ops_stats.pool_max = pool.options().get_max_connections();
        if let Ok(count) = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM events WHERE created_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-1 minute')",
        )
        .fetch_one(pool)
        .await
        {
            self.ops_stats.events_per_min = count;
        }

        // yggdrasil-148: burn-rate snapshot. One round-trip pulls the
        // current-hour tokens (for tokens/min extrapolation) and the
        // since-midnight UTC totals (for "today" cost + tokens).
        if !cost_hidden() {
            let burn: Option<(i64, f64, i64)> = sqlx::query_as(
                r#"
                SELECT
                  COALESCE((SELECT CAST(SUM(input_tokens + output_tokens) AS INTEGER)
                            FROM agent_stats
                            WHERE period = strftime('%Y-%m-%dT%H:00:00+00:00','now')), 0),
                  COALESCE((SELECT CAST(SUM(estimated_cost) AS REAL)
                            FROM agent_stats
                            WHERE period >= strftime('%Y-%m-%dT00:00:00+00:00','now')), 0.0),
                  COALESCE((SELECT CAST(SUM(input_tokens + output_tokens) AS INTEGER)
                            FROM agent_stats
                            WHERE period >= strftime('%Y-%m-%dT00:00:00+00:00','now')), 0)
                "#,
            )
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
            if let Some((hour_tokens, today_cost, today_tokens)) = burn {
                let minutes_in: u32 = chrono::Utc::now().minute();
                self.ops_stats.tokens_per_min = tokens_per_minute(hour_tokens, minutes_in);
                self.ops_stats.cost_today_usd = today_cost;
                self.ops_stats.tokens_today = today_tokens;
            }
        }
    }

    pub async fn handle_key(&mut self, pool: &DbPool, code: KeyCode, modifiers: KeyModifiers) {
        // yggdrasil-155: Tasks-pane inline rename — capture all keys for
        // the input buffer. Esc cancels, Enter commits, Backspace pops,
        // ctrl-c quits. Sits ahead of every other handler so the buffer
        // can't be drained by global keys.
        if self.active_view == ActiveView::Tasks && self.tasks.rename_mode() {
            match code {
                KeyCode::Esc => self.tasks.rename_cancel(),
                KeyCode::Backspace => self.tasks.rename_pop(),
                KeyCode::Enter => {
                    if let Err(e) = self.tasks.rename_commit(pool).await {
                        self.tasks.flash = format!("rename failed: {e}");
                    }
                    let _ = self.tasks.refresh(pool).await;
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                KeyCode::Char(c) => self.tasks.rename_push(c),
                _ => {}
            }
            return;
        }

        // DAG add-mode ('n' input overlay) captures most keys. Takes
        // precedence over Query focus since they never overlap.
        if self.active_view == ActiveView::Dag && self.dag.add_mode() {
            match code {
                KeyCode::Esc => self.dag.add_cancel(),
                KeyCode::Backspace => self.dag.add_pop(),
                KeyCode::Enter => {
                    if let Some((parent, title)) = self.dag.add_commit() {
                        let agent = std::env::var("YGG_AGENT_NAME")
                            .ok()
                            .unwrap_or_else(|| self.agent_name.clone());
                        let result = match parent {
                            Some(p) => {
                                crate::cli::plan_cmd::add(pool, &p, &title, None, None, &[], &agent)
                                    .await
                                    .map(|_| format!("added under {p}"))
                            }
                            None => crate::cli::plan_cmd::create(pool, &title, None, &agent)
                                .await
                                .map(|t| format!("created epic seq={}", t.seq)),
                        };
                        self.dag.flash = match result {
                            Ok(msg) => msg,
                            Err(e) => format!("add failed: {e}"),
                        };
                        let _ = self.dag.refresh(pool).await;
                    }
                }
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.dag.add_push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Dashboard inline-rename overlay captures keys while active.
        // Mirrors the DAG add-mode pattern; Enter commits, Esc cancels,
        // Ctrl-C still quits.
        if self.active_view == ActiveView::Dashboard && self.dashboard.rename_mode() {
            match code {
                KeyCode::Esc => self.dashboard.rename_cancel(),
                KeyCode::Backspace => self.dashboard.rename_pop(),
                KeyCode::Enter => self.dashboard.rename_commit(pool).await,
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.dashboard.rename_push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Dashboard message-input overlay captures keys while active.
        if self.active_view == ActiveView::Dashboard && self.dashboard.msg_mode() {
            match code {
                KeyCode::Esc => self.dashboard.msg_cancel(),
                KeyCode::Backspace => self.dashboard.msg_pop(),
                KeyCode::Enter => {
                    let from = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    self.dashboard.msg_commit(pool, &from).await;
                }
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.dashboard.msg_push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Chat compose overlay captures keys while active.
        if self.active_view == ActiveView::Chat && self.chat.composing() {
            match code {
                KeyCode::Esc => self.chat.compose_cancel(),
                KeyCode::Backspace => self.chat.compose_pop(),
                KeyCode::Enter => {
                    let from = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    self.chat.compose_commit(pool, &from).await;
                }
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.chat.compose_push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Chat detail overlay — Esc closes.
        if self.active_view == ActiveView::Chat && self.chat.detail_open() {
            match code {
                KeyCode::Esc => self.chat.detail_close(),
                KeyCode::Char(c) if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        // Chat filter-editing mode captures keys while typing.
        if self.active_view == ActiveView::Chat && self.chat.filter_editing() {
            match code {
                KeyCode::Esc => self.chat.filter_clear(),
                KeyCode::Enter => self.chat.filter_accept(),
                KeyCode::Backspace => self.chat.filter_pop(),
                KeyCode::Char(c) => {
                    if modifiers.contains(KeyModifiers::CONTROL) && c == 'c' {
                        self.should_quit = true;
                    } else {
                        self.chat.filter_push(c);
                    }
                }
                _ => {}
            }
            return;
        }

        // Pending delete-confirm takes precedence — whatever the next key is,
        // it either commits (`y`) or cancels. Ctrl-C still quits. Prevents a
        // stray keystroke from firing a tab switch while the prompt is armed.
        match self.active_view {
            ActiveView::Dag if self.dag.pending_delete.is_some() => {
                if matches!(code, KeyCode::Char(c) if c == 'y' || c == 'Y') {
                    if let Some(id) = self.dag.take_pending_delete() {
                        let label = id.to_string()[..8].to_string();
                        match crate::models::task::TaskRepo::new(pool).delete(&id).await {
                            Ok(()) => self.dag.flash = format!("deleted {label}"),
                            Err(e) => self.dag.flash = format!("delete failed: {e}"),
                        }
                        let _ = self.dag.refresh(pool).await;
                    }
                } else if matches!(code, KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.should_quit = true;
                } else {
                    self.dag.delete_cancel();
                }
                return;
            }
            ActiveView::Tasks if self.tasks.pending_delete.is_some() => {
                if matches!(code, KeyCode::Char(c) if c == 'y' || c == 'Y') {
                    if let Some(id) = self.tasks.take_pending_delete() {
                        let label = id.to_string()[..8].to_string();
                        match crate::models::task::TaskRepo::new(pool).delete(&id).await {
                            Ok(()) => self.tasks.set_flash(format!("deleted {label}")),
                            Err(e) => self.tasks.set_flash(format!("delete failed: {e}")),
                        }
                        let _ = self.tasks.refresh(pool).await;
                    }
                } else if matches!(code, KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL))
                {
                    self.should_quit = true;
                } else {
                    self.tasks.delete_cancel();
                }
                return;
            }
            _ => {}
        }

        // yggdrasil-132: help overlay open intercepts every key except
        // ? (toggle close) and ctrl-c (quit). Sits ahead of the detail
        // overlay gate so ? always closes whichever overlay is on top.
        if self.help.open {
            match code {
                KeyCode::Esc | KeyCode::Char('?') => self.help.close(),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        // yggdrasil-151: when the floating detail overlay is open, almost
        // every key is intercepted — only Esc (close) and ctrl-c (quit)
        // pass through. This stops navigation/scroll/refresh keys from
        // mutating panes the user is trying to read about.
        if self.detail_overlay.is_some() {
            match code {
                KeyCode::Esc => self.close_detail(),
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            }
            return;
        }

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('?') => self.help.toggle(),
            KeyCode::Backspace if self.active_view == ActiveView::Dag => {
                self.dag.delete_begin();
            }
            KeyCode::Backspace if self.active_view == ActiveView::Tasks => {
                self.tasks.delete_begin();
            }
            KeyCode::Char('1') => self.set_view(ActiveView::Dashboard),
            KeyCode::Char('2') => self.set_view(ActiveView::Dag),
            KeyCode::Char('3') => self.set_view(ActiveView::Tasks),
            KeyCode::Char('5') => self.set_view(ActiveView::Logs),
            KeyCode::Char('9') => self.set_view(ActiveView::Locks),
            KeyCode::Char('0') => self.set_view(ActiveView::Runs),
            KeyCode::Char('G') => self.set_view(ActiveView::RunGrid),
            KeyCode::Char('N') => self.set_view(ActiveView::Nerdy),
            KeyCode::Char('C') => self.set_view(ActiveView::Chat),
            // yggdrasil-151: open the detail overlay populated from the
            // current pane's selected row. Per-pane adapters: Tasks
            // shows the full title + description + acceptance + design
            // + notes. Other panes can wire `open_detail(...)` as
            // followups.
            KeyCode::Char('d') if self.active_view == ActiveView::Tasks => {
                if let Some((task, prefix)) = self.tasks.selected_task() {
                    let title = format!("{prefix}-{} · {}", task.seq, task.title);
                    let mut body = String::new();
                    if !task.description.is_empty() {
                        body.push_str(&task.description);
                        body.push_str("\n\n");
                    }
                    if let Some(a) = &task.acceptance {
                        body.push_str("Acceptance:\n");
                        body.push_str(a);
                        body.push_str("\n\n");
                    }
                    if let Some(d) = &task.design {
                        body.push_str("Design:\n");
                        body.push_str(d);
                        body.push_str("\n\n");
                    }
                    if let Some(n) = &task.notes {
                        body.push_str("Notes:\n");
                        body.push_str(n);
                    }
                    if body.trim().is_empty() {
                        body = "(no description, acceptance, design, or notes)".into();
                    }
                    self.open_detail(title, body);
                }
            }
            KeyCode::Char('f') if self.active_view == ActiveView::Runs => {
                self.runs.cycle_filter();
                let _ = self.runs.refresh(pool).await;
            }
            KeyCode::Tab | KeyCode::Right => self.cycle_view_forward(),
            KeyCode::BackTab | KeyCode::Left => self.cycle_view_backward(),
            KeyCode::Char('f') if self.active_view == ActiveView::Logs => {
                self.logs.cycle_filter();
            }
            KeyCode::Char('s') if self.active_view == ActiveView::Dag => {
                self.dag.cycle_sort();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('a') if self.active_view == ActiveView::Dag => {
                self.dag.cycle_agent_filter();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('f') if self.active_view == ActiveView::Dag => {
                self.dag.toggle_subtree_focus();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('c') if self.active_view == ActiveView::Dag => {
                self.dag.clear_filters();
                let _ = self.dag.refresh(pool).await;
            }
            KeyCode::Char('c') if self.active_view == ActiveView::Chat => {
                self.chat.compose_begin();
            }
            KeyCode::Enter if self.active_view == ActiveView::Chat => {
                // Enter on a broadcast = claim; on a directed msg = detail overlay.
                self.chat.detail_selected();
                if !self.chat.detail_open() {
                    // Was not a directed msg, so try claiming as broadcast.
                    let agent = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    self.chat.claim_selected(pool, &agent).await;
                }
            }
            KeyCode::Char('/') if self.active_view == ActiveView::Chat => {
                self.chat.filter_begin();
            }
            KeyCode::Esc if self.active_view == ActiveView::Chat => {
                // Esc clears active filter when not in a sub-mode.
                if self.chat.filter_editing() {
                    self.chat.filter_clear();
                } else {
                    self.chat.filter_clear();
                }
            }
            // yggdrasil-155: lazygit-style inline rename of the selected
            // task's title. 'r' is already taken (run-task), so 'e' for
            // edit-title — discoverable via the help row's hint.
            KeyCode::Char('e') if self.active_view == ActiveView::Tasks => {
                self.tasks.rename_begin();
            }
            KeyCode::Char('r') if self.active_view == ActiveView::Tasks => {
                // Run the selected task from Tasks pane. Mirrors DAG 'r'.
                if let Some(task_ref) = self.tasks.selected_task_ref() {
                    let agent = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    let silent = |_: &str| {};
                    match crate::cli::plan_cmd::run_with_reporter(
                        pool, &task_ref, &agent, false, &silent,
                    )
                    .await
                    {
                        Ok(headline) => self.tasks.set_flash(headline),
                        Err(e) => self.tasks.set_flash(format!("run failed: {e}")),
                    }
                    let _ = self.tasks.refresh(pool).await;
                }
            }
            KeyCode::Char('r') if self.active_view == ActiveView::Dag && !self.dag.add_mode() => {
                if let Some(task_ref) = self.dag.selected_task_ref() {
                    let agent = std::env::var("YGG_AGENT_NAME")
                        .ok()
                        .unwrap_or_else(|| self.agent_name.clone());
                    let silent = |_: &str| {};
                    match crate::cli::plan_cmd::run_with_reporter(
                        pool, &task_ref, &agent, false, &silent,
                    )
                    .await
                    {
                        Ok(headline) => self.dag.flash = headline,
                        Err(e) => self.dag.flash = format!("run failed: {e}"),
                    }
                    let _ = self.dag.refresh(pool).await;
                }
            }
            KeyCode::Char('n') if self.active_view == ActiveView::Dag && !self.dag.add_mode() => {
                self.dag.add_begin();
            }
            KeyCode::Char('u') if self.active_view == ActiveView::Dashboard => {
                self.dashboard.toggle_user_filter();
                let _ = self.dashboard.refresh(pool).await;
            }
            KeyCode::Char('t') if self.active_view == ActiveView::Dashboard => {
                self.dashboard.cycle_runs_window();
                let _ = self.dashboard.refresh(pool).await;
            }
            KeyCode::Char('S') if self.active_view == ActiveView::Dashboard => {
                self.dashboard.toggle_session_scope();
                let _ = self.dashboard.refresh(pool).await;
            }
            // yggdrasil-134: global repo/all scope toggle. Outside the
            // dashboard (which has its own session-scope on 'S'), 'S'
            // flips between current-repo and all-repos for any pane
            // that consults App.scope.
            KeyCode::Char('S') => {
                self.scope.toggle();
            }
            KeyCode::Char('r')
                if self.active_view == ActiveView::Dashboard
                    && self.dashboard.focus == super::dashboard::DashboardFocus::Agents =>
            {
                self.dashboard.rename_begin();
            }
            KeyCode::Char('a')
                if self.active_view == ActiveView::Dashboard
                    && self.dashboard.focus == super::dashboard::DashboardFocus::Agents =>
            {
                self.dashboard.archive_selected(pool).await;
            }
            KeyCode::Char('m')
                if self.active_view == ActiveView::Dashboard
                    && self.dashboard.focus == super::dashboard::DashboardFocus::Agents =>
            {
                self.dashboard.msg_begin();
            }
            KeyCode::Char('w') if self.active_view == ActiveView::Dashboard => {
                // Toggle focus between agents + workers panel. Whatever's
                // focused gets arrow-key + Enter attention.
                match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Agents => self.dashboard.workers_focus(),
                    super::dashboard::DashboardFocus::Workers => self.dashboard.agents_focus(),
                }
            }
            KeyCode::Char('r') if self.active_view == ActiveView::Locks => {
                let cfg = AppConfig::from_env().ok();
                let ttl = cfg.as_ref().map(|c| c.lock_ttl_secs).unwrap_or(300);
                self.locks.release_selected(pool, ttl).await;
            }
            KeyCode::Up => match self.active_view {
                ActiveView::Dag => self.dag.scroll_up(),
                ActiveView::Dashboard => match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Agents => self.dashboard.select_prev(),
                    super::dashboard::DashboardFocus::Workers => self.dashboard.worker_up(),
                },
                ActiveView::Tasks => self.tasks.select_prev(),
                ActiveView::Logs => self.logs.scroll_up(),
                ActiveView::Locks => self.locks.select_prev(),
                ActiveView::Runs => self.runs.select_prev(),
                ActiveView::RunGrid => self.run_grid.select_prev(),
                ActiveView::Chat => self.chat.select_prev(),
                _ => {}
            },
            KeyCode::Down => match self.active_view {
                ActiveView::Dag => self.dag.scroll_down(),
                ActiveView::Dashboard => match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Agents => self.dashboard.select_next(),
                    super::dashboard::DashboardFocus::Workers => self.dashboard.worker_down(),
                },
                ActiveView::Tasks => self.tasks.select_next(),
                ActiveView::Logs => self.logs.scroll_down(),
                ActiveView::Locks => self.locks.select_next(),
                ActiveView::Runs => self.runs.select_next(),
                ActiveView::RunGrid => self.run_grid.select_next(),
                ActiveView::Chat => self.chat.select_next(),
                _ => {}
            },
            KeyCode::Enter => match self.active_view {
                ActiveView::Dashboard => match self.dashboard.focus {
                    super::dashboard::DashboardFocus::Workers => {
                        // Attach to the selected worker's tmux window.
                        // Actual tmux exec runs after we tear down ratatui
                        // in the outer `run` loop — writing to a pending
                        // slot avoids half-mode-switched terminals.
                        if let Some(w) = self.dashboard.selected_worker().cloned() {
                            self.attach_pending = Some((w.tmux_session, w.tmux_window));
                            self.should_quit = true;
                        }
                    }
                    super::dashboard::DashboardFocus::Agents => {
                        // Jump to DAG with the owner filter pre-set to the
                        // agent whose row was selected.
                        if let Some(agent) = self.dashboard.selected_agent_full().cloned() {
                            self.dag.agent_filter =
                                super::dag_view::AgentFilter::Specific(agent.agent_id);
                            let _ = self.dag.refresh(pool).await;
                            self.set_view(ActiveView::Dag);
                        }
                    }
                },
                ActiveView::Dag => self.dag.toggle_detail(),
                ActiveView::Logs => self.logs.toggle_detail(),
                ActiveView::Tasks => self.tasks.toggle_detail(),
                _ => {}
            },
            KeyCode::Esc => match self.active_view {
                ActiveView::Dag if self.dag.detail_open => self.dag.detail_open = false,
                ActiveView::Tasks if self.tasks.detail_open => self.tasks.detail_open = false,
                _ => {}
            },
            _ => {}
        }
    }

    fn set_view(&mut self, v: ActiveView) {
        self.active_view = v;
        if v == ActiveView::Chat {
            self.chat.mark_seen();
        }
    }
    fn cycle_view_forward(&mut self) {
        let next = match self.active_view {
            ActiveView::Dashboard => ActiveView::Dag,
            ActiveView::Dag => ActiveView::Tasks,
            ActiveView::Tasks => ActiveView::Logs,
            ActiveView::Logs => ActiveView::Locks,
            ActiveView::Locks => ActiveView::Runs,
            ActiveView::Runs => ActiveView::RunGrid,
            ActiveView::RunGrid => ActiveView::Nerdy,
            ActiveView::Nerdy => ActiveView::Chat,
            ActiveView::Chat => ActiveView::Dashboard,
        };
        self.set_view(next);
    }
    fn cycle_view_backward(&mut self) {
        let prev = match self.active_view {
            ActiveView::Dashboard => ActiveView::Chat,
            ActiveView::Dag => ActiveView::Dashboard,
            ActiveView::Tasks => ActiveView::Dag,
            ActiveView::Logs => ActiveView::Tasks,
            ActiveView::Locks => ActiveView::Logs,
            ActiveView::Runs => ActiveView::Locks,
            ActiveView::RunGrid => ActiveView::Runs,
            ActiveView::Nerdy => ActiveView::RunGrid,
            ActiveView::Chat => ActiveView::Nerdy,
        };
        self.set_view(prev);
    }

    /// Paint the whole TUI into `frame`. Extracted so the run loop can call it
    /// both before and after a refresh — painting before refresh lets each
    /// view's own "loading" state be visible while the DB query blocks.
    /// Test-only entry point so integration tests can exercise the layout
    /// against `TestBackend` without spinning up a Postgres connection.
    /// Production code goes through `run` → `draw`.
    #[doc(hidden)]
    pub fn draw_for_test(&mut self, frame: &mut Frame) {
        self.draw(frame)
    }

    fn draw(&mut self, frame: &mut Frame) {
        // yggdrasil-153: drop expired toasts before computing the layout
        // so reserved row count and rendered content agree.
        self.prune_toasts();
        let toast_rows = self.visible_toast_rows();

        let area = frame.area();
        // yggdrasil-156: narrow-terminal collapse. Below 100 cols the wide
        // `[1] Dashboard [2] DAG …` row wraps and the right-hand ops-stats
        // panel crowds out the event tail. Drop both to compact equivalents.
        let narrow = area.width < NARROW_TERMINAL_THRESHOLD;
        // yggdrasil-153: reserve toast rows above the status strip when
        // any toasts are pending; otherwise the layout matches the
        // pre-toast tab/hint/main/strip shape exactly.
        let mut constraints: Vec<Constraint> = vec![
            Constraint::Length(1), // tab row
            Constraint::Length(1), // context-sensitive help row
            Constraint::Min(0),    // active pane
        ];
        if toast_rows > 0 {
            constraints.push(Constraint::Length(toast_rows));
        }
        constraints.push(Constraint::Length(3)); // global status strip
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        let strip_idx = chunks.len() - 1;
        let toast_idx = if toast_rows > 0 {
            Some(strip_idx - 1)
        } else {
            None
        };

        let tab = |label: &str, active: bool| -> Span<'static> {
            if active {
                Span::styled(
                    format!(" {label} "),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                )
            } else {
                Span::styled(format!(" {label} "), Style::default().fg(Color::Gray))
            }
        };

        let tabs: Vec<Span<'static>> = if narrow {
            // Compact tabs: just the activator digit/letter. The full label
            // moves to the help row's pane_hint, which already shows the
            // active view's affordances.
            [
                ("1", ActiveView::Dashboard),
                ("2", ActiveView::Dag),
                ("3", ActiveView::Tasks),
                ("5", ActiveView::Logs),
                ("9", ActiveView::Locks),
                ("0", ActiveView::Runs),
                ("G", ActiveView::RunGrid),
                ("N", ActiveView::Nerdy),
            ]
            .iter()
            .map(|(k, v)| tab(k, self.active_view == *v))
            .collect()
        } else {
            vec![
                tab("[1] Dashboard", self.active_view == ActiveView::Dashboard),
                tab("[2] DAG", self.active_view == ActiveView::Dag),
                tab("[3] Tasks", self.active_view == ActiveView::Tasks),
                tab("[5] Logs", self.active_view == ActiveView::Logs),
                tab("[9] Locks", self.active_view == ActiveView::Locks),
                tab("[0] Runs", self.active_view == ActiveView::Runs),
                tab("[G] Grid", self.active_view == ActiveView::RunGrid),
                tab("[N] Nerdy", self.active_view == ActiveView::Nerdy),
                {
                    let unread = self.chat.unread_count();
                    let label = if unread > 0 && self.active_view != ActiveView::Chat {
                        format!("[C] Chat ({unread})")
                    } else {
                        "[C] Chat".into()
                    };
                    tab(&label, self.active_view == ActiveView::Chat)
                },
            ]
        };
        frame.render_widget(Line::from(tabs), chunks[0]);

        let nav_span = Span::styled(
            "  q=quit  ←→/tab=nav  Enter=detail  ",
            Style::default().fg(Color::DarkGray),
        );
        let pane_hint = match self.active_view {
            ActiveView::Dashboard => "S=session-scope  t=window  u=mine/all",
            ActiveView::Dag => {
                "Enter=detail  r=run  n=add  ⌫=delete  s=sort  a=agent  f=focus  c=clear"
            }
            ActiveView::Tasks => {
                "↑↓ select  ·  Enter=detail  ·  d=overlay  ·  e=rename  ·  r=run  ·  ⌫=delete"
            }
            ActiveView::Logs => "f=filter  Enter=detail",
            ActiveView::Locks => "↑↓ select  ·  r=release",
            ActiveView::Runs => "↑↓ select  ·  f=cycle filter (all/live/terminal)",
            ActiveView::RunGrid => {
                "↑↓ select  ·  rows=tasks  ·  cols=recent attempts (newest left)"
            }
            ActiveView::Nerdy => "pool / tables / hooks (read-only deep-dive)",
            ActiveView::Chat => "c=compose  ↑↓=scroll  Enter=detail/claim  /=filter",
        };
        // yggdrasil-134: scope chip on the right of the help row makes
        // the current scope (repo / all) and the toggle key visible.
        let scope_chip = Span::styled(
            format!("  scope: {}  S=toggle  ?=help", self.scope.label()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        let hint_line = Line::from(vec![
            nav_span,
            Span::styled(
                pane_hint.to_string(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            scope_chip,
        ]);
        frame.render_widget(hint_line, chunks[1]);

        match self.active_view {
            ActiveView::Dashboard => self.dashboard.render(frame, chunks[2]),
            ActiveView::Dag => self.dag.render(frame, chunks[2]),
            ActiveView::Tasks => self.tasks.render(frame, chunks[2]),
            ActiveView::Logs => self.logs.render(frame, chunks[2]),
            ActiveView::Locks => self.locks.render(frame, chunks[2]),
            ActiveView::Runs => self.runs.render(frame, chunks[2]),
            ActiveView::RunGrid => self.run_grid.render(frame, chunks[2]),
            ActiveView::Nerdy => self.nerdy.render(frame, chunks[2]),
            ActiveView::Chat => self.chat.render(frame, chunks[2]),
        }

        // Render any pending toasts above the persistent status strip.
        if let Some(idx) = toast_idx {
            render_toasts(frame, chunks[idx], &self.toasts);
        }

        // Global status strip — two columns at wide widths (events left,
        // orchestration stats right); single-column at narrow widths so the
        // 34-col ops panel doesn't crowd out the event tail. yggdrasil-156.
        let strip_constraints: &[Constraint] = if narrow {
            &[Constraint::Min(0)]
        } else {
            &[Constraint::Min(40), Constraint::Length(34)]
        };
        let strip = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(strip_constraints)
            .split(chunks[strip_idx]);

        let event_lines: Vec<Line> = self
            .status_tail
            .iter()
            .map(|(ts, kind, detail)| {
                let (glyph, color) = event_glyph(kind);
                Line::from(vec![
                    Span::styled(ts.clone(), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::styled(format!("{kind:<18}"), Style::default().fg(color)),
                    Span::raw(" "),
                    Span::styled(detail.clone(), Style::default().fg(Color::Gray)),
                ])
            })
            .collect();
        let events_panel = ratatui::widgets::Paragraph::new(event_lines).block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::TOP)
                .title(" events "),
        );
        frame.render_widget(events_panel, strip[0]);

        let s = &self.ops_stats;
        // yggdrasil-152: REVERSED for two paint passes when the cell value
        // moved between refreshes. Helper closure so each cell adds the
        // modifier conditionally without scattering the same style match.
        let with_flash = |base: Style, flashing: bool| -> Style {
            if flashing {
                base.add_modifier(Modifier::REVERSED)
            } else {
                base
            }
        };
        let stuck_line = if s.agents_stuck > 0 {
            Line::from(vec![
                Span::styled(
                    "  ⚠ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{} stuck", s.agents_stuck),
                    with_flash(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                        self.flash.is_flashing_stuck(),
                    ),
                ),
            ])
        } else {
            Line::from(vec![Span::styled(
                format!("  db {}ms", s.db_ms),
                Style::default().fg(Color::DarkGray),
            )])
        };

        // yggdrasil-177: pool saturation + event throughput on a fourth
        // line so "is anything broken right now" resolves without leaving
        // the dashboard. Color-flips when pool gets hot (≥75% saturated).
        let pool_saturated = s.pool_max > 0 && (s.pool_used as f64 / s.pool_max as f64) >= 0.75;
        let pool_color = if pool_saturated {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        let db_line = Line::from(vec![
            Span::styled(
                format!("  pool {}/{} ", s.pool_used, s.pool_max),
                Style::default().fg(pool_color),
            ),
            Span::styled(
                format!("· {}/m", s.events_per_min),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        // yggdrasil-150: 10-glyph sparkline of recent alive-count history,
        // appended to the "● live" row so liveness trend is one glance.
        let alive_spark = self.alive_history.glyphs(10);
        let mut stats_lines = vec![
            Line::from(vec![
                Span::styled("  ● ", Style::default().fg(Color::Green)),
                Span::styled(
                    format!("{} live", s.agents_alive),
                    with_flash(
                        Style::default().add_modifier(Modifier::BOLD),
                        self.flash.is_flashing_alive(),
                    ),
                ),
                Span::styled(format!(" {alive_spark}"), Style::default().fg(Color::Green)),
                Span::styled(
                    format!(" / {} sessions", s.live_sessions),
                    with_flash(
                        Style::default().fg(Color::DarkGray),
                        self.flash.is_flashing_sessions(),
                    ),
                ),
            ]),
            Line::from(vec![
                Span::styled("  ▶ ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{} tasks running", s.tasks_running),
                    with_flash(
                        Style::default().add_modifier(Modifier::BOLD),
                        self.flash.is_flashing_tasks(),
                    ),
                ),
            ]),
            stuck_line,
            db_line,
        ];
        // yggdrasil-148: append the burn-rate line when cost displays
        // are not hidden. Stays in the orchestration panel so the chrome
        // doesn't grow another row.
        if !cost_hidden() {
            stats_lines.push(Line::from(vec![
                Span::styled("  $ ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    format_tokens_per_min(s.tokens_per_min),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · today ${:.2}", s.cost_today_usd),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        if !narrow {
            let stats_panel = ratatui::widgets::Paragraph::new(stats_lines).block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::TOP)
                    .title(" orchestration "),
            );
            frame.render_widget(stats_panel, strip[1]);
        }
        // Decay the flash counters after the paint that consumed them. One
        // flash mark = N full paints, regardless of refresh cadence.
        self.flash.tick_paint();
        // yggdrasil-166: decay the cascade-ripple queue alongside the flash
        // counter so signature-event animations expire on the same paint
        // budget as cell flashes.
        self.ripples.tick_paint();

        // yggdrasil-151: detail overlay paints last so it sits on top of
        // everything else. Esc handler in the key dispatcher closes it.
        if let Some(overlay) = &self.detail_overlay {
            render_detail_overlay(frame, area, overlay);
        }
        // yggdrasil-132: help overlay paints over everything (including
        // detail overlay) when toggled, so `?` is the universal "what
        // can I press?" answer regardless of state.
        if self.help.open {
            super::help::render(area, frame.buffer_mut(), self.active_view_label());
        }
    }

    /// Stable, human-readable label for the active view; used by the
    /// help overlay to look up pane-specific keybindings.
    fn active_view_label(&self) -> &'static str {
        match self.active_view {
            ActiveView::Dashboard => "Dashboard",
            ActiveView::Dag => "Dag",
            ActiveView::Tasks => "Tasks",
            ActiveView::Logs => "Logs",
            ActiveView::Locks => "Locks",
            ActiveView::Runs => "Runs",
            ActiveView::RunGrid => "RunGrid",
            ActiveView::Nerdy => "Nerdy",
            ActiveView::Chat => "Chat",
        }
    }
}

/// Width threshold below which the global TUI chrome collapses to a
/// single-column compact form. Picked empirically: 100 cols is the
/// narrowest the wide tab row + 34-col ops panel render without overlap.
pub const NARROW_TERMINAL_THRESHOLD: u16 = 100;

/// Render the current toast queue stacked oldest-on-top into `area`.
/// Caller is responsible for sizing `area` to `app.visible_toast_rows()`
/// rows. Newer toasts get the bottom row (closest to the status strip)
/// so the most recent message sits where eyes land first.
fn render_toasts(frame: &mut Frame, area: Rect, toasts: &[Toast]) {
    let visible: Vec<&Toast> = toasts.iter().rev().take(TOAST_MAX_VISIBLE).collect();
    let lines: Vec<Line> = visible
        .iter()
        .rev()
        .map(|t| {
            let (glyph, color) = match t.severity {
                ToastSeverity::Info => ("·", Color::Gray),
                ToastSeverity::Success => ("✓", Color::Green),
                ToastSeverity::Warn => ("⚠", Color::Yellow),
                ToastSeverity::Error => ("✗", Color::Red),
            };
            Line::from(vec![
                Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                Span::styled(t.msg.clone(), Style::default().fg(color)),
            ])
        })
        .collect();
    let para = ratatui::widgets::Paragraph::new(lines);
    frame.render_widget(para, area);
}

/// Carve a centered rectangle out of `outer`, capping each dimension at
/// the supplied percentages. 80%/80% leaves a margin around the overlay
/// so the underlying pane is still partially visible.
fn centered_rect(outer: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let h = outer.height.saturating_mul(pct_y) / 100;
    let w = outer.width.saturating_mul(pct_x) / 100;
    let x = outer.x + outer.width.saturating_sub(w) / 2;
    let y = outer.y + outer.height.saturating_sub(h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Render the detail overlay over the entire frame. Uses the `Clear`
/// widget so the underlying pane's pixels are wiped where the overlay
/// sits, then paints a bordered Paragraph on top.
fn render_detail_overlay(frame: &mut Frame, area: Rect, overlay: &DetailOverlay) {
    let rect = centered_rect(area, 80, 80);
    frame.render_widget(ratatui::widgets::Clear, rect);
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .title(format!(" {} ", overlay.title))
        .title_bottom(Line::from(vec![Span::styled(
            "  Esc=close  ",
            Style::default().fg(Color::DarkGray),
        )]));
    let para = ratatui::widgets::Paragraph::new(overlay.body.clone())
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(para, rect);
}

/// Glyph + color for an event kind — mirrors src/cli/logs_cmd.rs::kind_style.
fn event_glyph(kind: &str) -> (&'static str, Color) {
    match kind {
        "node_written" => ("●", Color::Green),
        "lock_acquired" => ("⚿", Color::Yellow),
        "lock_released" => ("○", Color::DarkGray),
        "digest_written" => ("◈", Color::Cyan),
        "similarity_hit" => ("≈", Color::Blue),
        "correction_detected" => ("✗", Color::Red),
        "hook_fired" => ("▸", Color::Yellow),
        "embedding_call" => ("⚡", Color::Cyan),
        "task_created" => ("✚", Color::Green),
        "task_status_changed" => ("◆", Color::Yellow),
        "remembered" => ("♦", Color::Blue),
        "embedding_cache_hit" => ("⚡", Color::Green),
        "classifier_decision" => ("⚖", Color::Cyan),
        "scoring_decision" => ("·", Color::Gray),
        "redaction_applied" => ("✂", Color::Red),
        "hit_referenced" => ("✓", Color::Green),
        "agent_state_changed" => ("↪", Color::Blue),
        _ => ("·", Color::Gray),
    }
}

/// Boot reconciliation. Cross-check the workers table against live tmux
/// windows; abandon any row whose window is gone. Called once at TUI
/// startup and also cheap to call from anywhere else (idempotent).
pub async fn reconcile_workers(pool: &DbPool) -> Result<(), anyhow::Error> {
    use crate::models::worker::{WorkerRepo, WorkerState};
    use std::collections::{HashMap, HashSet};
    let workers = WorkerRepo::new(pool).list_live().await.unwrap_or_default();
    if workers.is_empty() {
        return Ok(());
    }

    let mut by_session: HashMap<String, HashSet<String>> = HashMap::new();
    for w in &workers {
        by_session.entry(w.tmux_session.clone()).or_default();
    }
    for session in by_session.keys().cloned().collect::<Vec<_>>() {
        let out = std::process::Command::new("tmux")
            .args(["list-windows", "-t", &session, "-F", "#{window_name}"])
            .output();
        let set = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .collect(),
            _ => HashSet::new(), // session missing → treat all as gone
        };
        by_session.insert(session, set);
    }

    let repo = WorkerRepo::new(pool);
    let mut n = 0;
    for w in workers {
        let live = by_session
            .get(&w.tmux_session)
            .map(|set| set.contains(&w.tmux_window))
            .unwrap_or(false);
        if !live {
            let _ = repo
                .set_state(
                    &w.worker_id,
                    WorkerState::Abandoned,
                    Some("reconciled at TUI start — window absent"),
                )
                .await;
            n += 1;
        }
    }
    if n > 0 {
        tracing::info!(reconciled = n, "worker boot reconciliation");
    }
    Ok(())
}

/// Stable hash for "this run_terminal event came from this task at this
/// time." The cascade-ripple queue dedupes by this key so a second tick
/// that pulls the same row doesn't re-trigger the ripple.
pub fn ripple_key(when: &chrono::DateTime<chrono::Utc>, task_ref: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    when.timestamp_nanos_opt().unwrap_or(0).hash(&mut hasher);
    task_ref.hash(&mut hasher);
    hasher.finish()
}

fn short_status_detail(p: &serde_json::Value) -> String {
    // One-line detail for the bottom status strip. Best-effort per kind.
    if let Some(score) = p
        .get("total_score")
        .or_else(|| p.get("similarity"))
        .and_then(|v| v.as_f64())
    {
        let src = p
            .get("source_agent")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let snip = p.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
        let s = if snip.chars().count() > 40 {
            snip.chars().take(40).collect::<String>() + "…"
        } else {
            snip.to_string()
        };
        return format!("score={score:.2} from {src}  {s}");
    }
    if let Some(t) = p.get("turns").and_then(|v| v.as_i64()) {
        return format!("{t} turns");
    }
    if let Some(r) = p.get("ref").and_then(|v| v.as_str()) {
        if let Some(to) = p.get("to").and_then(|v| v.as_str()) {
            return format!("{r} → {to}");
        }
        return r.to_string();
    }
    if let Some(snip) = p.get("snippet").and_then(|v| v.as_str()) {
        let s = if snip.chars().count() > 60 {
            snip.chars().take(60).collect::<String>() + "…"
        } else {
            snip.to_string()
        };
        return s;
    }
    String::new()
}

/// Run the TUI event loop.
pub async fn run(pool: &DbPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let agent_name = std::env::var("YGG_AGENT_NAME").unwrap_or_else(|_| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "ygg".to_string())
    });

    // Boot reconciliation (yggdrasil-53). Any worker row marked live
    // whose tmux window is absent gets flipped to abandoned so the
    // dashboard reflects reality after a machine restart.
    if let Err(e) = reconcile_workers(pool).await {
        tracing::warn!(error = %e, "worker reconciliation on TUI start failed");
    }

    let mut app = App::new(agent_name);

    // Decouple refresh from input. Previously every keypress triggered
    // a full DB refresh cascade (~hundreds of ms) before the
    // next draw — arrow keys felt laggy. Now we refresh on a timer
    // (every 2s) while key polling at a short interval stays snappy.
    // Targeted per-action refreshes (e.g. after 'r' runs) still fire
    // from handle_key directly.
    use std::time::Instant;
    let refresh_interval = std::time::Duration::from_secs(2);
    let poll_interval = std::time::Duration::from_millis(50);
    let mut last_refresh: Option<Instant> = None;

    loop {
        // Draw every tick so input stays snappy and each view's own
        // "loading" state is visible until its first refresh lands.
        terminal.draw(|frame| app.draw(frame))?;

        // Refresh on a coarser timer — every keypress refreshing the DB
        // made arrow keys laggy. Targeted per-action refreshes still fire
        // from handle_key directly.
        let need_refresh = last_refresh
            .map(|t| t.elapsed() >= refresh_interval)
            .unwrap_or(true);

        if need_refresh {
            app.dashboard.refresh(pool).await?;
            app.refresh_status_tail(pool).await;
            match app.active_view {
                ActiveView::Dag => {
                    app.dag.refresh(pool).await?;
                }
                ActiveView::Tasks => {
                    app.tasks.refresh(pool).await?;
                }
                ActiveView::Logs => {
                    app.logs.refresh(pool).await?;
                }
                ActiveView::Locks => {
                    app.locks.refresh(pool, config.lock_ttl_secs).await?;
                }
                ActiveView::Runs => {
                    app.runs.refresh(pool).await?;
                }
                ActiveView::RunGrid => {
                    app.run_grid.refresh(pool).await?;
                }
                ActiveView::Nerdy => {
                    app.nerdy.update_ops(&app.ops_stats);
                    app.nerdy.refresh(pool).await?;
                }
                ActiveView::Chat => {
                    app.chat.refresh(pool).await?;
                    app.chat.mark_seen();
                }
                _ => {}
            }
            last_refresh = Some(Instant::now());
        }

        if event::poll(poll_interval)?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(pool, key.code, key.modifiers).await;
        }

        if app.should_quit {
            break;
        }
    }

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Honor a deferred tmux attach from Workers-panel Enter. If we're
    // already inside tmux, switch-client; otherwise, exec attach so
    // the ygg-tui process is replaced by tmux.
    if let Some((session, window)) = app.attach_pending {
        let target = format!("{session}:{window}");
        if std::env::var("TMUX").is_ok() {
            let _ = std::process::Command::new("tmux")
                .args(["switch-client", "-t", &target])
                .status();
        } else {
            // exec replaces the current process — ratatui is done, so
            // hand the terminal over to tmux cleanly.
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new("tmux")
                .args([
                    "attach",
                    "-t",
                    &session,
                    ";",
                    "select-window",
                    "-t",
                    &target,
                ])
                .exec();
            // exec returns only on failure; print and continue.
            eprintln!("tmux attach failed: {err}");
        }
    }
    Ok(())
}
