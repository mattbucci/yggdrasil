//! task_runs — one row per execution attempt of a task. ADR 0016 (DBOS-shaped
//! checkpoint, not Temporal-style replay). The scheduler is the only writer of
//! `state`; the Stop hook writes outcome fields (output / error / commit_sha)
//! and the scheduler reconciles them into a terminal state on its next tick.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "run_state", rename_all = "snake_case")]
pub enum RunState {
    Scheduled,
    Ready,
    Running,
    Succeeded,
    Failed,
    Crashed,
    Cancelled,
    Retrying,
    Poison,
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Crashed => "crashed",
            Self::Cancelled => "cancelled",
            Self::Retrying => "retrying",
            Self::Poison => "poison",
        }
    }

    /// Terminal states never transition further (except `retrying`, which is a
    /// transient bridge to a successor row, and is not terminal in that sense).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Crashed | Self::Cancelled | Self::Poison
        )
    }

    /// Failure-shaped terminals: candidates for retry consideration.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed | Self::Crashed)
    }
}

impl std::str::FromStr for RunState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "scheduled" => Ok(Self::Scheduled),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "crashed" => Ok(Self::Crashed),
            "cancelled" => Ok(Self::Cancelled),
            "retrying" => Ok(Self::Retrying),
            "poison" => Ok(Self::Poison),
            _ => Err(format!("unknown run_state: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "run_reason", rename_all = "snake_case")]
pub enum RunReason {
    Ok,
    AgentError,
    HeartbeatTimeout,
    TmuxGone,
    MaxAttempts,
    UserCancelled,
    DependencyFailed,
    LockConflict,
    Timeout,
    LoopDetected,
    BudgetExceeded,
}

impl std::fmt::Display for RunReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RunReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::AgentError => "agent_error",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::TmuxGone => "tmux_gone",
            Self::MaxAttempts => "max_attempts",
            Self::UserCancelled => "user_cancelled",
            Self::DependencyFailed => "dependency_failed",
            Self::LockConflict => "lock_conflict",
            Self::Timeout => "timeout",
            Self::LoopDetected => "loop_detected",
            Self::BudgetExceeded => "budget_exceeded",
        }
    }
}

