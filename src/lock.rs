use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("resource '{resource_key}' locked by agent {holder_agent_id} until {expires_at}")]
    AlreadyHeld {
        resource_key: String,
        holder_agent_id: String,
        expires_at: DateTime<Utc>,
    },

    #[error("lock not found: {0}")]
    NotFound(String),

    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ResourceLock {
    pub id: String,
    pub resource_key: String,
    pub agent_id: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
}

pub struct LockManager<'a> {
    pool: &'a DbPool,
    ttl_secs: u64,
    user_id: String,
}

impl<'a> LockManager<'a> {
    pub fn new(pool: &'a DbPool, ttl_secs: u64, user_id: &str) -> Self {
        Self {
            pool,
            ttl_secs,
            user_id: user_id.to_string(),
        }
    }

    /// Try to acquire a lock. Returns the lock on success, or info about the holder on conflict.
    pub async fn acquire(
        &self,
        resource_key: &str,
        agent_id: &str,
    ) -> Result<ResourceLock, LockError> {
        // Postgres used a data-modifying CTE (reap + insert in one
        // statement); SQLite doesn't allow DML in CTEs, so take the write
        // lock up front with BEGIN IMMEDIATE and run the two statements
        // atomically. busy_timeout + WAL absorb concurrent hook traffic.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(LockError::Db)?;
        sqlx::query(
            "DELETE FROM locks WHERE resource_key = $1
              AND expires_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now')",
        )
        .bind(resource_key)
        .execute(&mut *tx)
        .await?;
        let row: Option<ResourceLock> = sqlx::query_as::<_, ResourceLock>(
            r#"
            INSERT INTO locks (resource_key, agent_id, expires_at, user_id)
            VALUES ($1, $2, strftime('%Y-%m-%dT%H:%M:%f+00:00','now','+' || $3 || ' seconds'), $4)
            ON CONFLICT (resource_key) DO NOTHING
            RETURNING id, resource_key, agent_id, acquired_at, expires_at, heartbeat_at
            "#,
        )
        .bind(resource_key)
        .bind(agent_id)
        .bind(self.ttl_secs as f64)
        .bind(&self.user_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await.map_err(LockError::Db)?;

        match row {
            Some(lock) => Ok(lock),
            None => {
                let holder: ResourceLock = sqlx::query_as::<_, ResourceLock>(
                    "SELECT id, resource_key, agent_id, acquired_at, expires_at, heartbeat_at FROM locks WHERE resource_key = $1",
                )
                .bind(resource_key)
                .fetch_one(self.pool)
                .await?;

                Err(LockError::AlreadyHeld {
                    resource_key: resource_key.to_string(),
                    holder_agent_id: holder.agent_id,
                    expires_at: holder.expires_at,
                })
            }
        }
    }

    /// Release a lock.
    pub async fn release(&self, resource_key: &str, agent_id: &str) -> Result<(), LockError> {
        let result = sqlx::query("DELETE FROM locks WHERE resource_key = $1 AND agent_id = $2")
            .bind(resource_key)
            .bind(agent_id)
            .execute(self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(LockError::NotFound(resource_key.to_string()));
        }
        Ok(())
    }

    /// Heartbeat: extend the lease TTL.
    pub async fn heartbeat(&self, resource_key: &str, agent_id: &str) -> Result<(), LockError> {
        let result = sqlx::query(
            r#"
            UPDATE locks
            SET heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now','+' || $3 || ' seconds')
            WHERE resource_key = $1 AND agent_id = $2
            "#,
        )
        .bind(resource_key)
        .bind(agent_id)
        .bind(self.ttl_secs as f64)
        .execute(self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(LockError::NotFound(resource_key.to_string()));
        }
        Ok(())
    }

    /// Reap all expired locks. Returns count reaped.
    pub async fn reap_expired(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM locks WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now')",
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// List active locks for the current user.
    pub async fn list_all(&self) -> Result<Vec<ResourceLock>, sqlx::Error> {
        sqlx::query_as::<_, ResourceLock>(
            "SELECT id, resource_key, agent_id, acquired_at, expires_at, heartbeat_at FROM locks WHERE user_id = $1 ORDER BY acquired_at",
        )
        .bind(&self.user_id)
        .fetch_all(self.pool)
        .await
    }

    /// List active locks across ALL users.
    pub async fn list_all_users(&self) -> Result<Vec<ResourceLock>, sqlx::Error> {
        sqlx::query_as::<_, ResourceLock>(
            "SELECT id, resource_key, agent_id, acquired_at, expires_at, heartbeat_at FROM locks ORDER BY acquired_at",
        )
        .fetch_all(self.pool)
        .await
    }

    /// List locks held by a specific agent.
    pub async fn list_agent_locks(&self, agent_id: &str) -> Result<Vec<ResourceLock>, sqlx::Error> {
        sqlx::query_as::<_, ResourceLock>(
            "SELECT id, resource_key, agent_id, acquired_at, expires_at, heartbeat_at FROM locks WHERE agent_id = $1",
        )
        .bind(agent_id)
        .fetch_all(self.pool)
        .await
    }

    /// Release every lock held by an agent.
    pub async fn release_all_for_agent(&self, agent_id: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM locks WHERE agent_id = $1")
            .bind(agent_id)
            .execute(self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}
