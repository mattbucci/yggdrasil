//! Worker: a spawned CC session executing a specific task in a worktree.
//! Differs from `sessions`: a session is any CC conversation under an
//! agent; a worker is specifically a task-spawned, tmux-hosted one that
//! the supervisor or TUI kicked off via `ygg plan run`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, Serialize, Deserialize)]
#[sqlx(type_name = "worker_state", rename_all = "snake_case")]
pub enum WorkerState {
    Spawned,
    Running,
    Idle,
    NeedsAttention,
    Completed,
    Failed,
    Abandoned,
}

impl WorkerState {
    pub fn glyph_color(&self) -> (&'static str, &'static str) {
        match self {
            Self::Spawned => ("◌", "dark_gray"),
            Self::Running => ("▶", "green"),
            Self::Idle => ("•", "gray"),
            Self::NeedsAttention => ("⚠", "yellow"),
            Self::Completed => ("✓", "dark_gray"),
            Self::Failed => ("✗", "red"),
            Self::Abandoned => ("⊘", "dark_gray"),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Worker {
    pub worker_id: String,
    pub task_id: String,
    pub session_id: Option<String>,
    pub tmux_session: String,
    pub tmux_window: String,
    pub worktree_path: String,
    pub state: WorkerState,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub exit_reason: Option<String>,
    #[sqlx(default)]
    pub branch_pushed: bool,
    #[sqlx(default)]
    pub branch_merged: bool,
    #[sqlx(default)]
    pub pr_url: Option<String>,
    #[sqlx(default)]
    pub delivery_checked_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub intent: Option<String>,
}

pub struct WorkerRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> WorkerRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Register a freshly-spawned worker. Caller provides the tmux target
    /// strings it just created; we keep the exact window name so the
    /// observer can cross-check against `tmux list-windows`.
    pub async fn spawn(
        &self,
        task_id: String,
        session_id: Option<String>,
        tmux_session: &str,
        tmux_window: &str,
        worktree_path: &str,
    ) -> Result<Worker, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"
            INSERT INTO workers
                (task_id, session_id, tmux_session, tmux_window, worktree_path, state)
            VALUES ($1, $2, $3, $4, $5, 'spawned')
            RETURNING worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason,
                      branch_pushed, branch_merged, pr_url, delivery_checked_at, intent
            "#,
        )
        .bind(task_id)
        .bind(session_id)
        .bind(tmux_session)
        .bind(tmux_window)
        .bind(worktree_path)
        .fetch_one(self.pool)
        .await
    }

    pub async fn touch(&self, worker_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workers SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE worker_id = $1")
            .bind(worker_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Record a delivery check: whether the branch is pushed, whether it
    /// merged into the repo's main branch, and the PR url if found.
    pub async fn set_delivery(
        &self,
        worker_id: &str,
        branch_pushed: bool,
        branch_merged: bool,
        pr_url: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE workers
               SET branch_pushed = $2,
                   branch_merged = $3,
                   pr_url        = COALESCE($4, pr_url),
                   delivery_checked_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
             WHERE worker_id = $1
            "#,
        )
        .bind(worker_id)
        .bind(branch_pushed)
        .bind(branch_merged)
        .bind(pr_url)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_state(
        &self,
        worker_id: &str,
        state: WorkerState,
        exit_reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let terminal = matches!(
            state,
            WorkerState::Completed | WorkerState::Failed | WorkerState::Abandoned
        );
        sqlx::query(
            r#"
            UPDATE workers
               SET state = $2,
                   last_seen_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),
                   ended_at = CASE WHEN $3 AND ended_at IS NULL THEN strftime('%Y-%m-%dT%H:%M:%f+00:00','now') ELSE ended_at END,
                   exit_reason = COALESCE($4, exit_reason)
             WHERE worker_id = $1
            "#,
        )
        .bind(worker_id)
        .bind(state)
        .bind(terminal)
        .bind(exit_reason)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// All workers whose tmux window should still exist. Used by the
    /// observer and the dashboard Workers panel.
    /// Workers the dashboard should keep visible: alive OR recently ended
    /// but undelivered (completed without push/merge). Lets the user see
    /// "worker finished, branch still local" until they act on it.
    pub async fn list_visible(&self) -> Result<Vec<Worker>, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason,
                      branch_pushed, branch_merged, pr_url, delivery_checked_at, intent
                 FROM workers
                WHERE ended_at IS NULL
                   OR (ended_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-24 hours')
                       AND (branch_pushed = false OR branch_merged = false))
                ORDER BY (ended_at IS NULL) DESC, started_at DESC
                LIMIT 15"#,
        )
        .fetch_all(self.pool)
        .await
    }

    pub async fn list_live(&self) -> Result<Vec<Worker>, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason,
                      branch_pushed, branch_merged, pr_url, delivery_checked_at, intent
                 FROM workers
                WHERE ended_at IS NULL
                ORDER BY started_at DESC"#,
        )
        .fetch_all(self.pool)
        .await
    }

    pub async fn set_intent(
        &self,
        worker_id: &str,
        intent: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workers SET intent = $2 WHERE worker_id = $1")
            .bind(worker_id)
            .bind(intent)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_cleanable(&self) -> Result<Vec<Worker>, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason,
                      branch_pushed, branch_merged, pr_url, delivery_checked_at, intent
                 FROM workers
                WHERE (state IN ('completed', 'failed') AND branch_merged = true)
                   OR (state = 'abandoned' AND ended_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-1 hour'))
                ORDER BY ended_at ASC
                LIMIT 10"#,
        )
        .fetch_all(self.pool)
        .await
    }

    pub async fn get(&self, worker_id: &str) -> Result<Option<Worker>, sqlx::Error> {
        sqlx::query_as::<_, Worker>(
            r#"SELECT worker_id, task_id, session_id, tmux_session, tmux_window,
                      worktree_path, state, started_at, last_seen_at, ended_at, exit_reason,
                      branch_pushed, branch_merged, pr_url, delivery_checked_at, intent
                 FROM workers WHERE worker_id = $1"#,
        )
        .bind(worker_id)
        .fetch_optional(self.pool)
        .await
    }
}
