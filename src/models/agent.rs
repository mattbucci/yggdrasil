use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, Clone, PartialEq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "agent_state", rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Planning,
    Executing,
    WaitingTool,
    ContextFlush,
    HumanOverride,
    Mediation,
    Error,
    Shutdown,
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Planning => write!(f, "planning"),
            Self::Executing => write!(f, "executing"),
            Self::WaitingTool => write!(f, "waiting_tool"),
            Self::ContextFlush => write!(f, "context_flush"),
            Self::HumanOverride => write!(f, "human_override"),
            Self::Mediation => write!(f, "mediation"),
            Self::Error => write!(f, "error"),
            Self::Shutdown => write!(f, "shutdown"),
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct AgentWorkflow {
    pub agent_id: String,
    pub agent_name: String,
    pub current_state: AgentState,
    pub context_tokens: i32,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(default)]
    pub persona: Option<String>,
}

impl AgentWorkflow {
    /// Display key — `name` when persona is unset, `name:persona` otherwise.
    /// Use when rendering to humans; prefer `(agent_name, persona)` for
    /// lookups and joins.
    pub fn display_name(&self) -> String {
        match &self.persona {
            Some(p) if !p.is_empty() => format!("{}:{p}", self.agent_name),
            _ => self.agent_name.clone(),
        }
    }
}

pub struct AgentRepo<'a> {
    pool: &'a DbPool,
    user_id: String,
}

impl<'a> AgentRepo<'a> {
    pub fn new(pool: &'a DbPool, user_id: &str) -> Self {
        Self {
            pool,
            user_id: user_id.to_string(),
        }
    }

    /// Register a new agent or return existing one by (user_id, name, persona).
    pub async fn register(&self, name: &str) -> Result<AgentWorkflow, sqlx::Error> {
        self.register_with_persona(name, None).await
    }

