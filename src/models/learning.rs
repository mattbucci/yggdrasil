//! Scoped learnings — durable rule capture. Key property:
//! retrieval is deterministic (SQL predicates on repo_id + file_glob +
//! rule_id) rather than vector-similarity. A learning scoped to
//! `terraform/*.tf` with rule_id `CKV_AWS_337` surfaces exactly when a
//! task touches `terraform/secrets.tf` and references that check — no
//! cosine, no threshold tuning, no false-positive noise.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Learning {
    pub learning_id: String,
    pub repo_id: Option<String>,
    pub file_glob: Option<String>,
    pub rule_id: Option<String>,
    pub text: String,
    pub context: Option<String>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub applied_count: i32,
    pub last_applied_at: Option<DateTime<Utc>>,
    pub scope_tags: serde_json::Value,
    /// `pending` | `active`. Only `active` learnings are ever surfaced
    /// (ADR 0017). `pending` rows exist but fire on nothing until promoted.
    pub status: String,
    /// `manual` (hand-written via `create`) | `proposed` (agent-authored via
    /// `propose`). Records how the learning entered the corpus.
    pub source: String,
    pub approved_at: Option<DateTime<Utc>>,
    pub approved_by: Option<String>,
}

pub struct LearningRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> LearningRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Insert a learning. `status` is `pending` or `active`; `source` is
    /// `manual` or `proposed` (ADR 0017). Callers that want today's
    /// immediately-firing behavior pass `("active", "manual")`.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        repo_id: Option<String>,
        file_glob: Option<&str>,
        rule_id: Option<&str>,
        text: &str,
        context: Option<&str>,
        created_by: Option<String>,
        scope_tags: &serde_json::Value,
        status: &str,
        source: &str,
    ) -> Result<Learning, sqlx::Error> {
        sqlx::query_as::<_, Learning>(
            r#"INSERT INTO learnings (repo_id, file_glob, rule_id, text, context, created_by, scope_tags, status, source)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
               RETURNING learning_id, repo_id, file_glob, rule_id, text, context,
                         created_by, created_at, applied_count, last_applied_at, scope_tags,
                         status, source, approved_at, approved_by"#,
        )
        .bind(repo_id)
        .bind(file_glob)
        .bind(rule_id)
        .bind(text)
        .bind(context)
        .bind(created_by)
        .bind(scope_tags)
        .bind(status)
        .bind(source)
        .fetch_one(self.pool)
        .await
    }

    /// List learnings whose scope matches the filters. NULL filters act as
    /// "any"; a learning with `file_glob IS NULL` counts as "applies to any
    /// file in this repo" and is always included when `repo_id` matches
    /// (or when the learning itself is global).
    ///
    /// `agent_name` / `task_kind`: when non-None, exclude learnings whose
    /// scope_tags constrain that dimension to a *different* value. Learnings
    /// with no constraint on that dimension (key absent or null) always pass.
    pub async fn list_matching(
        &self,
        repo_id: Option<String>,
        file_path: Option<&str>,
        rule_id: Option<&str>,
        agent_name: Option<&str>,
        task_kind: Option<&str>,
    ) -> Result<Vec<Learning>, sqlx::Error> {
        sqlx::query_as::<_, Learning>(
            r#"
            SELECT learning_id, repo_id, file_glob, rule_id, text, context,
                   created_by, created_at, applied_count, last_applied_at, scope_tags,
                   status, source, approved_at, approved_by
            FROM learnings
            WHERE status = 'active'
              AND ($1 IS NULL OR repo_id IS NULL OR repo_id = $1)
              AND ($2 IS NULL
                   OR file_glob IS NULL
                   OR $2 LIKE replace(replace(file_glob, '*', '%'), '?', '_'))
              AND ($3 IS NULL OR rule_id = $3)
              AND (scope_tags->>'agent' IS NULL
                   OR $4 IS NULL
                   OR scope_tags->>'agent' = $4)
              AND (scope_tags->>'kind' IS NULL
                   OR $5 IS NULL
                   OR scope_tags->>'kind' = $5)
            ORDER BY
              (rule_id IS NOT NULL) + (file_glob IS NOT NULL) DESC,
              created_at DESC
            "#,
        )
        .bind(repo_id)
        .bind(file_path)
        .bind(rule_id)
        .bind(agent_name)
        .bind(task_kind)
        .fetch_all(self.pool)
        .await
    }

    /// List `pending` learnings (the triage queue), newest first. `repo_id`
    /// NULL lists across all repos; otherwise current-repo + global pending.
    pub async fn list_pending(
        &self,
        repo_id: Option<String>,
    ) -> Result<Vec<Learning>, sqlx::Error> {
        sqlx::query_as::<_, Learning>(
            r#"
            SELECT learning_id, repo_id, file_glob, rule_id, text, context,
                   created_by, created_at, applied_count, last_applied_at, scope_tags,
                   status, source, approved_at, approved_by
            FROM learnings
            WHERE status = 'pending'
              AND ($1 IS NULL OR repo_id IS NULL OR repo_id = $1)
            ORDER BY created_at DESC
            "#,
        )
        .bind(repo_id)
        .fetch_all(self.pool)
        .await
    }

    /// Promote a `pending` learning to `active`, stamping approval metadata.
    /// Returns the updated row, or None if no pending learning has that id
    /// (already active, rejected, or unknown).
    pub async fn approve(
        &self,
        learning_id: String,
        approved_by: Option<String>,
    ) -> Result<Option<Learning>, sqlx::Error> {
        sqlx::query_as::<_, Learning>(
            r#"
            UPDATE learnings
            SET status = 'active', approved_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), approved_by = $2
            WHERE learning_id = $1 AND status = 'pending'
            RETURNING learning_id, repo_id, file_glob, rule_id, text, context,
                      created_by, created_at, applied_count, last_applied_at, scope_tags,
                      status, source, approved_at, approved_by
            "#,
        )
        .bind(learning_id)
        .bind(approved_by)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn increment_applied(&self, learning_id: String) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE learnings SET applied_count = applied_count + 1, last_applied_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE learning_id = $1",
        )
        .bind(learning_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete(&self, learning_id: String) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM learnings WHERE learning_id = $1")
            .bind(learning_id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Reject a `pending` proposal: hard-delete it (ADR 0017 — tombstone vs
    /// delete deferred to implementation; we hard-delete). Returns true if a
    /// pending row was removed, false if none matched (active rows untouched).
    pub async fn reject(&self, learning_id: String) -> Result<bool, sqlx::Error> {
        let res =
            sqlx::query("DELETE FROM learnings WHERE learning_id = $1 AND status = 'pending'")
                .bind(learning_id)
                .execute(self.pool)
                .await?;
        Ok(res.rows_affected() > 0)
    }
}