impl std::str::FromStr for RunReason {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ok" => Ok(Self::Ok),
            "agent_error" => Ok(Self::AgentError),
            "heartbeat_timeout" => Ok(Self::HeartbeatTimeout),
            "tmux_gone" => Ok(Self::TmuxGone),
            "max_attempts" => Ok(Self::MaxAttempts),
            "user_cancelled" => Ok(Self::UserCancelled),
            "dependency_failed" => Ok(Self::DependencyFailed),
            "lock_conflict" => Ok(Self::LockConflict),
            "timeout" => Ok(Self::Timeout),
            "loop_detected" => Ok(Self::LoopDetected),
            "budget_exceeded" => Ok(Self::BudgetExceeded),
            _ => Err(format!("unknown run_reason: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TaskRun {
    pub run_id: String,
    pub task_id: String,
    pub attempt: i32,
    pub parent_run_id: Option<String>,
    pub idempotency_key: String,
    pub state: RunState,
    pub reason: RunReason,
    pub scheduled_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub heartbeat_ttl_s: i32,
    pub agent_id: Option<String>,
    pub worker_id: Option<String>,
    pub session_id: Option<String>,
    pub max_attempts: i32,
    pub retry_strategy: serde_json::Value,
    pub deadline_at: Option<DateTime<Utc>>,
    pub input: serde_json::Value,
    pub output: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
    pub output_commit_sha: Option<String>,
    pub output_branch: Option<String>,
    pub output_pr_url: Option<String>,
    pub output_worktree: Option<String>,
    pub output_blob_ref: Option<String>,
    pub fingerprint: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Inputs for inserting a fresh run. Most fields default sensibly; callers
/// typically populate `task_id`, `attempt`, `idempotency_key`, and `input`.
#[derive(Debug, Default)]
pub struct TaskRunCreate {
    pub task_id: String,
    pub attempt: i32,
    pub parent_run_id: Option<String>,
    pub max_attempts: Option<i32>,
    pub retry_strategy: Option<serde_json::Value>,
    pub deadline_at: Option<DateTime<Utc>>,
    pub heartbeat_ttl_s: Option<i32>,
    pub input: serde_json::Value,
}

/// Discipline ceiling for inline JSONB payloads. Anything larger should go
/// through the blob store; see `src/blob.rs`. Aligns with TOAST cliff guidance.
pub const MAX_INLINE_PAYLOAD_BYTES: usize = 16 * 1024;

pub fn idempotency_key_for(task_id: &str, attempt: i32) -> String {
    format!("run:{task_id}:attempt:{attempt}")
}

/// Reject oversize payloads at the boundary. Returns `Err` with a descriptive
/// message; callers should divert to `BlobStore::put` and reference by hash.
pub fn check_inline_size(payload: &serde_json::Value, field: &str) -> Result<(), String> {
    let serialized =
        serde_json::to_vec(payload).map_err(|e| format!("serializing {field}: {e}"))?;
    if serialized.len() > MAX_INLINE_PAYLOAD_BYTES {
        return Err(format!(
            "{field} payload {} bytes > MAX_INLINE_PAYLOAD_BYTES ({} bytes); \
             use blob store and reference by hash",
            serialized.len(),
            MAX_INLINE_PAYLOAD_BYTES
        ));
    }
    Ok(())
}

/// Where a payload should land after the size gate runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadSink {
    /// Under the cap — write to the JSONB column directly.
    Inline(serde_json::Value),
    /// Over the cap — write to the blob store; row gets a 64-char ref.
    Blob {
        blob_ref: String,
        original_bytes: usize,
    },
}

/// One-stop router for any task_runs JSONB payload (output / error / future
/// per-step checkpoints). Serializes once, checks against `MAX_INLINE_PAYLOAD_BYTES`,
/// and either returns the value for inline write or persists to the blob
/// store and returns a content-addressed reference. Every writer of those
/// columns goes through this so the size discipline can't drift between
/// call sites.
pub fn route_payload(
    payload: &serde_json::Value,
    store: &crate::blob::BlobStore,
) -> Result<PayloadSink, String> {
    let serialized = serde_json::to_vec(payload).map_err(|e| format!("serializing: {e}"))?;
    if serialized.len() <= MAX_INLINE_PAYLOAD_BYTES {
        return Ok(PayloadSink::Inline(payload.clone()));
    }
    let blob_ref = store
        .put(&serialized)
        .map_err(|e| format!("blob put: {e}"))?;
    Ok(PayloadSink::Blob {
        blob_ref: blob_ref.as_str().to_string(),
        original_bytes: serialized.len(),
    })
}

pub struct TaskRunRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> TaskRunRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Insert a new run. The idempotency_key derives from (task_id, attempt)
    /// so duplicate inserts collide on the UNIQUE constraint and surface as
    /// errors rather than silent doubles.
    pub async fn create(&self, spec: TaskRunCreate) -> Result<TaskRun, sqlx::Error> {
        check_inline_size(&spec.input, "input").map_err(sqlx::Error::Protocol)?;

        let key = idempotency_key_for(&spec.task_id, spec.attempt);
        let max_attempts = spec.max_attempts.unwrap_or(3);
        let heartbeat_ttl_s = spec.heartbeat_ttl_s.unwrap_or(300);

        sqlx::query_as::<_, TaskRun>(
            r#"INSERT INTO task_runs
               (task_id, attempt, parent_run_id, idempotency_key,
                max_attempts, retry_strategy, deadline_at, heartbeat_ttl_s, input)
               VALUES ($1, $2, $3, $4, $5,
                       COALESCE($6, '{"kind":"exponential","base_ms":60000,"cap_ms":600000,"jitter":true}'),
                       $7, $8, $9)
               RETURNING *"#,
        )
        .bind(spec.task_id)
        .bind(spec.attempt)
        .bind(spec.parent_run_id)
        .bind(&key)
        .bind(max_attempts)
        .bind(spec.retry_strategy)
        .bind(spec.deadline_at)
        .bind(heartbeat_ttl_s)
        .bind(spec.input)
        .fetch_one(self.pool)
        .await
    }

    pub async fn get(&self, run_id: &str) -> Result<Option<TaskRun>, sqlx::Error> {
        sqlx::query_as::<_, TaskRun>("SELECT * FROM task_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_optional(self.pool)
            .await
    }

    /// Most recent attempts first.
    pub async fn list_by_task(&self, task_id: &str) -> Result<Vec<TaskRun>, sqlx::Error> {
        sqlx::query_as::<_, TaskRun>(
            "SELECT * FROM task_runs WHERE task_id = $1 ORDER BY attempt DESC",
        )
        .bind(task_id)
        .fetch_all(self.pool)
        .await
    }

    /// Latest attempt for a task, if any.
    pub async fn latest(&self, task_id: &str) -> Result<Option<TaskRun>, sqlx::Error> {
        sqlx::query_as::<_, TaskRun>(
            "SELECT * FROM task_runs WHERE task_id = $1 ORDER BY attempt DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Heartbeat bump — typically called by the PreToolUse hook on every tool
    /// invocation so the scheduler knows the run is alive.
    pub async fn heartbeat(&self, run_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE task_runs SET heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE run_id = $1",
        )
        .bind(run_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Write `output` (JSONB) for a run, routing through the size gate. Small
    /// payloads land in the column; large ones land in the blob store and the
    /// 64-char `output_blob_ref` is set instead. The two columns are mutually
    /// exclusive on this writer's writes, so readers can `COALESCE` over them
    /// without ambiguity.
    pub async fn write_output(
        &self,
        run_id: &str,
        payload: &serde_json::Value,
        store: &crate::blob::BlobStore,
    ) -> Result<PayloadSink, sqlx::Error> {
        let sink = route_payload(payload, store).map_err(sqlx::Error::Protocol)?;
        match &sink {
            PayloadSink::Inline(value) => {
                sqlx::query(
                    r#"UPDATE task_runs
                       SET output = $2,
                           output_blob_ref = NULL,
                           updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                       WHERE run_id = $1"#,
                )
                .bind(run_id)
                .bind(value)
                .execute(self.pool)
                .await?;
            }
            PayloadSink::Blob { blob_ref, .. } => {
                sqlx::query(
                    r#"UPDATE task_runs
                       SET output = NULL,
                           output_blob_ref = $2,
                           updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                       WHERE run_id = $1"#,
                )
                .bind(run_id)
                .bind(blob_ref)
                .execute(self.pool)
                .await?;
            }
        }
        Ok(sink)
    }

    /// Same gate for `error`. Errors that exceed the cap are rare in practice
    /// (they're usually short messages) but agent-tool transcripts attached
    /// for debugging can spike, so the same routing applies.
    pub async fn write_error(
        &self,
        run_id: &str,
        payload: &serde_json::Value,
        store: &crate::blob::BlobStore,
    ) -> Result<PayloadSink, sqlx::Error> {
        let sink = route_payload(payload, store).map_err(sqlx::Error::Protocol)?;
        match &sink {
            PayloadSink::Inline(value) => {
                sqlx::query(
                    "UPDATE task_runs SET error = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE run_id = $1",
                )
                .bind(run_id)
                .bind(value)
                .execute(self.pool)
                .await?;
            }
            PayloadSink::Blob { blob_ref, .. } => {
                // Error spillover is rare enough that we don't have a dedicated
                // error_blob_ref column; reuse output_blob_ref with an
                // {"blob_ref": "..."} stub in `error` so readers know where to
                // look. If errors start spilling regularly, add a column.
                let stub = serde_json::json!({
                    "spilled_to_blob": blob_ref,
                });
                sqlx::query(
                    r#"UPDATE task_runs
                       SET error = $2,
                           output_blob_ref = COALESCE(output_blob_ref, $3),
                           updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                       WHERE run_id = $1"#,
                )
                .bind(run_id)
                .bind(&stub)
                .bind(blob_ref)
                .execute(self.pool)
                .await?;
            }
        }
        Ok(sink)
    }

    /// Set the run's state. Caller is responsible for ensuring the transition
    /// is legal — Rust enforces, not the DB. Sets `ended_at` if transitioning
    /// to a terminal state.
    pub async fn set_state(
        &self,
        run_id: &str,
        state: RunState,
        reason: RunReason,
    ) -> Result<(), sqlx::Error> {
        let mark_ended = state.is_terminal();
        sqlx::query(
            r#"UPDATE task_runs
               SET state = $2,
                   reason = $3,
                   ended_at = CASE WHEN $4 THEN COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')) ELSE ended_at END,
                   updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
               WHERE run_id = $1"#,
        )
        .bind(run_id)
        .bind(state)
        .bind(reason)
        .bind(mark_ended)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_state_terminal_classification() {
        assert!(RunState::Succeeded.is_terminal());
        assert!(RunState::Failed.is_terminal());
        assert!(RunState::Crashed.is_terminal());
        assert!(RunState::Cancelled.is_terminal());
        assert!(RunState::Poison.is_terminal());
        assert!(!RunState::Scheduled.is_terminal());
        assert!(!RunState::Ready.is_terminal());
        assert!(!RunState::Running.is_terminal());
        assert!(!RunState::Retrying.is_terminal());
    }

    #[test]
    fn run_state_failure_classification() {
        assert!(RunState::Failed.is_failure());
        assert!(RunState::Crashed.is_failure());
        assert!(!RunState::Succeeded.is_failure());
        assert!(!RunState::Cancelled.is_failure());
    }

    #[test]
    fn idempotency_key_format() {
        let id: String = "11111111-1111-1111-1111-111111111111".parse().unwrap();
        assert_eq!(
            idempotency_key_for(&id, 1),
            "run:11111111-1111-1111-1111-111111111111:attempt:1"
        );
    }

    #[test]
    fn check_inline_size_under_cap() {
        let v = serde_json::json!({"k": "v"});
        check_inline_size(&v, "input").unwrap();
    }

    #[test]
    fn check_inline_size_over_cap() {
        let big = "x".repeat(MAX_INLINE_PAYLOAD_BYTES + 1);
        let v = serde_json::json!({"k": big});
        let err = check_inline_size(&v, "output").unwrap_err();
        assert!(err.contains("output payload"));
    }

    #[test]
    fn run_state_round_trip() {
        for s in [
            "scheduled",
            "ready",
            "running",
            "succeeded",
            "failed",
            "crashed",
            "cancelled",
            "retrying",
            "poison",
        ] {
            let parsed: RunState = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }

    #[test]
    fn run_reason_round_trip() {
        for s in [
            "ok",
            "agent_error",
            "heartbeat_timeout",
            "tmux_gone",
            "max_attempts",
            "user_cancelled",
            "dependency_failed",
            "lock_conflict",
            "timeout",
            "loop_detected",
            "budget_exceeded",
        ] {
            let parsed: RunReason = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
        }
    }
}
