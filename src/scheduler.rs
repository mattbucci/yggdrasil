//! `ygg scheduler` — single authoritative daemon that advances the task DAG.
//! ADR 0016 + docs/design/scheduler.md.
//!
//! Stage 1 MVP (yggdrasil-98): dispatch_ready + finalize_terminal_runs.
//! Heartbeat reap, deadline enforcement, retry, loop detection, poison, and
//! per-repo/epic budgets land per yggdrasil-99 / 100.
//!
//! The scheduler is the only writer of `task_runs.state` past `running`. The
//! Stop hook (yggdrasil-97) writes outcome fields and may transition the run
//! to terminal in manual mode; the scheduler treats already-terminal runs as
//! already-finalized and is therefore safe to run alongside manual workflows.

use crate::config::AppConfig;
use crate::db::DbPool;
use crate::models::event::{EventKind, EventRepo};
use crate::models::task_run::{RunReason, RunState, idempotency_key_for};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Scheduler singleton lock file name, created beside the SQLite database.
/// An exclusive flock() on it at startup gives us the singleton invariant;
/// concurrent attempts fail fast with a visible error. (This replaces the
/// Postgres advisory lock from the pre-SQLite era.)
const SCHEDULER_LOCK_FILE: &str = "ygg.scheduler.lock";

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub tick_interval: Duration,
    /// Global cap on concurrent running attempts.
    pub max_concurrent: i64,
    pub default_max_attempts: i32,
    pub default_heartbeat_ttl_s: i32,
    /// N consecutive identical fingerprints → poison. Set to a large number
    /// to disable; default 3.
    pub poison_threshold: i32,
    /// yggdrasil-109: when true, the scheduler treats tasks at any
    /// `approval_level` as approved without an explicit `approved_at`. Set
    /// via `YGG_AUTO_APPROVE=1|true|yes` for unattended overnight runs.
    pub auto_approve: bool,
}

impl SchedulerConfig {
    pub fn from_app(_app: &AppConfig) -> Self {
        let tick_ms: u64 = std::env::var("YGG_SCHEDULER_TICK_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2_000);
        let max_concurrent: i64 = std::env::var("YGG_SCHEDULER_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4);
        let poison_threshold: i32 = std::env::var("YGG_SCHEDULER_POISON_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        Self {
            tick_interval: Duration::from_millis(tick_ms),
            max_concurrent,
            default_max_attempts: 3,
            default_heartbeat_ttl_s: 300,
            poison_threshold,
            auto_approve: parse_bool_env("YGG_AUTO_APPROVE"),
        }
    }
}

/// Parse a 1/true/yes/on env flag, case-insensitive. Anything else (including
/// unset) → false. Centralized so we don't drift across env-flag readers.
fn parse_bool_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .as_deref()
        .map(str::trim)
        .is_some_and(|v| {
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
                || v.eq_ignore_ascii_case("on")
        })
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TickStats {
    pub finalized: i64,
    pub scheduled: i64,
    pub dispatched: i64,
    pub reaped: i64,
    pub deadlined: i64,
    pub retried: i64,
    pub poisoned: i64,
    /// Running hermes-backed runs advanced to a terminal state this tick by
    /// polling the gateway (Part B).
    pub hermes_reconciled: i64,
}

/// Run one scheduler tick. Public so `ygg scheduler tick` can call it
/// synchronously for testing without spinning up the daemon loop.
pub async fn tick(pool: &DbPool, cfg: &SchedulerConfig) -> Result<TickStats, anyhow::Error> {
    let mut stats = TickStats::default();
    stats.reaped = reap_expired_heartbeats(pool).await?;
    stats.deadlined = enforce_deadlines(pool).await?;
    // Reconcile running hermes-backed runs against the gateway BEFORE finalize,
    // so a remote task that just went terminal writes its output/error and the
    // same finalize_terminal_runs pass closes the parent task (Part B). Errors
    // here (gateway hiccup) are non-fatal — the run is retried next tick.
    stats.hermes_reconciled = match crate::cli::hermes_backend::reconcile_running(pool).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "hermes reconcile failed");
            0
        }
    };
    stats.poisoned = detect_loops(pool, cfg.poison_threshold).await?;
    // yggdrasil-112: retries MUST run before finalize. finalize closes the
    // parent task when the latest attempt is terminal; schedule_retries' guard
    // is `WHERE t.status <> 'closed'`, so reversing this order silently kills
    // retry. If a retry queues a fresh `ready` attempt, finalize then sees a
    // non-terminal latest attempt and skips the task (latest-terminal-run
    // GROUP BY in finalize_terminal_runs).
    stats.retried = schedule_retries(pool, cfg).await?;
    stats.finalized = finalize_terminal_runs(pool).await?;
    stats.scheduled = schedule_ready_tasks(pool, cfg).await?;
    stats.dispatched = dispatch_ready(pool, cfg).await?;
    Ok(stats)
}

/// Run the scheduler loop until shutdown. Holds the singleton file lock
/// (and the event-bus socket) for the lifetime of the process.
pub async fn run(pool: DbPool, cfg: SchedulerConfig) -> Result<(), anyhow::Error> {
    let _guard = acquire_advisory_lock(&pool).await?;
    tracing::info!(
        tick_ms = cfg.tick_interval.as_millis() as u64,
        max_concurrent = cfg.max_concurrent,
        "scheduler started"
    );
    emit_simple_event(
        &pool,
        EventKind::SchedulerTick,
        serde_json::json!({
            "started": true,
            "max_concurrent": cfg.max_concurrent,
        }),
    )
    .await
    .ok();

    // Event-bus wake-up (unix datagram socket): CLI one-shots and hooks fire
    // best-effort datagrams; a received event cuts the wait short so work
    // dispatches immediately. The periodic tick remains the delivery safety
    // net — a dropped datagram only costs one tick of latency.
    let bus = crate::bus::BusListener::bind()
        .map_err(|e| {
            tracing::debug!(error = %e, "event bus unavailable; running on timer only");
            e
        })
        .ok();

    loop {
        // Sleep + ctrl-c handling; the bus (when bound) can wake us early.
        let sleep = tokio::time::sleep(cfg.tick_interval);
        tokio::pin!(sleep);
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("scheduler stopping (ctrl-c)");
                break;
            }
            _ = &mut sleep => {}
            _event = async {
                match &bus {
                    Some(b) => b.recv().await,
                    None => std::future::pending().await,
                }
            } => {}
        }

        match tick(&pool, &cfg).await {
            Ok(stats) => {
                let total = stats.finalized
                    + stats.scheduled
                    + stats.dispatched
                    + stats.reaped
                    + stats.deadlined
                    + stats.retried
                    + stats.poisoned
                    + stats.hermes_reconciled;
                if total > 0 {
                    tracing::info!(
                        finalized = stats.finalized,
                        scheduled = stats.scheduled,
                        dispatched = stats.dispatched,
                        reaped = stats.reaped,
                        deadlined = stats.deadlined,
                        retried = stats.retried,
                        poisoned = stats.poisoned,
                        hermes_reconciled = stats.hermes_reconciled,
                        "scheduler tick"
                    );
                    emit_simple_event(&pool, EventKind::SchedulerTick, serde_json::json!(stats))
                        .await
                        .ok();
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "scheduler tick failed");
                emit_simple_event(
                    &pool,
                    EventKind::SchedulerError,
                    serde_json::json!({
                        "error": err.to_string(),
                    }),
                )
                .await
                .ok();
            }
        }
    }

    Ok(())
}

