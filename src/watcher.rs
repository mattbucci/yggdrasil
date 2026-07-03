use crate::db::DbPool;
use std::process::Command;
use std::time::Duration;

use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::event::{EventKind, EventRepo};
use crate::models::worker::{Worker, WorkerRepo, WorkerState};
use crate::tmux::TmuxManager;

/// Background watcher daemon.
/// Periodically: reap expired locks, flag stale agents, cleanup.
pub struct Watcher {
    pool: DbPool,
    config: AppConfig,
}

impl Watcher {
    pub fn new(pool: DbPool, config: AppConfig) -> Self {
        Self { pool, config }
    }

    /// Main loop — runs until SIGTERM/SIGINT.
    pub async fn run(&self) -> Result<(), anyhow::Error> {
        let interval = Duration::from_secs(self.config.watcher_interval_secs);
        tracing::info!(
            interval_secs = self.config.watcher_interval_secs,
            "watcher started"
        );

        let mut tick = tokio::time::interval(interval);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = self.tick().await {
                        tracing::error!(error = %e, "watcher tick failed");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("watcher shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn tick(&self) -> Result<(), anyhow::Error> {
        let reaped = self.reap_expired_locks().await?;
        let stale = self.flag_stale_agents().await?;
        let worker_updates = self.observe_workers().await.unwrap_or(0);
        let delivery_updates = self.check_delivery().await.unwrap_or(0);
        let cleaned = self.cleanup_delivered().await.unwrap_or(0);
        let zombies = self.cleanup_zombie_agents().await.unwrap_or(0);

        if reaped > 0
            || stale > 0
            || worker_updates > 0
            || delivery_updates > 0
            || cleaned > 0
            || zombies > 0
        {
            tracing::info!(
                reaped_locks = reaped,
                stale_agents = stale,
                worker_updates = worker_updates,
                delivery_updates = delivery_updates,
                cleaned_workers = cleaned,
                zombie_agents = zombies,
                "watcher tick"
            );
        }

        Ok(())
    }

    /// For terminated workers whose delivery status we haven't checked
    /// recently, run git/gh locally to find out if the branch is pushed,
    /// merged, and whether a PR is open. Cheap — only runs on completed/
    /// failed rows and throttled by delivery_checked_at.
    async fn check_delivery(&self) -> Result<u64, anyhow::Error> {
        let workers: Vec<_> = sqlx::query_as::<_, crate::models::worker::Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason,
                      branch_pushed, branch_merged, pr_url, delivery_checked_at, intent
                 FROM workers
                WHERE state IN ('completed', 'failed')
                  AND (branch_pushed = false OR branch_merged = false)
                  AND (delivery_checked_at IS NULL
                       OR delivery_checked_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-60 seconds'))
                ORDER BY ended_at DESC NULLS LAST
                LIMIT 20"#,
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let repo = WorkerRepo::new(&self.pool);
        let mut n = 0u64;
        for w in workers {
            // Branch name follows the plan_cmd scheme: ygg/<prefix>-<seq>
            let branch = derive_branch(&w.tmux_window);
            let (pushed, merged, pr) = inspect_delivery(&w.worktree_path, branch.as_deref());
            let _ = repo
                .set_delivery(&w.worker_id, pushed, merged, pr.as_deref())
                .await;
            if pushed != w.branch_pushed || merged != w.branch_merged {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Observer loop for click-to-do workers. Lists tmux windows in the
    /// `yggdrasil` session, cross-checks against live worker rows, and:
    ///   - touches last_seen_at for matches
    ///   - captures the pane and scans for prompt markers → needs_attention
    ///   - marks rows whose window is gone → abandoned
    async fn observe_workers(&self) -> Result<u64, anyhow::Error> {
        let workers = WorkerRepo::new(&self.pool)
            .list_live()
            .await
            .unwrap_or_default();
        if workers.is_empty() {
            return Ok(0);
        }

        // Group by tmux_session so we make one list-windows call per.
        use std::collections::{HashMap, HashSet};
        let mut by_session: HashMap<String, Vec<Worker>> = HashMap::new();
        for w in workers {
            by_session
                .entry(w.tmux_session.clone())
                .or_default()
                .push(w);
        }

        let repo = WorkerRepo::new(&self.pool);
        let mut changes = 0u64;

        for (session, ws) in by_session {
            let windows: HashSet<String> = list_tmux_windows(&session).into_iter().collect();
            for w in ws {
                if !windows.contains(&w.tmux_window) {
                    // Window vanished — machine restart, manual kill, or
                    // claude exited and the shell closed.
                    let _ = repo
                        .set_state(
                            &w.worker_id,
                            WorkerState::Abandoned,
                            Some("tmux window absent on observer tick"),
                        )
                        .await;
                    changes += 1;
                    continue;
                }

                // Touch last_seen_at; then inspect the pane for prompts.
                let _ = repo.touch(&w.worker_id).await;
                let pane = capture_pane(&session, &w.tmux_window).unwrap_or_default();
                let next = classify_pane(&pane);
                if next != w.state {
                    let _ = repo.set_state(&w.worker_id, next, None).await;
                    changes += 1;
                }
                let intent = extract_intent(&pane, next);
                if intent.as_deref() != w.intent.as_deref() {
                    let _ = repo.set_intent(&w.worker_id, intent.as_deref()).await;
                }
            }
        }
        Ok(changes)
    }

    /// Remove all expired locks.
    async fn reap_expired_locks(&self) -> Result<u64, anyhow::Error> {
        let lock_mgr =
            LockManager::new(&self.pool, self.config.lock_ttl_secs, crate::db::user_id());
        let count = lock_mgr.reap_expired().await?;
        Ok(count)
    }

    /// Surface agents stuck in an active state with no recent updates as
    /// `agent_stale_warning` events. Observation-only: the watcher must not
    /// transition agent or run state itself. The scheduler is the single
    /// writer of `task_runs.state = 'crashed'` via the heartbeat-reap path
    /// (yggdrasil-140), and any agent-state recovery follows from the
    /// scheduler's run terminal events, not from a parallel watcher pass.
    /// Previous versions force-transitioned the agent to Idle here, which
    /// risked split-brain with the scheduler's reap of the same agent's
    /// in-flight run.
    pub async fn flag_stale_agents(&self) -> Result<u64, anyhow::Error> {
        let stale_threshold = (self.config.lock_ttl_secs * 2) as i64;

        let stale_agents: Vec<_> = sqlx::query_as::<_, crate::models::agent::AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE archived_at IS NULL
              AND current_state IN ('executing', 'waiting_tool', 'planning', 'context_flush')
              AND updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' seconds')
            "#,
        )
        .bind(stale_threshold as f64)
        .fetch_all(&self.pool)
        .await?;

        let events = EventRepo::new(&self.pool);
        let mut count = 0u64;
        for agent in stale_agents {
            tracing::warn!(
                agent = %agent.agent_name,
                last_update = %agent.updated_at,
                "agent_stale_warning"
            );
            let payload = serde_json::json!({
                "agent_id": agent.agent_id,
                "current_state": agent.current_state,
                "last_update": agent.updated_at,
                "stale_threshold_secs": stale_threshold,
            });
            if let Err(e) = events
                .emit(
                    EventKind::AgentStaleWarning,
                    &agent.agent_name,
                    Some(agent.agent_id),
                    payload,
                )
                .await
            {
                tracing::warn!(error = %e, "failed to emit agent_stale_warning event");
            }
            count += 1;
        }
        Ok(count)
    }

    /// Reap zombie spawn-agents — agents whose tmux window is gone but
    /// whose `agents` row is still in a non-terminal state. The window
    /// disappearing is dispositive: tmux closes the window when its last
    /// pane's process exits, so if it's gone the Claude harness exited
    /// without (or before) the Stop hook could finalize state.
    ///
    /// We only touch agents that have a worktree under `.ygg/worktrees/` —
    /// that's our marker for "spawned via `ygg spawn`." Agents launched
    /// from an interactive shell (no worktree) are managed by their owner
    /// and don't get reaped here.
    async fn cleanup_zombie_agents(&self) -> Result<u64, anyhow::Error> {
        // Stale threshold: 4× the lock TTL. Long enough that an idle but
        // alive harness doesn't get reaped during a slow turn; short enough
        // that genuinely-dead agents clear within minutes.
        let min_idle_secs = (self.config.lock_ttl_secs as i64) * 4;

        let agent_repo = crate::models::agent::AgentRepo::new(&self.pool, crate::db::user_id());
        let candidates = agent_repo
            .list_reap_candidates(min_idle_secs)
            .await
            .unwrap_or_default();
        if candidates.is_empty() {
            return Ok(0);
        }

        // Resolve the repo root so worktree paths are absolute regardless of
        // the watcher's CWD.
        let repo_root = match repo_toplevel() {
            Some(r) => r,
            None => {
                tracing::warn!("cleanup_zombie_agents: not inside a git repo, skipping");
                return Ok(0);
            }
        };

        // Snapshot live windows once per tick. Empty when tmux isn't
        // running or the `ygg` session doesn't exist — both mean every
        // candidate's window is gone.
        let live_windows = list_tmux_windows("ygg");
        let live: std::collections::HashSet<&str> =
            live_windows.iter().map(String::as_str).collect();

        let lock_mgr =
            LockManager::new(&self.pool, self.config.lock_ttl_secs, crate::db::user_id());
        let session_repo = crate::models::session::SessionRepo::new(&self.pool);

        let mut n = 0u64;
        for a in candidates {
            let worktree = repo_root.join(".ygg/worktrees").join(&a.agent_name);
            // No worktree → not a `ygg spawn` agent → leave alone.
            if !worktree.exists() {
                continue;
            }
            // Window still alive → harness is running, not a zombie.
            if live.contains(a.agent_name.as_str()) {
                continue;
            }

            let _ = session_repo.end_all_for_agent(&a.agent_id).await;
            let _ = lock_mgr.release_all_for_agent(&a.agent_id).await;
            let _ = agent_repo
                .force_state(
                    &a.agent_id,
                    crate::models::agent::AgentState::Shutdown,
                    None,
                )
                .await;
            // Killing the window is mostly a no-op (it's already gone) but
            // covers the rare case where tmux still has a stub window with
            // a dead pane after a panic.
            TmuxManager::kill_window_sync("ygg", &a.agent_name);
            // Checkpoint work-in-progress onto refs/ygg/recovery/<run_id> before
            // we destroy the worktree, so the retry can continue from it rather
            // than reset to the starting commit (yggdrasil-115).
            if let Some(run_id) = latest_run_for_agent(&self.pool, &a.agent_id).await
                && let Some(sha) = crate::worktree::checkpoint(&worktree, run_id.clone())
            {
                record_pre_recovery_commit(&self.pool, &run_id, &sha).await;
                tracing::info!(
                    agent = %a.agent_name,
                    run = %run_id,
                    sha = %sha,
                    "pre-recovery checkpoint"
                );
            }
            let worktree_str = worktree.to_string_lossy();
            remove_worktree(&worktree_str);
            tracing::info!(
                agent = %a.agent_name,
                prior_state = %a.current_state,
                "reaped zombie agent"
            );
            n += 1;
        }
        Ok(n)
    }

    /// Clean up workers that are terminal AND fully delivered (merged) or
    /// abandoned for >1h. Kills the tmux window and removes the worktree.
    async fn cleanup_delivered(&self) -> Result<u64, anyhow::Error> {
        let workers = WorkerRepo::new(&self.pool)
            .list_cleanable()
            .await
            .unwrap_or_default();
        let mut n = 0u64;
        for w in workers {
            TmuxManager::kill_window_sync(&w.tmux_session, &w.tmux_window);
            remove_worktree(&w.worktree_path);
            tracing::info!(
                worker = %w.worker_id,
                state = ?w.state,
                worktree = %w.worktree_path,
                "cleaned up delivered worker"
            );
            n += 1;
        }
        Ok(n)
    }
}

/// `tmux list-windows -t <session> -F '#{window_name}'` → Vec<name>.
/// Empty on any error (session missing, tmux absent) — the observer
/// treats that as "all workers abandoned," which is correct.
fn list_tmux_windows(session: &str) -> Vec<String> {
    let out = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn capture_pane(session: &str, window: &str) -> Option<String> {
    let target = format!("{session}:{window}");
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", &target, "-S", "-200"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Classify the last ~200 lines of the pane into a WorkerState. Looks
/// for Claude Code / Codex prompt markers first, then idle heuristics.
/// Window names are "<agent>·<prefix>-<seq>·<uniq>". The branch is
/// "ygg/<prefix>-<seq>" — slice out the middle segment.
fn derive_branch(window: &str) -> Option<String> {
    let parts: Vec<&str> = window.split('·').collect();
    if parts.len() >= 2 {
        Some(format!("ygg/{}", parts[1]))
    } else {
        None
    }
}

/// Three-way delivery inspection. Any of these can fail silently —
/// git may not have a remote, gh may not be installed, branch may
/// have been deleted. Return conservative (false/false/None) on any
/// error so we don't mis-report.
fn inspect_delivery(worktree: &str, branch: Option<&str>) -> (bool, bool, Option<String>) {
    let Some(branch) = branch else {
        return (false, false, None);
    };
    let wt = std::path::Path::new(worktree);
    if !wt.exists() {
        return (false, false, None);
    }

    // Pushed: `git rev-parse <branch>@{upstream}` succeeds + `git log
    // origin/<branch>..<branch>` is empty.
    let upstream_ok = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args([
            "rev-parse",
            "--abbrev-ref",
            &format!("{branch}@{{upstream}}"),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let pushed = upstream_ok
        && Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(["rev-list", "--count", &format!("origin/{branch}..{branch}")])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim() == "0")
            .unwrap_or(false);

    // Merged: `git merge-base --is-ancestor <branch> origin/main` exit 0.
    let merged = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["merge-base", "--is-ancestor", branch, "origin/main"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // PR via gh (optional). One-line JSON, first match.
    let pr_url = Command::new("gh")
        .arg("-C")
        .arg(wt)
        .args([
            "pr", "list", "--head", branch, "--json", "url", "--limit", "1",
        ])
        .output()
        .ok()
        .and_then(|o| {
            if !o.status.success() {
                return None;
            }
            let s = String::from_utf8_lossy(&o.stdout);
            let v: serde_json::Value = serde_json::from_str(&s).ok()?;
            v.as_array()?
                .first()?
                .get("url")?
                .as_str()
                .map(String::from)
        });

    (pushed, merged, pr_url)
}

fn classify_pane(pane: &str) -> WorkerState {
    const ATTENTION: &[&str] = &[
        "Do you want to",
        "Bypass permissions",
        "trust this folder",
        "Quick safety check",
        "Do you trust",
        "Continue? [y/n]",
        "Select an option",
        // Plan-mode approval prompts
        "Would you like to proceed",
        "Yes, and bypass permissions",
        "Yes, manually approve",
        "Tell Claude what to change",
        "plan mode on",
    ];
    for m in ATTENTION {
        if pane.contains(m) {
            return WorkerState::NeedsAttention;
        }
    }

    let tail: String = pane.lines().rev().take(40).collect::<Vec<_>>().join("\n");
    if tail.contains("│ >") || tail.contains("Ctrl-C") || tail.contains("esc to interrupt") {
        return WorkerState::Running;
    }

    WorkerState::Idle
}

fn extract_intent(pane: &str, state: WorkerState) -> Option<String> {
    if state == WorkerState::NeedsAttention {
        if pane.contains("Would you like to proceed") || pane.contains("plan mode on") {
            return Some("awaiting plan approval".into());
        }
        return Some("awaiting user input".into());
    }

    let tail: Vec<&str> = pane.lines().rev().take(60).collect();
    let joined = tail.join("\n");

    // Tool call patterns from Claude Code status line
    if joined.contains("Compiling")
        || joined.contains("cargo build")
        || joined.contains("cargo check")
    {
        return Some("building".into());
    }
    if joined.contains("cargo test") || joined.contains("running test") {
        return Some("running tests".into());
    }
    if joined.contains("git push") {
        return Some("pushing".into());
    }
    if joined.contains("git commit") {
        return Some("committing".into());
    }

    // Claude Code tool indicators from the status bar
    for line in &tail {
        if line.contains("Read(") || line.contains("Reading") {
            return Some("reading files".into());
        }
        if line.contains("Edit(") || line.contains("Editing") {
            return Some("editing files".into());
        }
        if line.contains("Bash(") {
            return Some("running command".into());
        }
        if line.contains("Write(") || line.contains("Writing") {
            return Some("writing files".into());
        }
    }

    if state == WorkerState::Running {
        return Some("working".into());
    }

    // Check for shell prompt (agent exited, shell returned)
    if let Some(last) = tail.first() {
        let trimmed = last.trim();
        if trimmed.ends_with('$') || trimmed.ends_with('#') || trimmed.ends_with('%') {
            return Some("shell idle".into());
        }
    }

    None
}

fn repo_toplevel() -> Option<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if top.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(top))
}

/// The most recent run bound to an agent — the one whose worktree we're about
/// to reap. Returns None if the agent never bound to a run.
async fn latest_run_for_agent(pool: &DbPool, agent_id: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT run_id FROM task_runs WHERE agent_id = $1
          ORDER BY started_at DESC NULLS LAST, created_at DESC LIMIT 1",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

async fn record_pre_recovery_commit(pool: &DbPool, run_id: &str, sha: &str) {
    let _ = sqlx::query(
        "UPDATE task_runs SET pre_recovery_commit = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE run_id = $1",
    )
    .bind(run_id)
    .bind(sha)
    .execute(pool)
    .await;
}

fn remove_worktree(path: &str) {
    let wt = std::path::Path::new(path);
    if !wt.exists() {
        return;
    }
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", path])
        .output();
}
