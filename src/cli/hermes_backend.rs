//! Hermes scheduler execution backend (Part B).
//!
//! The default task-run backend is `tmux`: the scheduler spawns a local Claude
//! Code worker in a tmux window with its own git worktree. A task can instead
//! opt into `hermes` (via its `tasks.backend` column, e.g. `hermes:feature-dev`),
//! in which case the scheduler *dispatches* the run to a sandboxed agent behind
//! the hermes-gateway (`POST /v1/tasks`) and later *reconciles* the run's
//! terminal state by polling the gateway (`GET /v1/tasks/{id}`) — no tmux, no
//! worktree.
//!
//! This module owns both seams:
//!   * [`dispatch`] — called from `scheduler::dispatch_ready` in place of
//!     `spawn::execute` for hermes-backed runs.
//!   * [`reconcile_running`] — called each `scheduler::tick` to advance running
//!     hermes runs, enforce their deadlines (with a remote cancel), and write
//!     output/error so `finalize_terminal_runs` closes the parent task exactly
//!     as it does for tmux runs.
//!
//! The whole remote path is gated on `HERMES_GATEWAY_URL`: if a hermes run
//! appears while the gateway is unconfigured, the run is marked crashed with a
//! clear reason rather than panicking.

use crate::blob::BlobStore;
use crate::config::AppConfig;
use crate::db::DbPool;
use crate::hermes::{HermesClient, TaskSubmit};
use crate::models::task_run::{RunReason, RunState, TaskRunRepo};

/// The gateway agent a bare `hermes` selector routes to.
/// `YGG_HERMES_DEFAULT_AGENT`, else `feature-dev`.
pub fn default_agent() -> String {
    std::env::var("YGG_HERMES_DEFAULT_AGENT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "feature-dev".to_string())
}

/// Poll interval (seconds) for reconciling running hermes runs.
/// `YGG_HERMES_POLL_SECS`, default 5.
fn poll_secs() -> i64 {
    std::env::var("YGG_HERMES_POLL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(5)
}

/// Parse a task's `backend` selector. Returns the gateway agent when the task
/// is hermes-backed, or `None` for the default (tmux) backend. Accepts:
///   * `None` / `""` / `"tmux"` → tmux (returns `None`)
///   * `"hermes"`               → the [`default_agent`]
///   * `"hermes:<agent>"`       → `<agent>`
pub fn hermes_agent(backend: Option<&str>) -> Option<String> {
    let b = backend.map(str::trim).unwrap_or("");
    if b.is_empty() || b == "tmux" {
        return None;
    }
    if b == "hermes" {
        return Some(default_agent());
    }
    if let Some(agent) = b.strip_prefix("hermes:") {
        let agent = agent.trim();
        return Some(if agent.is_empty() {
            default_agent()
        } else {
            agent.to_string()
        });
    }
    None
}

/// Dispatch an already-claimed (`running`) run to a gateway agent. Submits the
/// prompt as a task, records `backend='hermes'` + `remote_task_id`, and binds
/// the run to its parent task. Returns the remote task id on success.
///
/// On any error (gateway unconfigured, submit rejected) the caller is expected
/// to mark the run crashed — this function performs no state rollback so the
/// failure stays observable.
pub async fn dispatch(
    pool: &DbPool,
    cfg: &AppConfig,
    run_id: &str,
    task_id: &str,
    agent: &str,
    prompt: &str,
) -> anyhow::Result<String> {
    let client = HermesClient::from_config(cfg)?;
    let rec = client.task_submit(&TaskSubmit::new(agent, prompt)).await?;

    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"UPDATE task_runs
           SET backend = 'hermes',
               remote_task_id = $2,
               state = 'running',
               updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
           WHERE run_id = $1"#,
    )
    .bind(run_id)
    .bind(&rec.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE tasks SET current_attempt_id = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE task_id = $1",
    )
    .bind(task_id)
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rec.id)
}