    pub async fn register_with_persona(
        &self,
        name: &str,
        persona: Option<&str>,
    ) -> Result<AgentWorkflow, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            INSERT INTO agents (agent_name, persona, user_id)
            VALUES ($1, $2, $3)
            ON CONFLICT (user_id, agent_name, COALESCE(persona, ''))
              DO UPDATE SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            RETURNING agent_id, agent_name, current_state,
                      context_tokens, metadata, created_at, updated_at, persona
            "#,
        )
        .bind(name)
        .bind(persona)
        .bind(&self.user_id)
        .fetch_one(self.pool)
        .await
    }

    /// Transition agent state with optimistic concurrency control.
    pub async fn transition(
        &self,
        agent_id: &str,
        from: AgentState,
        to: AgentState,
    ) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            UPDATE agents
            SET current_state = $3, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
            WHERE agent_id = $1 AND current_state = $2
            RETURNING agent_id, agent_name, current_state,
                      context_tokens, metadata, created_at, updated_at, persona
            "#,
        )
        .bind(agent_id)
        .bind(&from)
        .bind(&to)
        .fetch_optional(self.pool)
        .await
    }

    /// Set the agent's state unconditionally, optionally recording the tool
    /// it's waiting on. Hook-driven state updates can't guess the current
    /// state, so they use this rather than the OCC transition(). Emits an
    /// `agent_state_changed` event when the state actually changes so the
    /// dashboard timeline can render transitions.
    pub async fn force_state(
        &self,
        agent_id: &str,
        to: AgentState,
        last_tool: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let meta_patch = match last_tool {
            Some(t) => serde_json::json!({"last_tool": t}),
            None => serde_json::json!({"last_tool": null}),
        };
        // SQLite's RETURNING clause cannot contain subqueries, so read the
        // prior state first, then update. Single-writer CLI: the tiny window
        // between the two statements is harmless (worst case a duplicate
        // state-changed event).
        let row: Option<(AgentState, String)> =
            sqlx::query_as("SELECT current_state, agent_name FROM agents WHERE agent_id = $1")
                .bind(agent_id)
                .fetch_optional(self.pool)
                .await?;
        if row.is_some() {
            sqlx::query(
                r#"
                UPDATE agents
                   SET current_state = $2,
                       metadata = json_patch(metadata, $3),
                       updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
                 WHERE agent_id = $1
                "#,
            )
            .bind(agent_id)
            .bind(&to)
            .bind(meta_patch)
            .execute(self.pool)
            .await?;
        }

        if let Some((old, name)) = row
            && old != to
        {
            let payload = serde_json::json!({
                "from": old.to_string(),
                "to": to.to_string(),
                "tool": last_tool,
            });
            let _ = sqlx::query(
                    "INSERT INTO events (event_kind, agent_id, agent_name, payload, cc_session_id, user_id)
                     VALUES ('agent_state_changed', $1, $2, $3, $4, $5)",
                )
                .bind(agent_id)
                .bind(&name)
                .bind(payload)
                .bind(crate::models::event::cc_session_id())
                .bind(&self.user_id)
                .execute(self.pool)
                .await;
        }
        Ok(())
    }

    /// Get agent by ID.
    pub async fn get(&self, agent_id: &str) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents WHERE agent_id = $1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Get agent by name within the current user's namespace.
    pub async fn get_by_name(&self, name: &str) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents WHERE agent_name = $1 AND user_id = $2
            ORDER BY (persona IS NOT NULL), updated_at DESC
            LIMIT 1
            "#,
        )
        .bind(name)
        .bind(&self.user_id)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_name_persona(
        &self,
        name: &str,
        persona: Option<&str>,
    ) -> Result<Option<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE agent_name = $1 AND COALESCE(persona, '') = COALESCE($2, '') AND user_id = $3
            "#,
        )
        .bind(name)
        .bind(persona)
        .bind(&self.user_id)
        .fetch_optional(self.pool)
        .await
    }

    /// List live agents for the current user.
    pub async fn list(&self) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE archived_at IS NULL AND user_id = $1
            ORDER BY created_at
            "#,
        )
        .bind(&self.user_id)
        .fetch_all(self.pool)
        .await
    }

    /// Include archived agents for the current user.
    pub async fn list_all(&self) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents WHERE user_id = $1 ORDER BY created_at
            "#,
        )
        .bind(&self.user_id)
        .fetch_all(self.pool)
        .await
    }

    /// List live agents across ALL users — for `ygg status --all-users`.
    pub async fn list_all_users(&self) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE archived_at IS NULL
            ORDER BY created_at
            "#,
        )
        .fetch_all(self.pool)
        .await
    }

    pub async fn archive(&self, agent_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agents SET archived_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent_id = $1")
            .bind(agent_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn rename(&self, agent_id: &str, new_name: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agents SET agent_name = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent_id = $1")
            .bind(agent_id)
            .bind(new_name)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_orphan_candidates(
        &self,
        min_idle_secs: i64,
    ) -> Result<Vec<(String, String, String)>, sqlx::Error> {
        sqlx::query_as::<_, (String, String, String)>(
            r#"
            SELECT a.agent_id, a.agent_name, w.worktree_path, MAX(w.started_at)
            FROM agents a
            JOIN tasks t   ON t.assignee = a.agent_id
            JOIN workers w ON w.task_id = t.task_id
            WHERE a.archived_at IS NULL
              AND a.user_id = $2
              AND w.worktree_path <> ''
              AND a.updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' seconds')
            GROUP BY a.agent_id
            ORDER BY a.agent_id
            "#,
        )
        .bind(min_idle_secs)
        .bind(&self.user_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn unarchive(&self, agent_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE agents SET archived_at = NULL WHERE agent_id = $1")
            .bind(agent_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn find_stale(&self, days: i64) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT a.agent_id, a.agent_name, a.current_state,
                   a.context_tokens, a.metadata, a.created_at, a.updated_at, a.persona
            FROM agents a
            WHERE a.archived_at IS NULL
              AND a.user_id = $2
              AND a.updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' days')
              AND NOT EXISTS (
                    SELECT 1 FROM events e
                     WHERE e.agent_id = a.agent_id
                       AND e.created_at >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' days')
              )
              AND NOT EXISTS (
                    SELECT 1 FROM sessions s
                     WHERE s.agent_id = a.agent_id
                       AND COALESCE(s.ended_at, s.started_at)
                            >= strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' days')
              )
              AND NOT EXISTS (
                    SELECT 1 FROM locks l
                     WHERE l.agent_id = a.agent_id AND l.expires_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
              )
            ORDER BY a.updated_at
            "#,
        )
        .bind(days.to_string())
        .bind(&self.user_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn find_orphaned(&self, stale_secs: i64) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
            FROM agents
            WHERE current_state IN ('executing', 'waiting_tool', 'planning', 'context_flush')
              AND updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' seconds')
              AND user_id = $2
            ORDER BY updated_at
            "#,
        )
        .bind(stale_secs as f64)
        .bind(&self.user_id)
        .fetch_all(self.pool)
        .await
    }

    /// Reset an orphaned agent to Idle for resume.
    pub async fn reset_to_idle(&self, agent_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE agents SET current_state = 'idle', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent_id = $1",
        )
        .bind(agent_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Candidates for the watcher's zombie reap. Returns agents whose state
    /// hasn't been touched in `min_idle_secs` and which aren't already in a
    /// terminal state (Shutdown / HumanOverride / Error). Cross-user safe
    /// because the watcher reaps on behalf of every spawn-owner.
    pub async fn list_reap_candidates(
        &self,
        min_idle_secs: i64,
    ) -> Result<Vec<AgentWorkflow>, sqlx::Error> {
        sqlx::query_as::<_, AgentWorkflow>(
            r#"
            SELECT agent_id, agent_name, current_state,
                   context_tokens, metadata, created_at, updated_at, persona
              FROM agents
             WHERE archived_at IS NULL
               AND current_state NOT IN ('shutdown', 'human_override', 'error')
               AND updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' seconds')
            "#,
        )
        .bind(min_idle_secs as f64)
        .fetch_all(self.pool)
        .await
    }
}