/// (1) Find tasks whose current run reached a terminal state but whose task
/// status is still in_progress, and close the task accordingly. Idempotent.
/// Stage 1 only: succeeded → closed; failed/crashed → closed with reason
/// "failed" (no retry yet — that's yggdrasil-99).
pub async fn finalize_terminal_runs(pool: &DbPool) -> Result<i64, anyhow::Error> {
    // Pull the latest terminal run per task where the task hasn't been
    // closed yet. Finalize is idempotent (a repeated tick would do the same
    // UPDATE harmlessly), and the file lock guarantees a single scheduler.
    // SQLite: MAX(attempt) with bare columns selects the row of the max —
    // the documented replacement for Postgres DISTINCT ON.
    let rows: Vec<(String, RunState, RunReason, i32, Option<String>)> = sqlx::query_as(
        r#"SELECT tr.task_id, tr.state, tr.reason, MAX(tr.attempt) AS attempt,
                  tr.output_commit_sha
             FROM task_runs tr
             JOIN tasks t ON t.task_id = tr.task_id
            WHERE tr.state IN ('succeeded', 'failed', 'crashed', 'cancelled', 'poison')
              AND t.status IN ('open', 'in_progress')
            GROUP BY tr.task_id
            LIMIT 100"#,
    )
    .fetch_all(pool)
    .await?;

    let mut closed = 0i64;
    for (task_id, state, reason, attempt, commit) in rows {
        let close_reason = match state {
            RunState::Succeeded => format!("run #{attempt} succeeded"),
            RunState::Failed => format!("run #{attempt} failed: {reason}"),
            RunState::Crashed => format!("run #{attempt} crashed: {reason}"),
            RunState::Cancelled => format!("run #{attempt} cancelled"),
            RunState::Poison => format!("run #{attempt} poisoned: {reason}"),
            _ => continue,
        };

        let mut tx = pool.begin().await?;
        sqlx::query(
            r#"UPDATE tasks
               SET status = 'closed',
                   closed_at = COALESCE(closed_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
                   close_reason = $2,
                   current_attempt_id = NULL,
                   result_blob_ref = COALESCE(result_blob_ref, $3),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
               WHERE task_id = $1"#,
        )
        .bind(&task_id)
        .bind(&close_reason)
        .bind(commit.as_deref())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO task_events (task_id, kind, payload) VALUES ($1, 'status_change', $2)",
        )
        .bind(&task_id)
        .bind(serde_json::json!({
            "to": "closed",
            "by": "scheduler",
            "run_state": state.as_str(),
            "run_reason": reason.as_str(),
            "attempt": attempt,
            "commit_sha": commit,
        }))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        closed += 1;
    }
    Ok(closed)
}

/// (2) For tasks marked `runnable=TRUE` with no current_attempt_id and all
/// blockers closed, insert a fresh `task_runs` row at state='ready'. This is
/// where the scheduler "owns" the queue: turning runnable tasks into ready
/// runs that dispatch_ready() will pick up next.
pub async fn schedule_ready_tasks(
    pool: &DbPool,
    cfg: &SchedulerConfig,
) -> Result<i64, anyhow::Error> {
    let rows: Vec<(String, Option<i32>, Option<i64>)> = sqlx::query_as(
        r#"SELECT t.task_id, t.max_attempts, t.timeout_ms
             FROM tasks t
            WHERE t.runnable = TRUE
              AND t.status IN ('open', 'in_progress')
              AND t.current_attempt_id IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM task_deps d
                  JOIN tasks b ON b.task_id = d.blocker_id
                   WHERE d.task_id = t.task_id AND b.status <> 'closed'
              )
              -- Skip when a non-terminal run already exists (e.g. a stuck
              -- 'running' row past heartbeat — that's yggdrasil-99 territory).
              AND NOT EXISTS (
                  SELECT 1 FROM task_runs r
                   WHERE r.task_id = t.task_id
                     AND r.state IN ('scheduled', 'ready', 'running', 'retrying')
              )
              -- Approval gate: tasks at approve_plan/completion need a
              -- positive approved_at before the scheduler will dispatch.
              -- yggdrasil-109: $1 short-circuits the gate when YGG_AUTO_APPROVE.
              AND ($1 OR t.approval_level = 'auto' OR t.approved_at IS NOT NULL)
            ORDER BY t.priority, t.updated_at
            LIMIT 100"#,
    )
    .bind(cfg.auto_approve)
    .fetch_all(pool)
    .await?;

    let mut scheduled = 0i64;
    for (task_id, max_attempts, timeout_ms) in rows {
        let prev_attempt: Option<i32> =
            sqlx::query_scalar("SELECT MAX(attempt) FROM task_runs WHERE task_id = $1")
                .bind(&task_id)
                .fetch_one(pool)
                .await
                .ok()
                .flatten();
        let attempt = prev_attempt.unwrap_or(0) + 1;
        let max = max_attempts.unwrap_or(cfg.default_max_attempts);

        let deadline =
            timeout_ms.map(|ms| chrono::Utc::now() + chrono::Duration::milliseconds(ms.max(0)));

        let key = idempotency_key_for(&task_id, attempt);
        // Insert directly as 'ready'. If we crash mid-tick the row is on disk
        // and the next tick picks it up via dispatch_ready.
        let row: Option<String> = sqlx::query_scalar(
            r#"INSERT INTO task_runs
               (task_id, attempt, idempotency_key, state, max_attempts,
                heartbeat_ttl_s, deadline_at, input)
               VALUES ($1, $2, $3, 'ready', $4, $5, $6, $7)
               ON CONFLICT (idempotency_key) DO NOTHING
               RETURNING run_id"#,
        )
        .bind(&task_id)
        .bind(attempt)
        .bind(&key)
        .bind(max)
        .bind(cfg.default_heartbeat_ttl_s)
        .bind(deadline)
        .bind(serde_json::json!({}))
        .fetch_optional(pool)
        .await?;

        if let Some(run_id) = row {
            scheduled += 1;
            let task_ref = task_ref_for(pool, &task_id)
                .await
                .unwrap_or_else(|| task_id.to_string());
            EventRepo::new(pool)
                .emit(
                    EventKind::RunScheduled,
                    "scheduler",
                    None,
                    serde_json::json!({
                        "task_ref": task_ref,
                        "run_id": run_id,
                        "attempt": attempt,
                    }),
                )
                .await
                .ok();
        }
    }
    Ok(scheduled)
}

