//! Durable notes for `ygg remember` (re-added post-ADR-0015). Unlike the
//! removed `nodes` corpus, these carry NO embeddings: retrieval is plain SQL
//! ordered by recency, scoped to (repo_id, NULL=global). Recent notes surface
//! in `ygg prime` and `ygg remember --list` — never via similarity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Memory {
    pub memory_id: Uuid,
    pub repo_id: Option<Uuid>,
    pub text: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

pub struct MemoryRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> MemoryRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        repo_id: Option<Uuid>,
        text: &str,
        created_by: Option<Uuid>,
    ) -> Result<Memory, sqlx::Error> {
        sqlx::query_as::<_, Memory>(
            r#"INSERT INTO memories (repo_id, text, created_by)
               VALUES ($1, $2, $3)
               RETURNING memory_id, repo_id, text, created_by, created_at"#,
        )
        .bind(repo_id)
        .bind(text)
        .bind(created_by)
        .fetch_one(self.pool)
        .await
    }

    /// List notes newest-first. `repo_id = Some` returns that repo's notes plus
    /// global (repo_id IS NULL) ones; `repo_id = None` with `all = true` returns
    /// every note; `repo_id = None` with `all = false` returns global-only.
    pub async fn list(
        &self,
        repo_id: Option<Uuid>,
        all: bool,
        limit: i64,
    ) -> Result<Vec<Memory>, sqlx::Error> {
        sqlx::query_as::<_, Memory>(
            r#"SELECT memory_id, repo_id, text, created_by, created_at
               FROM memories
               WHERE ($2::bool IS TRUE)
                  OR ($1::UUID IS NULL AND repo_id IS NULL)
                  OR (repo_id = $1 OR repo_id IS NULL)
               ORDER BY created_at DESC
               LIMIT $3"#,
        )
        .bind(repo_id)
        .bind(all)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    pub async fn delete(&self, memory_id: Uuid) -> Result<bool, sqlx::Error> {
        let n = sqlx::query("DELETE FROM memories WHERE memory_id = $1")
            .bind(memory_id)
            .execute(self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }
}
