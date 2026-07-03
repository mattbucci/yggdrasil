//! Shared integration-test harness: every test gets its own throwaway
//! SQLite database (schema migrated), so tests run in parallel with no
//! cross-test interference and no external services. Set DATABASE_URL to
//! point tests at an existing database instead (legacy behavior).
#![allow(dead_code)]

use ygg::db::DbPool;

/// A per-test database. Keeps the backing temp directory alive for the
/// pool's lifetime; dropping this removes the database file.
pub struct TestDb {
    pub pool: DbPool,
    _dir: Option<tempfile::TempDir>,
}

impl std::ops::Deref for TestDb {
    type Target = DbPool;
    fn deref(&self) -> &DbPool {
        &self.pool
    }
}

/// Open a fresh, fully-migrated database for one test.
/// Honors DATABASE_URL when set (points at a shared/persistent DB);
/// otherwise creates an isolated temp file.
pub async fn test_db() -> TestDb {
    if let Ok(url) = std::env::var("DATABASE_URL")
        && !url.is_empty()
    {
        let pool = ygg::db::create_pool(&url).await.expect("connect db");
        ygg::db::run_migrations(&pool).await.expect("migrations");
        return TestDb { pool, _dir: None };
    }
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("ygg-test.db");
    let pool = ygg::db::create_pool(path.to_str().expect("utf8 path"))
        .await
        .expect("create sqlite pool");
    ygg::db::run_migrations(&pool).await.expect("migrations");
    TestDb {
        pool,
        _dir: Some(dir),
    }
}