/// (Stage 2) Mark running runs whose heartbeat is past TTL as crashed.
/// PreToolUse hook bumps heartbeat_at on every tool call, so a quiet run
/// past the TTL is genuinely stuck (tmux dead, claude killed, OS evicted).
pub async fn reap_expired_heartbeats(pool: &DbPool) -> Result<i64, anyhow::Error> {
    let rows: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
        r#"UPDATE task_runs
              SET state = 'crashed',
                  reason = 'heartbeat_timeout',
                  ended_at = COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
                  updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE state = 'running'
              -- Hermes runs have no local heartbeat (no in-process worker); the
              -- gateway poller (hermes_backend::reconcile_running) owns their
              -- liveness. Excluding them keeps the tmux reap byte-identical.
              AND backend <> 'hermes'
              AND heartbeat_at IS NOT NULL
              AND heartbeat_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || heartbeat_ttl_s || ' seconds')
        RETURNING run_id, task_id, attempt, agent_id"#,
    )
    .fetch_all(pool)
    .await?;

    for (run_id, task_id, attempt, agent_id) in &rows {
        let task_ref = task_ref_for(pool, task_id)
            .await
            .unwrap_or_else(|| task_id.to_string());
        EventRepo::new(pool)
            .emit(
                EventKind::RunTerminal,
                "scheduler",
                agent_id.clone(),
                serde_json::json!({
                    "task_ref": task_ref,
                    "run_id": run_id,
                    "attempt": attempt,
                    "state": "crashed",
                    "reason": "heartbeat_timeout",
                    "by": "scheduler.reap",
                }),
            )
            .await
            .ok();
    }
    Ok(rows.len() as i64)
}

/// (Stage 2) Enforce deadlines on running runs. A deadline_at in the past
/// transitions the run to cancelled with reason=timeout. Forceful: the
/// scheduler does not currently signal the agent — that's a follow-up.
pub async fn enforce_deadlines(pool: &DbPool) -> Result<i64, anyhow::Error> {
    let rows: Vec<(String, String, i32)> = sqlx::query_as(
        r#"UPDATE task_runs
              SET state = 'cancelled',
                  reason = 'timeout',
                  ended_at = COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
                  updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE state = 'running'
              -- Hermes deadlines are enforced by the gateway poller so it can
              -- also POST /v1/tasks/{id}/cancel on the remote side.
              AND backend <> 'hermes'
              AND deadline_at IS NOT NULL
              AND deadline_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
        RETURNING run_id, task_id, attempt"#,
    )
    .fetch_all(pool)
    .await?;

    for (run_id, task_id, attempt) in &rows {
        let task_ref = task_ref_for(pool, task_id)
            .await
            .unwrap_or_else(|| task_id.to_string());
        EventRepo::new(pool)
            .emit(
                EventKind::RunTerminal,
                "scheduler",
                None,
                serde_json::json!({
                    "task_ref": task_ref,
                    "run_id": run_id,
                    "attempt": attempt,
                    "state": "cancelled",
                    "reason": "timeout",
                    "by": "scheduler.deadline",
                }),
            )
            .await
            .ok();
    }
    Ok(rows.len() as i64)
}

