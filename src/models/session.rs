use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

use crate::models::agent::AgentState;

/// Resolve the current CC session row for an agent, upserting if the hook
/// path gave us a CLAUDE_SESSION_ID. Returns None when we're outside a CC
/// session (e.g. running `ygg task list` in a plain shell) — callers should
/// treat that as "don't bother updating session state".
pub async fn resolve_current_session(
    pool: &DbPool,
    agent_id: &str,
    repo_id: Option<String>,
) -> Option<String> {
    let cc_sid = crate::models::event::cc_session_id()?;
    let session = SessionRepo::new(pool)
        .ensure(agent_id, repo_id, Some(&cc_sid))
        .await
        .ok()?;
    Some(session.session_id)
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Session {
    pub session_id: String,
    pub agent_id: String,
    pub repo_id: Option<String>,
    pub cc_session_id: Option<String>,
    pub current_state: AgentState,
    pub context_tokens: i32,
    pub last_tool: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub metadata: serde_json::Value,
}

pub struct SessionRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> SessionRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Upsert a session keyed by CC's session_id. Returns an existing live
    /// session when the cc_session_id is already known — otherwise starts a
    /// fresh row. Without a cc_session_id we fall back to starting a new row
    /// (legacy callers without env-propagated session id).
    pub async fn ensure(
        &self,
        agent_id: &str,
        repo_id: Option<String>,
        cc_session_id: Option<&str>,
    ) -> Result<Session, sqlx::Error> {
        if let Some(sid) = cc_session_id {
            // Fast path: session already exists.
            if let Some(existing) = sqlx::query_as::<_, Session>(
                r#"SELECT session_id, agent_id, repo_id, cc_session_id,
                          current_state, context_tokens,
                          last_tool, started_at, ended_at, updated_at, metadata
                   FROM sessions WHERE cc_session_id = $1 LIMIT 1"#,
            )
            .bind(sid)
            .fetch_optional(self.pool)
            .await?
            {
                return Ok(existing);
            }
            // Otherwise INSERT with ON CONFLICT to handle the race where two
            // hooks fire the insert at once.
            sqlx::query_as::<_, Session>(
                r#"INSERT INTO sessions (agent_id, repo_id, cc_session_id)
                   VALUES ($1, $2, $3)
                   ON CONFLICT (cc_session_id) DO UPDATE SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                   RETURNING session_id, agent_id, repo_id, cc_session_id,
                             current_state, context_tokens,
                             last_tool, started_at, ended_at, updated_at, metadata"#,
            )
            .bind(agent_id)
            .bind(repo_id)
            .bind(sid)
            .fetch_one(self.pool)
            .await
        } else {
            // No CC session id available — start an anonymous session row.
            sqlx::query_as::<_, Session>(
                r#"INSERT INTO sessions (agent_id, repo_id)
                   VALUES ($1, $2)
                   RETURNING session_id, agent_id, repo_id, cc_session_id,
                             current_state, context_tokens,
                             last_tool, started_at, ended_at, updated_at, metadata"#,
            )
            .bind(agent_id)
            .bind(repo_id)
            .fetch_one(self.pool)
            .await
        }
    }

    /// Set session state unconditionally with optional tool metadata.
    /// Mirrors AgentRepo::force_state but targets the per-session row so
    /// parallel sessions don't stomp each other.
    pub async fn force_state(
        &self,
        session_id: &str,
        to: AgentState,
        last_tool: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE sessions
                  SET current_state = $2,
                      last_tool = $3,
                      updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                WHERE session_id = $1"#,
        )
        .bind(session_id)
        .bind(&to)
        .bind(last_tool)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn end(&self, session_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE sessions SET ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE session_id = $1 AND ended_at IS NULL",
        )
        .bind(session_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// End every still-open session for an agent. Used by the watcher's
    /// zombie reap when the Claude process is gone but its sessions row
    /// never reached the Stop hook.
    pub async fn end_all_for_agent(&self, agent_id: &str) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE sessions SET ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
              WHERE agent_id = $1 AND ended_at IS NULL",
        )
        .bind(agent_id)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn latest_for_agent(&self, agent_id: &str) -> Result<Option<Session>, sqlx::Error> {
        sqlx::query_as::<_, Session>(
            r#"SELECT session_id, agent_id, repo_id, cc_session_id,
                      current_state, context_tokens,
                      last_tool, started_at, ended_at, updated_at, metadata
               FROM sessions
               WHERE agent_id = $1
               ORDER BY started_at DESC
               LIMIT 1"#,
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Count live sessions (no ended_at) per agent. Used by the dashboard to
    /// show "N active" next to the agent name.
    pub async fn live_counts(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sqlx::query_as::<_, (String, i64)>(
            "SELECT agent_id, COUNT(*)
               FROM sessions
              WHERE ended_at IS NULL
              GROUP BY agent_id",
        )
        .fetch_all(self.pool)
        .await
    }
}
