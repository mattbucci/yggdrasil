//! `ygg scheduler` — daemon CLI. Dispatches `ygg scheduler run` to the
//! foreground loop in `src/scheduler.rs`. See docs/design/scheduler.md.

use crate::config::AppConfig;
use crate::scheduler::{self, SchedulerConfig};
use sqlx::PgPool;

/// `ygg scheduler run` — start the daemon. Holds the singleton advisory lock;
/// second instance exits cleanly with a visible error.
pub async fn run(pool: PgPool, app_cfg: &AppConfig) -> Result<(), anyhow::Error> {
    let cfg = SchedulerConfig::from_app(app_cfg);
    scheduler::run(pool, cfg).await
}

/// `ygg scheduler tick` — run one tick synchronously. For testing without
/// spinning up the daemon. Prints the stats to stdout in JSON for scriptability.
pub async fn tick(pool: &PgPool, app_cfg: &AppConfig) -> Result<(), anyhow::Error> {
    let cfg = SchedulerConfig::from_app(app_cfg);
    let stats = scheduler::tick(pool, &cfg).await?;
    println!("{}", serde_json::to_string(&stats)?);
    Ok(())
}

/// `ygg scheduler status` — print "is the singleton lock held? what's queued?".
pub async fn status(pool: &PgPool) -> Result<(), anyhow::Error> {
    // Probe by trying to acquire the advisory lock. If we get it, no
    // scheduler is currently running on this DB. We release immediately.
    let lock_id: i64 = 0x4347_4753_4348;
    let mut conn = pool.acquire().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(lock_id)
        .fetch_one(&mut *conn)
        .await?;
    if acquired {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(lock_id)
            .execute(&mut *conn)
            .await?;
        println!("scheduler: not running");
    } else {
        println!("scheduler: running (advisory lock held)");
    }

    let scheduled: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_runs WHERE state = 'scheduled'")
            .fetch_one(pool)
            .await?;
    let ready: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_runs WHERE state = 'ready'")
        .fetch_one(pool)
        .await?;
    let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_runs WHERE state = 'running'")
        .fetch_one(pool)
        .await?;
    let runnable: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tasks WHERE runnable = TRUE AND status IN ('open','in_progress')",
    )
    .fetch_one(pool)
    .await?;

    println!("queue:");
    println!("  runnable tasks (not yet scheduled): {runnable}");
    println!("  scheduled runs:                     {scheduled}");
    println!("  ready runs:                         {ready}");
    println!("  running runs:                       {running}");
    Ok(())
}

/// `ygg scheduler dry-run` — print what `tick` would do without writing.
/// Stage 1 implementation: shows what's runnable and what would be claimed.
pub async fn dry_run(pool: &PgPool) -> Result<(), anyhow::Error> {
    let runnable: Vec<(String, String, i32, i16)> = sqlx::query_as(
        r#"SELECT t.title, r.task_prefix, t.seq, t.priority
             FROM tasks t JOIN repos r USING (repo_id)
            WHERE t.runnable = TRUE
              AND t.status IN ('open', 'in_progress')
              AND t.current_attempt_id IS NULL
              AND NOT EXISTS (
                  SELECT 1 FROM task_deps d
                  JOIN tasks b ON b.task_id = d.blocker_id
                   WHERE d.task_id = t.task_id AND b.status <> 'closed'
              )
              AND (t.approval_level = 'auto' OR t.approved_at IS NOT NULL)
            ORDER BY t.priority, t.updated_at
            LIMIT 50"#,
    )
    .fetch_all(pool)
    .await?;

    if runnable.is_empty() {
        println!("dry-run: nothing eligible to schedule");
        return Ok(());
    }
    println!("dry-run: would schedule + dispatch:");
    for (title, prefix, seq, priority) in runnable {
        println!("  {prefix}-{seq:<4} P{priority}  {title}");
    }
    Ok(())
}