/// Reconcile running hermes runs against the gateway. Returns the number of
/// runs transitioned to a terminal state this pass. Cheap when there are no
/// hermes runs (a single indexed SELECT). Safe to call every tick — polling is
/// throttled per-run to `YGG_HERMES_POLL_SECS` via `updated_at`.
pub async fn reconcile_running(pool: &DbPool) -> anyhow::Result<i64> {
    let cfg = AppConfig::from_env().map_err(|e| anyhow::anyhow!("config: {e}"))?;

    // Gate on gateway configuration. If a hermes run is live but the gateway is
    // unconfigured, fail it loudly rather than leaving it stuck forever.
    let client = match HermesClient::from_config(&cfg) {
        Ok(c) => c,
        Err(e) => return crash_all_running(pool, &e.to_string()).await,
    };

    let poll = poll_secs();
    // Due = throttle window elapsed, OR the deadline has passed (enforce
    // promptly regardless of throttle). `reap_expired_heartbeats` and
    // `enforce_deadlines` skip hermes runs, so this is the only path that ends
    // them.
    let due: Vec<(String, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        r#"SELECT run_id, remote_task_id, deadline_at
             FROM task_runs
            WHERE backend = 'hermes'
              AND state = 'running'
              AND remote_task_id IS NOT NULL
              AND (
                    updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' seconds')
                    OR (deadline_at IS NOT NULL AND deadline_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
                  )
            LIMIT 100"#,
    )
    .bind(poll)
    .fetch_all(pool)
    .await?;

    if due.is_empty() {
        return Ok(0);
    }

    let store = BlobStore::open_default().map_err(|e| anyhow::anyhow!("blob store: {e}"))?;
    let runs = TaskRunRepo::new(pool);
    let now = chrono::Utc::now();
    let mut terminal = 0i64;

    for (run_id, remote_id, deadline_at) in due {
        // Deadline enforcement: cancel remotely, then mark cancelled/timeout.
        if let Some(dl) = deadline_at
            && dl < now
        {
            if let Err(e) = client.task_cancel(&remote_id).await {
                tracing::warn!(run_id = %run_id, remote = %remote_id, error = %e,
                    "hermes deadline cancel failed (marking cancelled anyway)");
            }
            runs.set_state(&run_id, RunState::Cancelled, RunReason::Timeout)
                .await?;
            terminal += 1;
            continue;
        }

        match client.task_get(&remote_id).await {
            Ok(rec) => match rec.state.as_str() {
                "succeeded" => {
                    let payload = serde_json::json!({
                        "backend": "hermes",
                        "remote_task_id": remote_id,
                        "content": rec
                            .result
                            .as_ref()
                            .and_then(|r| r.get("content"))
                            .cloned(),
                        "finish_reason": rec
                            .result
                            .as_ref()
                            .and_then(|r| r.get("finish_reason"))
                            .cloned(),
                        "usage": rec.result.as_ref().and_then(|r| r.get("usage")).cloned(),
                    });
                    let _ = runs.write_output(&run_id, &payload, &store).await;
                    runs.set_state(&run_id, RunState::Succeeded, RunReason::Ok)
                        .await?;
                    terminal += 1;
                }
                "failed" => {
                    let payload = rec.error.clone().unwrap_or_else(|| {
                        serde_json::json!({ "message": "hermes task failed", "kind": "agent_error" })
                    });
                    let _ = runs.write_error(&run_id, &payload, &store).await;
                    runs.set_state(&run_id, RunState::Failed, RunReason::AgentError)
                        .await?;
                    terminal += 1;
                }
                "cancelled" => {
                    runs.set_state(&run_id, RunState::Cancelled, RunReason::UserCancelled)
                        .await?;
                    terminal += 1;
                }
                "expired" => {
                    let payload = serde_json::json!({
                        "message": "hermes task expired (deadline reached on the gateway)",
                        "kind": "timeout",
                    });
                    let _ = runs.write_error(&run_id, &payload, &store).await;
                    runs.set_state(&run_id, RunState::Crashed, RunReason::Timeout)
                        .await?;
                    terminal += 1;
                }
                // pending / running / anything non-terminal: leave it, but bump
                // updated_at so we honor the poll throttle before the next GET.
                _ => touch(pool, &run_id).await?,
            },
            Err(e) => {
                // Transient (network) or a vanished remote task. Back off via
                // the throttle; a persistent failure will eventually hit the
                // run's own deadline and be cancelled above.
                tracing::warn!(run_id = %run_id, remote = %remote_id, error = %e,
                    "hermes task poll failed");
                touch(pool, &run_id).await?;
            }
        }
    }

    Ok(terminal)
}

/// Bump `updated_at` so the poll throttle waits `YGG_HERMES_POLL_SECS` before
/// re-polling this run.
async fn touch(pool: &DbPool, run_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE task_runs SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE run_id = $1",
    )
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Gateway unconfigured but hermes runs are live: crash them with a clear
/// reason instead of leaving them wedged.
async fn crash_all_running(pool: &DbPool, why: &str) -> anyhow::Result<i64> {
    let run_ids: Vec<String> = sqlx::query_scalar(
        "SELECT run_id FROM task_runs WHERE backend = 'hermes' AND state = 'running'",
    )
    .fetch_all(pool)
    .await?;
    if run_ids.is_empty() {
        return Ok(0);
    }
    let runs = TaskRunRepo::new(pool);
    let store = BlobStore::open_default().ok();
    for run_id in &run_ids {
        if let Some(store) = &store {
            let payload = serde_json::json!({
                "message": format!("hermes backend unavailable: {why}"),
                "kind": "agent_error",
            });
            let _ = runs.write_error(run_id, &payload, store).await;
        }
        runs.set_state(run_id, RunState::Crashed, RunReason::AgentError)
            .await?;
    }
    Ok(run_ids.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_agent_selector_parsing() {
        assert_eq!(hermes_agent(None), None);
        assert_eq!(hermes_agent(Some("")), None);
        assert_eq!(hermes_agent(Some("tmux")), None);
        assert_eq!(
            hermes_agent(Some("hermes:debugger")).as_deref(),
            Some("debugger")
        );
        assert_eq!(
            hermes_agent(Some("hermes: security ")).as_deref(),
            Some("security")
        );
        // bare hermes → default agent
        assert!(hermes_agent(Some("hermes")).is_some());
        // unknown scheme → not hermes
        assert_eq!(hermes_agent(Some("k8s:foo")), None);
    }
}
