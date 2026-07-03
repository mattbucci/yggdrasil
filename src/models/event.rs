use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "event_kind", rename_all = "snake_case")]
pub enum EventKind {
    LockAcquired,
    LockReleased,
    HookFired,
    TaskCreated,
    TaskStatusChanged,
    AgentStateChanged,
    Message,
    RunScheduled,
    RunClaimed,
    RunTerminal,
    RunRetry,
    SchedulerTick,
    SchedulerError,
    AgentStaleWarning,
}

impl EventKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::LockAcquired => "lock_acquired",
            Self::LockReleased => "lock_released",
            Self::HookFired => "hook_fired",
            Self::TaskCreated => "task_created",
            Self::TaskStatusChanged => "task_status",
            Self::AgentStateChanged => "agent_state",
            Self::Message => "message",
            Self::RunScheduled => "run_scheduled",
            Self::RunClaimed => "run_claimed",
            Self::RunTerminal => "run_terminal",
            Self::RunRetry => "run_retry",
            Self::SchedulerTick => "scheduler_tick",
            Self::SchedulerError => "scheduler_error",
            Self::AgentStaleWarning => "agent_stale_warning",
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Event {
    pub id: String,
    pub event_kind: EventKind,
    pub agent_id: Option<String>,
    pub agent_name: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Process-global CC session id, set once by the hook entry point. Preferred
/// over mutating `CLAUDE_SESSION_ID` in the environment, which is `unsafe` and
/// not thread-safe under edition 2024.
static SESSION_ID_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the CC session id for this process. Called from the hook handler
/// when Claude Code supplies `session_id` in the payload. First non-empty
/// value wins; a hook process handles exactly one session.
pub fn set_cc_session_id(session_id: &str) {
    if !session_id.is_empty() {
        let _ = SESSION_ID_OVERRIDE.set(session_id.to_string());
    }
}

/// Claude Code session id for the current process. Prefers the value the hook
/// entry recorded via [`set_cc_session_id`]; otherwise falls back to the
/// `CLAUDE_SESSION_ID` environment variable, which spawn/inject/digest inherit
/// from the shell that invoked them. Neither set => None, the column stays NULL.
pub fn cc_session_id() -> Option<String> {
    if let Some(sid) = SESSION_ID_OVERRIDE.get() {
        return Some(sid.clone());
    }
    std::env::var("CLAUDE_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

pub struct EventRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> EventRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    pub async fn emit(
        &self,
        kind: EventKind,
        agent_name: &str,
        agent_id: Option<String>,
        payload: serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        // Auto-tag with the ambient CC session id when the hook path set it.
        // Keeps every emit() callsite untouched.
        let cc_session_id = cc_session_id();
        sqlx::query(
            "INSERT INTO events (event_kind, agent_id, agent_name, payload, cc_session_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&kind)
        .bind(agent_id)
        .bind(agent_name)
        .bind(payload)
        .bind(cc_session_id)
        .execute(self.pool)
        .await?;
        // Wake the scheduler (if one is listening) — fire-and-forget.
        crate::bus::notify("events", kind.label());
        Ok(())
    }

    /// Fetch events newer than `since`, optionally filtered by agent name.
    pub async fn list_since(
        &self,
        since: DateTime<Utc>,
        agent_name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Event>, sqlx::Error> {
        if let Some(name) = agent_name {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events WHERE created_at > $1 AND agent_name = $2
                   ORDER BY created_at ASC LIMIT $3"#,
            )
            .bind(since)
            .bind(name)
            .bind(limit)
            .fetch_all(self.pool)
            .await
        } else {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events WHERE created_at > $1
                   ORDER BY created_at ASC LIMIT $2"#,
            )
            .bind(since)
            .bind(limit)
            .fetch_all(self.pool)
            .await
        }
    }

    /// Fetch the most recent N events (newest first, for initial display).
    pub async fn list_recent(
        &self,
        limit: i64,
        agent_name: Option<&str>,
    ) -> Result<Vec<Event>, sqlx::Error> {
        if let Some(name) = agent_name {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events WHERE agent_name = $1
                   ORDER BY created_at DESC LIMIT $2"#,
            )
            .bind(name)
            .bind(limit)
            .fetch_all(self.pool)
            .await
        } else {
            sqlx::query_as::<_, Event>(
                r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
                   FROM events ORDER BY created_at DESC LIMIT $1"#,
            )
            .bind(limit)
            .fetch_all(self.pool)
            .await
        }
    }
}