/// (Stage 3) Compute a fingerprint for a terminal run. Combines task-level
/// invariants (title, acceptance, kind) with the failure mode (state +
/// reason + first 200 chars of error message). N consecutive matching
/// fingerprints on the same task triggers poison.
fn compute_fingerprint(
    title: &str,
    acceptance: Option<&str>,
    kind: &str,
    state: RunState,
    reason: RunReason,
    error_message: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\x00");
    if let Some(a) = acceptance {
        hasher.update(a.as_bytes());
    }
    hasher.update(b"\x00");
    hasher.update(kind.as_bytes());
    hasher.update(b"\x00");
    hasher.update(state.as_str().as_bytes());
    hasher.update(b"\x00");
    hasher.update(reason.as_str().as_bytes());
    hasher.update(b"\x00");
    if let Some(msg) = error_message {
        let prefix: String = msg.chars().take(200).collect();
        hasher.update(prefix.as_bytes());
    }
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

/// (Stage 3) Compute fingerprints for terminal runs that haven't been
/// fingerprinted yet, and poison tasks with N consecutive matching prints.
pub async fn detect_loops(pool: &DbPool, threshold: i32) -> Result<i64, anyhow::Error> {
    // 1. Backfill fingerprints for any terminal run missing one.
    let pending: Vec<(
        String,
        String,
        RunState,
        RunReason,
        Option<serde_json::Value>,
    )> = sqlx::query_as(
        r#"SELECT r.run_id, r.task_id, r.state, r.reason, r.error
             FROM task_runs r
            WHERE r.fingerprint IS NULL
              AND r.state IN ('failed', 'crashed', 'cancelled', 'succeeded')"#,
    )
    .fetch_all(pool)
    .await?;

    for (run_id, task_id, state, reason, error) in pending {
        let task_meta: Option<(String, Option<String>, String)> =
            sqlx::query_as("SELECT title, acceptance, kind FROM tasks WHERE task_id = $1")
                .bind(&task_id)
                .fetch_optional(pool)
                .await?;
        let Some((title, acceptance, kind)) = task_meta else {
            continue;
        };
        let msg = error
            .as_ref()
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let fp = compute_fingerprint(
            &title,
            acceptance.as_deref(),
            &kind,
            state,
            reason,
            msg.as_deref(),
        );
        sqlx::query("UPDATE task_runs SET fingerprint = $2 WHERE run_id = $1")
            .bind(&run_id)
            .bind(fp)
            .execute(pool)
            .await?;
    }

    // 2. For each task, look at its last `threshold` failure-shaped runs.
    //    If their fingerprints all match, poison the task.
    let candidates: Vec<(String,)> = sqlx::query_as(
        r#"SELECT DISTINCT t.task_id
             FROM tasks t
             JOIN task_runs r USING (task_id)
            WHERE t.status NOT IN ('closed')
              AND r.state IN ('failed', 'crashed')
              AND r.fingerprint IS NOT NULL
            GROUP BY t.task_id
           HAVING COUNT(*) FILTER (WHERE r.state IN ('failed','crashed')) >= $1"#,
    )
    .bind(threshold as i64)
    .fetch_all(pool)
    .await?;

    let mut poisoned = 0i64;
    for (task_id,) in candidates {
        let recent: Vec<(String,)> = sqlx::query_as(
            r#"SELECT fingerprint FROM task_runs
                WHERE task_id = $1 AND state IN ('failed','crashed') AND fingerprint IS NOT NULL
                ORDER BY attempt DESC
                LIMIT $2"#,
        )
        .bind(&task_id)
        .bind(threshold as i64)
        .fetch_all(pool)
        .await?;
        if recent.len() < threshold as usize {
            continue;
        }
        let first = &recent[0].0;
        if !recent.iter().all(|(fp,)| fp == first) {
            continue;
        }

        // All N fingerprints match → poison.
        let mut tx = pool.begin().await?;
        sqlx::query(
            r#"UPDATE task_runs
                  SET state = 'poison', reason = 'loop_detected', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                WHERE task_id = $1 AND state IN ('failed', 'crashed', 'retrying')"#,
        )
        .bind(&task_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"UPDATE tasks
                  SET status = 'blocked',
                      close_reason = 'poison: loop_detected (' || $2 || ' identical failures)',
                      current_attempt_id = NULL,
                      updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                WHERE task_id = $1"#,
        )
        .bind(&task_id)
        .bind(threshold)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let task_ref = task_ref_for(pool, &task_id)
            .await
            .unwrap_or_else(|| task_id.to_string());
        EventRepo::new(pool)
            .emit(
                EventKind::RunTerminal,
                "scheduler",
                None,
                serde_json::json!({
                    "task_ref": task_ref,
                    "state": "poison",
                    "reason": "loop_detected",
                    "by": "scheduler.loop_detector",
                    "threshold": threshold,
                }),
            )
            .await
            .ok();
        poisoned += 1;
    }
    Ok(poisoned)
}

/// (Stage 3) Per-repo and per-epic concurrency budgets layered on top of the
/// global per-host cap. Returns the run_ids the scheduler may dispatch this
/// tick. Default per-repo cap is 3; per-epic cap matches.
pub async fn dispatchable_under_budgets(
    pool: &DbPool,
    cfg: &SchedulerConfig,
) -> Result<Vec<String>, anyhow::Error> {
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_runs WHERE state = 'running'")
        .fetch_one(pool)
        .await?;
    let host_budget = (cfg.max_concurrent - live).max(0);
    if host_budget == 0 {
        return Ok(Vec::new());
    }

    // Pick ready runs ordered by priority + age, then filter by per-repo and
    // per-epic budgets. Could be done in SQL but the cap math is clearer in
    // Rust and the candidate set is small (host_budget rows max).
    let candidates: Vec<(String, String, String)> = sqlx::query_as(
        r#"SELECT tr.run_id, t.task_id, t.repo_id
             FROM task_runs tr
             JOIN tasks t USING (task_id)
            WHERE tr.state = 'ready'
              -- yggdrasil-108 follow-up: re-check runnable so `ygg task
              -- runnable --off` cancels already-queued ready runs on the next
              -- tick. Otherwise the column flip only stops *future* schedules.
              AND t.runnable = TRUE
              AND NOT EXISTS (
                  SELECT 1 FROM task_deps d
                  JOIN tasks bt ON bt.task_id = d.blocker_id
                   WHERE d.task_id = tr.task_id AND bt.status <> 'closed'
              )
              -- yggdrasil-109: $1 short-circuits the gate when YGG_AUTO_APPROVE.
              AND ($1 OR t.approval_level = 'auto' OR t.approved_at IS NOT NULL)
            ORDER BY t.priority, tr.scheduled_at"#,
    )
    .bind(cfg.auto_approve)
    .fetch_all(pool)
    .await?;

    let per_repo_cap = std::env::var("YGG_SCHEDULER_MAX_PER_REPO")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(3);

    let per_repo_live: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT t.repo_id, COUNT(*)
             FROM task_runs r JOIN tasks t USING (task_id)
            WHERE r.state = 'running'
            GROUP BY t.repo_id"#,
    )
    .fetch_all(pool)
    .await?;
    let mut repo_used: std::collections::HashMap<String, i64> = per_repo_live.into_iter().collect();

    let mut chosen = Vec::new();
    for (run_id, _task_id, repo_id) in candidates {
        if chosen.len() as i64 >= host_budget {
            break;
        }
        let used = repo_used.entry(repo_id).or_insert(0);
        if *used >= per_repo_cap {
            continue;
        }
        *used += 1;
        chosen.push(run_id);
    }
    Ok(chosen)
}

/// `ygg task unpoison <ref>` — clear the poison state and let the scheduler
/// retry. Resets the latest run's state to allow retries within max_attempts;
/// flips task.status back to open.
pub async fn unpoison(pool: &DbPool, task_id: &str) -> Result<(), anyhow::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"UPDATE task_runs
              SET state = 'failed', reason = 'agent_error', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE task_id = $1 AND state = 'poison'"#,
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        r#"UPDATE tasks
              SET status = 'open',
                  close_reason = NULL,
                  closed_at = NULL,
                  updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE task_id = $1 AND status = 'blocked'"#,
    )
    .bind(task_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// `ygg task approve <ref>` — mark the task as approved. Tasks with
/// approval_level=approve_plan stay in 'ready' until approved_at is set.
pub async fn approve(
    pool: &DbPool,
    task_id: &str,
    approver: Option<String>,
) -> Result<(), anyhow::Error> {
    sqlx::query(
        r#"UPDATE tasks
              SET approved_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                  approved_by_agent_id = $2,
                  updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE task_id = $1"#,
    )
    .bind(task_id)
    .bind(approver)
    .execute(pool)
    .await?;
    Ok(())
}

/// (Stage 2) For failed/crashed runs whose backoff window elapsed and that
/// haven't already produced a successor, insert a new attempt row. The
/// previous attempt's error gets threaded into input.previous_attempt so the
/// retry agent can avoid the same failure mode.
pub async fn schedule_retries(pool: &DbPool, cfg: &SchedulerConfig) -> Result<i64, anyhow::Error> {
    let candidates: Vec<(
        String,
        String,
        i32,
        i32,
        RunReason,
        Option<chrono::DateTime<chrono::Utc>>,
        serde_json::Value,
        Option<serde_json::Value>,
        serde_json::Value,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<i64>,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT r.run_id, r.task_id, r.attempt, r.max_attempts, r.reason, r.ended_at,
                  r.input, r.error, r.retry_strategy,
                  r.deadline_at, t.timeout_ms, r.pre_recovery_commit
             FROM task_runs r
             JOIN tasks t ON t.task_id = r.task_id
            WHERE r.state IN ('failed', 'crashed')
              AND r.attempt < r.max_attempts
              AND t.status <> 'closed'
              AND NOT EXISTS (
                  SELECT 1 FROM task_runs r2
                   WHERE r2.task_id = r.task_id AND r2.attempt = r.attempt + 1
              )
            ORDER BY r.ended_at NULLS FIRST
            LIMIT 100"#,
    )
    .fetch_all(pool)
    .await?;

    let mut retried = 0i64;
    let now = chrono::Utc::now();
    for (
        run_id,
        task_id,
        attempt,
        _max,
        _reason,
        ended_at,
        input,
        error,
        retry_strategy,
        _old_deadline,
        timeout_ms,
        pre_recovery_commit,
    ) in candidates
    {
        let backoff_ms = compute_backoff_ms(&retry_strategy, attempt);
        let due = ended_at
            .map(|t| t + chrono::Duration::milliseconds(backoff_ms))
            .unwrap_or(now);
        if due > now {
            continue;
        }

        let next_attempt = attempt + 1;
        let key = idempotency_key_for(&task_id, next_attempt);

        // Thread previous attempt summary forward.
        let mut next_input = input.clone();
        if let serde_json::Value::Object(map) = &mut next_input {
            map.insert(
                "previous_attempt".into(),
                serde_json::json!({
                    "run_id": run_id,
                    "attempt": attempt,
                    "error": error,
                    "pre_recovery_commit": pre_recovery_commit,
                }),
            );
        }

        let deadline =
            timeout_ms.map(|ms| chrono::Utc::now() + chrono::Duration::milliseconds(ms.max(0)));

        let new_run: Option<String> = sqlx::query_scalar(
            r#"INSERT INTO task_runs
               (task_id, attempt, parent_run_id, idempotency_key, state,
                heartbeat_ttl_s, deadline_at, input)
               VALUES ($1, $2, $3, $4, 'ready', $5, $6, $7)
               ON CONFLICT (idempotency_key) DO NOTHING
               RETURNING run_id"#,
        )
        .bind(&task_id)
        .bind(next_attempt)
        .bind(&run_id)
        .bind(&key)
        .bind(cfg.default_heartbeat_ttl_s)
        .bind(deadline)
        .bind(next_input)
        .fetch_optional(pool)
        .await?;

        if let Some(new_run_id) = new_run {
            // Mark the previous run as 'retrying' for the brief window before
            // the new attempt starts. This gives `ygg task show` a clean
            // narrative ("attempt 1 failed → retrying → attempt 2 ...").
            sqlx::query(
                "UPDATE task_runs SET state = 'retrying', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE run_id = $1",
            )
            .bind(&run_id)
            .execute(pool)
            .await?;

            let task_ref = task_ref_for(pool, &task_id)
                .await
                .unwrap_or_else(|| task_id.to_string());
            EventRepo::new(pool)
                .emit(
                    EventKind::RunRetry,
                    "scheduler",
                    None,
                    serde_json::json!({
                        "task_ref": task_ref,
                        "run_id": new_run_id,
                        "parent_run_id": run_id,
                        "attempt": next_attempt,
                        "backoff_ms": backoff_ms,
                    }),
                )
                .await
                .ok();
            retried += 1;
        }
    }
    Ok(retried)
}

/// Exponential backoff with optional jitter. Mirrors the JSONB shape stored
/// on task_runs.retry_strategy and the default in the migration.
fn compute_backoff_ms(strategy: &serde_json::Value, attempt: i32) -> i64 {
    let kind = strategy
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("exponential");
    let base_ms = strategy
        .get("base_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(60_000);
    let cap_ms = strategy
        .get("cap_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(600_000);
    let jitter = strategy
        .get("jitter")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let raw = match kind {
        "fixed" => base_ms,
        _ => {
            // 2^(attempt-1) * base, clamped to cap.
            let shift = (attempt.saturating_sub(1).max(0).min(20)) as u32;
            base_ms
                .saturating_mul(1i64.checked_shl(shift).unwrap_or(i64::MAX))
                .min(cap_ms)
        }
    };

    if jitter {
        // ±25% jitter, deterministic from a structural source so tests don't
        // flake. Use lower bits of run + attempt to avoid pulling a full RNG.
        let frac = ((attempt as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15_u64 as i64)) & 0x7F;
        let jitter_ms = (raw / 4) * (frac - 64) / 64;
        (raw + jitter_ms).max(0)
    } else {
        raw
    }
}

/// (3) Claim ready runs up to budget; spawn agents; bind run→agent.
pub async fn dispatch_ready(pool: &DbPool, cfg: &SchedulerConfig) -> Result<i64, anyhow::Error> {
    // Stage 3: budget = host - live, then filter through per-repo cap.
    let eligible = dispatchable_under_budgets(pool, cfg).await?;
    if eligible.is_empty() {
        return Ok(0);
    }

    // The claim. Postgres used FOR UPDATE SKIP LOCKED here; on SQLite a
    // single-host BEGIN IMMEDIATE transaction takes the write lock up front
    // (busy_timeout + WAL absorb hook contention) and the atomic
    // ready→running UPDATE keeps the claim idempotent.
    let eligible_json = serde_json::to_string(&eligible).unwrap_or_else(|_| "[]".to_string());
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let claimed: Vec<(String, String, i32, serde_json::Value)> = sqlx::query_as(
        r#"UPDATE task_runs
              SET state = 'running',
                  claimed_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                  started_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                  heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                  updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE run_id IN (SELECT value FROM json_each($1))
              AND state = 'ready'
        RETURNING run_id, task_id, attempt, input"#,
    )
    .bind(&eligible_json)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    if claimed.is_empty() {
        return Ok(0);
    }

    let app_cfg = AppConfig::from_env().map_err(|e| anyhow::anyhow!("config: {e}"))?;
    let mut dispatched = 0i64;
    for (run_id, task_id, attempt, input) in claimed {
        // Pull task title for the spawn prompt + ref for events. agent_slug
        // (yggdrasil-183) names the worker thematically when the creator set it.
        let row: Option<(String, String, i32, String, Option<String>)> = sqlx::query_as(
            r#"SELECT t.title, r.task_prefix, t.seq, COALESCE(t.description, ''), t.agent_slug
                 FROM tasks t JOIN repos r USING (repo_id)
                WHERE t.task_id = $1"#,
        )
        .bind(&task_id)
        .fetch_optional(pool)
        .await?;
        let Some((title, prefix, seq, desc, agent_slug)) = row else {
            // Task vanished underneath us; mark crashed so we don't retry.
            sqlx::query(
                "UPDATE task_runs SET state = 'crashed', reason = 'dependency_failed',
                                       ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                  WHERE run_id = $1",
            )
            .bind(&run_id)
            .execute(pool)
            .await?;
            continue;
        };

        let task_ref = format!("{prefix}-{seq}");

        // Part B: hermes-backed tasks dispatch to a sandboxed gateway agent
        // instead of a local tmux worker. The tmux branch below is unchanged.
        // The backend selector is a separate lightweight read so the tmux-path
        // query above stays byte-identical.
        let backend: Option<String> =
            sqlx::query_scalar("SELECT backend FROM tasks WHERE task_id = $1")
                .bind(&task_id)
                .fetch_optional(pool)
                .await?
                .flatten();
        if let Some(hermes_agent) = crate::cli::hermes_backend::hermes_agent(backend.as_deref()) {
            let hermes_prompt = format!(
                "{recovery}[ygg-scheduler] Task {task_ref} (attempt {attempt}): {title}\n\n{desc}",
                recovery = recovery_note(&input),
            );
            match crate::cli::hermes_backend::dispatch(
                pool,
                &app_cfg,
                &run_id,
                &task_id,
                &hermes_agent,
                &hermes_prompt,
            )
            .await
            {
                Ok(remote_id) => {
                    EventRepo::new(pool)
                        .emit(
                            EventKind::RunClaimed,
                            "scheduler",
                            None,
                            serde_json::json!({
                                "task_ref": task_ref,
                                "run_id": run_id,
                                "attempt": attempt,
                                "backend": "hermes",
                                "agent": hermes_agent,
                                "remote_task_id": remote_id,
                            }),
                        )
                        .await
                        .ok();
                    dispatched += 1;
                }
                Err(err) => {
                    tracing::warn!(error = %err, task_ref = %task_ref, "hermes dispatch failed");
                    sqlx::query(
                        "UPDATE task_runs
                            SET state = 'crashed',
                                reason = 'agent_error',
                                backend = 'hermes',
                                error = json_object('reason_code', 'agent_error',
                                                    'message', $2),
                                ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                                updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                          WHERE run_id = $1",
                    )
                    .bind(&run_id)
                    .bind(err.to_string())
                    .execute(pool)
                    .await?;
                    EventRepo::new(pool)
                        .emit(
                            EventKind::SchedulerError,
                            "scheduler",
                            None,
                            serde_json::json!({
                                "phase": "hermes_dispatch",
                                "task_ref": task_ref,
                                "run_id": run_id,
                                "error": err.to_string(),
                            }),
                        )
                        .await
                        .ok();
                }
            }
            continue;
        }

        let agent_name = scheduler_agent_name(&prefix, seq, attempt, agent_slug.as_deref());
        let prompt = format!(
            "{recovery}[ygg-scheduler] Task {task_ref} (attempt {attempt}): {title}\n\n{desc}\n\n\
             Run `ygg run claim {task_ref}` to bind your session, then implement and \
             commit. The scheduler is watching this run; close the task with \
             `ygg task close {task_ref} --reason '...'` when done.",
            recovery = recovery_note(&input),
        );

        // Use crate::cli::spawn::execute. A failure to spawn marks the run
        // crashed so the failure is observable and won't burn a retry budget
        // on a non-recoverable problem (tmux down, no claude binary, ...).
        match crate::cli::spawn::execute(pool, &app_cfg, &prompt, Some(&agent_name)).await {
            Ok(()) => {
                // Bind the run to the agent we just registered (spawn::execute
                // registers the agent before launching tmux).
                let agent_id: Option<String> =
                    sqlx::query_scalar("SELECT agent_id FROM agents WHERE name = $1 LIMIT 1")
                        .bind(&agent_name)
                        .fetch_optional(pool)
                        .await
                        .ok()
                        .flatten();
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "UPDATE task_runs SET agent_id = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE run_id = $1",
                )
                .bind(&run_id)
                .bind(agent_id)
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "UPDATE tasks SET current_attempt_id = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE task_id = $1",
                )
                .bind(&task_id)
                .bind(&run_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;

                EventRepo::new(pool)
                    .emit(
                        EventKind::RunClaimed,
                        "scheduler",
                        None,
                        serde_json::json!({
                            "task_ref": task_ref,
                            "run_id": run_id,
                            "attempt": attempt,
                            "agent": agent_name,
                        }),
                    )
                    .await
                    .ok();
                dispatched += 1;
            }
            Err(err) => {
                tracing::warn!(error = %err, task_ref = %task_ref, "scheduler spawn failed");
                sqlx::query(
                    "UPDATE task_runs
                        SET state = 'crashed',
                            reason = 'tmux_gone',
                            error = json_object('reason_code', 'tmux_gone',
                                                'message', $2),
                            ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                            updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                      WHERE run_id = $1",
                )
                .bind(&run_id)
                .bind(err.to_string())
                .execute(pool)
                .await?;
                EventRepo::new(pool)
                    .emit(
                        EventKind::SchedulerError,
                        "scheduler",
                        None,
                        serde_json::json!({
                            "phase": "spawn",
                            "task_ref": task_ref,
                            "run_id": run_id,
                            "error": err.to_string(),
                        }),
                    )
                    .await
                    .ok();
            }
        }
    }
    Ok(dispatched)
}

/// (yggdrasil-95) One-shot migration: synthesize task_runs rows for tasks
/// that predate ADR 0016. Idempotent — safe to re-run; uses ON CONFLICT on
/// the idempotency key. For in_progress tasks with a live worker, creates a
/// running run; for closed tasks, creates a succeeded run pointing at the
/// last known commit (best-effort).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BackfillStats {
    pub in_progress_runs_created: i64,
    pub closed_runs_created: i64,
    pub skipped: i64,
}

pub async fn backfill(pool: &DbPool) -> Result<BackfillStats, anyhow::Error> {
    let mut stats = BackfillStats::default();

    // In-progress tasks with no current_attempt_id and no existing run rows.
    let in_progress: Vec<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT t.task_id, t.assignee
             FROM tasks t
            WHERE t.status = 'in_progress'
              AND t.current_attempt_id IS NULL
              AND NOT EXISTS (SELECT 1 FROM task_runs r WHERE r.task_id = t.task_id)"#,
    )
    .fetch_all(pool)
    .await?;

    for (task_id, assignee) in in_progress {
        let key = idempotency_key_for(&task_id, 1);
        let row: Option<String> = sqlx::query_scalar(
            r#"INSERT INTO task_runs
               (task_id, attempt, idempotency_key, state, agent_id,
                started_at, heartbeat_at, claimed_at, scheduled_at)
               VALUES ($1, 1, $2, 'running', $3, strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
               ON CONFLICT (idempotency_key) DO NOTHING
               RETURNING run_id"#,
        )
        .bind(&task_id)
        .bind(&key)
        .bind(assignee)
        .fetch_optional(pool)
        .await?;
        if let Some(run_id) = row {
            sqlx::query("UPDATE tasks SET current_attempt_id = $2 WHERE task_id = $1")
                .bind(&task_id)
                .bind(run_id)
                .execute(pool)
                .await?;
            stats.in_progress_runs_created += 1;
        } else {
            stats.skipped += 1;
        }
    }

    // Closed tasks with no run rows. We can't reliably reconstruct the agent
    // or commit, so we leave those NULL — the row is mainly so `ygg task show`
    // displays "run #1 succeeded" for completeness.
    let closed: Vec<(
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
    )> = sqlx::query_as(
        r#"SELECT t.task_id, t.closed_at, t.close_reason
             FROM tasks t
            WHERE t.status = 'closed'
              AND NOT EXISTS (SELECT 1 FROM task_runs r WHERE r.task_id = t.task_id)
            LIMIT 5000"#,
    )
    .fetch_all(pool)
    .await?;

    for (task_id, closed_at, reason) in closed {
        let key = idempotency_key_for(&task_id, 1);
        let (state, run_reason) = match reason.as_deref() {
            Some(r) if r.to_ascii_lowercase().contains("crash") => ("crashed", "tmux_gone"),
            Some(r) if r.to_ascii_lowercase().contains("cancel") => ("cancelled", "user_cancelled"),
            Some(r) if r.to_ascii_lowercase().contains("fail") => ("failed", "agent_error"),
            _ => ("succeeded", "ok"),
        };
        let row: Option<String> = sqlx::query_scalar(
            r#"INSERT INTO task_runs
               (task_id, attempt, idempotency_key, state, reason,
                started_at, ended_at, scheduled_at)
               VALUES ($1, 1, $2,
                       $3, $4,
                       strftime('%Y-%m-%dT%H:%M:%f+00:00', COALESCE($5, 'now'), '-1 minute'),
                       strftime('%Y-%m-%dT%H:%M:%f+00:00', COALESCE($5, 'now')),
                       strftime('%Y-%m-%dT%H:%M:%f+00:00', COALESCE($5, 'now'), '-1 minute'))
               ON CONFLICT (idempotency_key) DO NOTHING
               RETURNING run_id"#,
        )
        .bind(&task_id)
        .bind(&key)
        .bind(state)
        .bind(run_reason)
        .bind(closed_at)
        .fetch_optional(pool)
        .await?;
        if row.is_some() {
            stats.closed_runs_created += 1;
        } else {
            stats.skipped += 1;
        }
    }

    Ok(stats)
}

/// Name the worker that will run a scheduled task. A thematic `agent_slug`
/// (yggdrasil-183), when the task carries one, becomes the name *prefix*;
/// otherwise we fall back to the task ref (`ygg-<prefix>-<seq>`). Either way
/// we append `-a<attempt>-<suffix>` so retries and concurrent runs get
/// distinct agent rows, tmux windows, and worktrees.
fn scheduler_agent_name(prefix: &str, seq: i32, attempt: i32, agent_slug: Option<&str>) -> String {
    let suffix = uuid::Uuid::new_v4()
        .to_string()
        .chars()
        .take(6)
        .collect::<String>();
    let base = match agent_slug {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => format!("ygg-{prefix}-{seq}"),
    };
    format!("{base}-a{attempt}-{suffix}")
}

/// System-message preamble injected into a retry's spawn prompt when the prior
/// attempt was force-recovered (yggdrasil-115). The work-in-progress lives at
/// the checkpoint SHA on refs/ygg/recovery/<prior_run_id>; tell the agent how to
/// resume from it instead of restarting from the base commit. Empty for a first
/// attempt or when no checkpoint was taken.
fn recovery_note(input: &serde_json::Value) -> String {
    let sha = input
        .get("previous_attempt")
        .and_then(|p| p.get("pre_recovery_commit"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    match sha {
        Some(sha) => format!(
            "[ygg-scheduler] Previous attempt was force-recovered: its uncommitted \
             work-in-progress is checkpointed at commit {sha}. Continue from there \
             — run `git cherry-pick -n {sha}` (or `git checkout {sha} -- .`) to \
             restore it instead of starting over.\n\n"
        ),
        None => String::new(),
    }
}

async fn task_ref_for(pool: &DbPool, task_id: &str) -> Option<String> {
    sqlx::query_as::<_, (String, i32)>(
        r#"SELECT r.task_prefix, t.seq FROM tasks t
           JOIN repos r USING (repo_id) WHERE t.task_id = $1"#,
    )
    .bind(task_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(p, s)| format!("{p}-{s}"))
}

async fn emit_simple_event(
    pool: &DbPool,
    kind: EventKind,
    payload: serde_json::Value,
) -> Result<(), sqlx::Error> {
    EventRepo::new(pool)
        .emit(kind, "scheduler", None, payload)
        .await
}

/// Acquire the singleton scheduler lock: an exclusive flock() on a lock file
/// beside the SQLite database. Returned guard auto-releases on drop (process
/// exit also releases it — flocks don't survive the holder). A second
/// instance fails fast with the same visible error as before the port.
pub async fn acquire_advisory_lock(_pool: &DbPool) -> Result<AdvisoryLockGuard, anyhow::Error> {
    use fs4::fs_std::FileExt;

    let db_url = crate::db::resolve_database_url();
    let db_path = std::path::PathBuf::from(
        db_url
            .trim_start_matches("sqlite://")
            .trim_start_matches("sqlite:"),
    );
    let lock_path = match db_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(SCHEDULER_LOCK_FILE),
        _ => std::path::PathBuf::from(SCHEDULER_LOCK_FILE),
    };
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let acquired = file.try_lock_exclusive()?;
    if !acquired {
        anyhow::bail!(
            "another ygg scheduler is already running on this database (lock file {} held)",
            lock_path.display()
        );
    }
    Ok(AdvisoryLockGuard { _file: file })
}

pub struct AdvisoryLockGuard {
    _file: std::fs::File,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_name_includes_attempt_and_unique_suffix() {
        let n1 = scheduler_agent_name("ygg", 42, 1, None);
        let n2 = scheduler_agent_name("ygg", 42, 1, None);
        assert!(n1.starts_with("ygg-ygg-42-a1-"));
        assert!(n2.starts_with("ygg-ygg-42-a1-"));
        assert_ne!(n1, n2);
    }

    #[test]
    fn agent_name_uses_thematic_slug_when_present() {
        // yggdrasil-183: a task's agent_slug becomes the name prefix, with the
        // attempt + uniqueness suffix still appended.
        let n = scheduler_agent_name("ygg", 42, 2, Some("oauth-refresh"));
        assert!(
            n.starts_with("oauth-refresh-a2-"),
            "slug should prefix the name, got {n}"
        );
        assert!(
            !n.contains("ygg-ygg-42"),
            "task ref must not leak in, got {n}"
        );
        // Empty slug falls back to the task-ref scheme.
        let f = scheduler_agent_name("ygg", 42, 1, Some(""));
        assert!(f.starts_with("ygg-ygg-42-a1-"));
    }

    #[test]
    fn config_defaults_reasonable() {
        let app = AppConfig {
            database_url: "test".into(),
            context_limit_tokens: 250_000,
            context_hard_cap_tokens: 300_000,
            lock_ttl_secs: 300,
            heartbeat_interval_secs: 60,
            watcher_interval_secs: 30,
            rtk_binary_path: "rtk".into(),
            hermes_gateway_url: None,
            hermes_gateway_token: None,
            hermes_dashboard_token: None,
            mnemosyne_mcp_url: None,
            mnemosyne_mcp_token: None,
            mnemosyne_mcp_bank: None,
            mnemosyne_export_dir: None,
        };
        unsafe { std::env::remove_var("YGG_SCHEDULER_TICK_MS") };
        unsafe { std::env::remove_var("YGG_SCHEDULER_MAX_CONCURRENT") };
        unsafe { std::env::remove_var("YGG_SCHEDULER_POISON_THRESHOLD") };
        let cfg = SchedulerConfig::from_app(&app);
        assert_eq!(cfg.tick_interval, Duration::from_millis(2000));
        assert_eq!(cfg.max_concurrent, 4);
        assert_eq!(cfg.default_max_attempts, 3);
        assert_eq!(cfg.poison_threshold, 3);
    }

    #[test]
    fn fingerprint_stable_for_same_inputs() {
        let f1 = compute_fingerprint(
            "fix bug",
            Some("tests pass"),
            "bug",
            RunState::Failed,
            RunReason::AgentError,
            Some("cargo test failed"),
        );
        let f2 = compute_fingerprint(
            "fix bug",
            Some("tests pass"),
            "bug",
            RunState::Failed,
            RunReason::AgentError,
            Some("cargo test failed"),
        );
        assert_eq!(f1, f2);
        assert_eq!(f1.len(), 64);
    }

    #[test]
    fn recovery_note_present_only_with_checkpoint() {
        // First attempt: no previous_attempt → empty note.
        assert!(recovery_note(&serde_json::json!({})).is_empty());

        // Retry whose parent was force-recovered → note carries the SHA and a
        // concrete resume command.
        let input = serde_json::json!({
            "previous_attempt": { "pre_recovery_commit": "deadbeefcafe" }
        });
        let note = recovery_note(&input);
        assert!(note.contains("deadbeefcafe"));
        assert!(note.contains("cherry-pick"));

        // Retry with no checkpoint captured (null SHA) → still empty.
        let input = serde_json::json!({
            "previous_attempt": { "pre_recovery_commit": serde_json::Value::Null }
        });
        assert!(recovery_note(&input).is_empty());
    }

    #[test]
    fn fingerprint_distinguishes_state_and_reason() {
        let f_fail = compute_fingerprint(
            "t",
            None,
            "task",
            RunState::Failed,
            RunReason::AgentError,
            None,
        );
        let f_crash = compute_fingerprint(
            "t",
            None,
            "task",
            RunState::Crashed,
            RunReason::TmuxGone,
            None,
        );
        assert_ne!(f_fail, f_crash);
    }
}
