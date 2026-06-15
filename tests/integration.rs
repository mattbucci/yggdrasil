use std::env;

/// Integration tests — require a running Postgres with migrations applied.
/// Run with: DATABASE_URL=postgres://ngj49@localhost:5432/ygg cargo test -- --test-threads=1

#[tokio::test]
async fn test_agent_lifecycle() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let agent_repo = ygg::models::agent::AgentRepo::new(&pool, "test");

    // Register
    let agent = agent_repo.register("test-agent-lifecycle").await.unwrap();
    assert_eq!(agent.agent_name, "test-agent-lifecycle");
    assert_eq!(agent.current_state, ygg::models::agent::AgentState::Idle);

    // Transition idle → executing
    let updated = agent_repo
        .transition(
            agent.agent_id,
            ygg::models::agent::AgentState::Idle,
            ygg::models::agent::AgentState::Executing,
        )
        .await
        .unwrap();
    assert!(updated.is_some());
    assert_eq!(
        updated.unwrap().current_state,
        ygg::models::agent::AgentState::Executing
    );

    // Invalid transition — executing → idle (should work)
    let back = agent_repo
        .transition(
            agent.agent_id,
            ygg::models::agent::AgentState::Executing,
            ygg::models::agent::AgentState::Idle,
        )
        .await
        .unwrap();
    assert!(back.is_some());

    // Wrong current state — should return None
    let bad = agent_repo
        .transition(
            agent.agent_id,
            ygg::models::agent::AgentState::Executing,
            ygg::models::agent::AgentState::Shutdown,
        )
        .await
        .unwrap();
    assert!(bad.is_none()); // already idle, not executing

    // Cleanup
    sqlx::query("DELETE FROM agents WHERE agent_name = 'test-agent-lifecycle'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_lock_acquire_release() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let lock_mgr = ygg::lock::LockManager::new(&pool, 300, "test");
    let agent_id = uuid::Uuid::new_v4();

    // Acquire
    let lock = lock_mgr.acquire("test:resource:1", agent_id).await.unwrap();
    assert_eq!(lock.resource_key, "test:resource:1");

    // Double acquire by same agent — should conflict (different row)
    let agent2 = uuid::Uuid::new_v4();
    let conflict = lock_mgr.acquire("test:resource:1", agent2).await;
    assert!(conflict.is_err());

    // Release
    lock_mgr.release("test:resource:1", agent_id).await.unwrap();

    // Now agent2 can acquire
    let lock2 = lock_mgr.acquire("test:resource:1", agent2).await.unwrap();
    assert_eq!(lock2.agent_id, agent2);

    // Cleanup
    lock_mgr.release("test:resource:1", agent2).await.unwrap();
}

#[tokio::test]
async fn test_lock_atomic_no_toctou() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let lock_mgr = ygg::lock::LockManager::new(&pool, 300, "test");
    let a1 = uuid::Uuid::new_v4();
    let a2 = uuid::Uuid::new_v4();

    // Both try to acquire simultaneously
    let (r1, r2) = tokio::join!(
        lock_mgr.acquire("test:atomic:1", a1),
        lock_mgr.acquire("test:atomic:1", a2),
    );

    // Exactly one should succeed
    let (ok_count, err_count) = (
        r1.is_ok() as u32 + r2.is_ok() as u32,
        r1.is_err() as u32 + r2.is_err() as u32,
    );
    assert_eq!(ok_count, 1);
    assert_eq!(err_count, 1);

    // Cleanup
    let winner = if r1.is_ok() { a1 } else { a2 };
    lock_mgr.release("test:atomic:1", winner).await.unwrap();
}

#[tokio::test]
async fn test_crash_recovery() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let agent_repo = ygg::models::agent::AgentRepo::new(&pool, "test");

    // Create agent stuck in executing
    let agent = agent_repo.register("test-crash-recovery").await.unwrap();
    agent_repo
        .transition(
            agent.agent_id,
            ygg::models::agent::AgentState::Idle,
            ygg::models::agent::AgentState::Executing,
        )
        .await
        .unwrap();

    // Make it stale by backdating updated_at
    sqlx::query("UPDATE agents SET updated_at = now() - interval '1 hour' WHERE agent_name = 'test-crash-recovery'")
        .execute(&pool).await.unwrap();

    // Find orphaned (stale > 60s)
    let orphaned = agent_repo.find_orphaned(60).await.unwrap();
    assert!(
        orphaned
            .iter()
            .any(|a| a.agent_name == "test-crash-recovery")
    );

    // Reset
    agent_repo.reset_to_idle(agent.agent_id).await.unwrap();
    let recovered = agent_repo.get(agent.agent_id).await.unwrap().unwrap();
    assert_eq!(
        recovered.current_state,
        ygg::models::agent::AgentState::Idle
    );

    // Cleanup
    sqlx::query("DELETE FROM agents WHERE agent_name = 'test-crash-recovery'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_msg_send_inbox_mark_read() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool, "test");

    sqlx::query("DELETE FROM events WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool)
        .await
        .unwrap();

    agent_repo.register("test-msg-sender").await.unwrap();
    agent_repo.register("test-msg-recipient").await.unwrap();

    // Send two messages; inbox returns both; mark_read drains.
    ygg::cli::msg_cmd::send(
        &pool,
        "test-msg-sender",
        "test-msg-recipient",
        "hello 1",
        false,
    )
    .await
    .unwrap();
    ygg::cli::msg_cmd::send(
        &pool,
        "test-msg-sender",
        "test-msg-recipient",
        "hello 2",
        false,
    )
    .await
    .unwrap();

    let unread = ygg::cli::msg_cmd::inbox(&pool, "test-msg-recipient", false)
        .await
        .unwrap();
    assert_eq!(unread.len(), 2);
    assert_eq!(unread[0].body, "hello 1");
    assert_eq!(unread[1].body, "hello 2");
    assert_eq!(unread[0].from_agent_name, "test-msg-sender");

    ygg::cli::msg_cmd::mark_read(&pool, "test-msg-recipient")
        .await
        .unwrap();
    let post = ygg::cli::msg_cmd::inbox(&pool, "test-msg-recipient", false)
        .await
        .unwrap();
    assert!(post.is_empty(), "inbox empty after mark_read");

    // But --all should still return both.
    let all = ygg::cli::msg_cmd::inbox(&pool, "test-msg-recipient", true)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);

    // Missing recipient errors (no_spawn=true to test the error path;
    // send() with no_spawn=false auto-spawns the recipient since PR #70).
    let err =
        ygg::cli::msg_cmd::send_inner(&pool, "test-msg-sender", "does-not-exist", "x", false, true)
            .await;
    assert!(err.is_err());

    // Cleanup
    sqlx::query("DELETE FROM events WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-msg-sender', 'test-msg-recipient')")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_lock_release_all_for_agent() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool, "test");

    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-lock-stop-a', 'test-lock-stop-b')")
        .execute(&pool)
        .await
        .unwrap();

    let a = agent_repo.register("test-lock-stop-a").await.unwrap();
    let b = agent_repo.register("test-lock-stop-b").await.unwrap();

    let lock_mgr = ygg::lock::LockManager::new(&pool, 300, "test");
    lock_mgr.acquire("test-res-a-1", a.agent_id).await.unwrap();
    lock_mgr.acquire("test-res-a-2", a.agent_id).await.unwrap();
    lock_mgr.acquire("test-res-b-1", b.agent_id).await.unwrap();

    let released = lock_mgr.release_all_for_agent(a.agent_id).await.unwrap();
    assert_eq!(released, 2, "should release both locks for agent a");

    let a_after = lock_mgr.list_agent_locks(a.agent_id).await.unwrap();
    assert!(a_after.is_empty(), "agent a should hold no locks");
    let b_after = lock_mgr.list_agent_locks(b.agent_id).await.unwrap();
    assert_eq!(b_after.len(), 1, "agent b's lock must not be touched");

    // Cleanup
    let _ = lock_mgr.release_all_for_agent(b.agent_id).await;
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-lock-stop-a', 'test-lock-stop-b')")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_orphan_candidates_filter_by_idle() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool, "test");

    sqlx::query(
        "DELETE FROM agents WHERE agent_name IN ('test-orphan-fresh', 'test-orphan-stale')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM repos WHERE name = 'test-orphan-repo'")
        .execute(&pool)
        .await
        .unwrap();

    let fresh = agent_repo.register("test-orphan-fresh").await.unwrap();
    let stale = agent_repo.register("test-orphan-stale").await.unwrap();

    // Back-date the stale agent so it crosses the idle threshold.
    sqlx::query("UPDATE agents SET updated_at = now() - interval '2 hours' WHERE agent_id = $1")
        .bind(stale.agent_id)
        .execute(&pool)
        .await
        .unwrap();

    // Minimal fixture: repo + task per agent + worker with a bogus path.
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (name, task_prefix) VALUES ('test-orphan-repo', 'test-orphan-repo') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1) ON CONFLICT DO NOTHING")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    for (aid, seq) in [(fresh.agent_id, 1), (stale.agent_id, 2)] {
        let task: (uuid::Uuid,) = sqlx::query_as(
            "INSERT INTO tasks (repo_id, seq, title, kind, assignee) VALUES ($1, $2, 'fixture', 'task', $3) RETURNING task_id"
        ).bind(repo.0).bind(seq).bind(aid).fetch_one(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO workers (task_id, tmux_session, tmux_window, worktree_path) VALUES ($1, 's', 'w', '/tmp/ygg-test-nonexistent-worktree')"
        ).bind(task.0).execute(&pool).await.unwrap();
    }

    // At a 1-hour idle threshold, only the stale agent should be returned.
    let cands = agent_repo.list_orphan_candidates(3600).await.unwrap();
    assert!(
        cands.iter().any(|(id, _, _)| *id == stale.agent_id),
        "stale agent should be a candidate at 1h threshold"
    );
    assert!(
        !cands.iter().any(|(id, _, _)| *id == fresh.agent_id),
        "fresh agent should NOT be a candidate at 1h threshold"
    );

    // Cleanup (cascades wipe tasks + workers).
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM agents WHERE agent_name IN ('test-orphan-fresh', 'test-orphan-stale')",
    )
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn test_agent_rename() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let agent_repo = ygg::models::agent::AgentRepo::new(&pool, "test");

    // Clean up any leftovers from a prior failed run.
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-rename-src', 'test-rename-dst', 'test-rename-collision')")
        .execute(&pool).await.unwrap();

    let src = agent_repo.register("test-rename-src").await.unwrap();
    let other = agent_repo.register("test-rename-collision").await.unwrap();

    // Happy path: rename succeeds, id preserved, lookup by new name works,
    // old name is gone.
    agent_repo
        .rename(src.agent_id, "test-rename-dst")
        .await
        .unwrap();
    let found = agent_repo.get_by_name("test-rename-dst").await.unwrap();
    assert!(found.is_some(), "agent lookup by new name should succeed");
    assert_eq!(
        found.unwrap().agent_id,
        src.agent_id,
        "agent_id must be preserved"
    );
    let gone = agent_repo.get_by_name("test-rename-src").await.unwrap();
    assert!(gone.is_none(), "old name should no longer resolve");

    // Collision path: renaming another agent into an occupied (name, persona)
    // slot must error with a unique-violation (SQLSTATE 23505).
    let err = agent_repo
        .rename(other.agent_id, "test-rename-dst")
        .await
        .unwrap_err();
    let code = err
        .as_database_error()
        .and_then(|d| d.code().map(|c| c.to_string()));
    assert_eq!(
        code.as_deref(),
        Some("23505"),
        "collision should be unique_violation"
    );

    // Cleanup
    sqlx::query("DELETE FROM agents WHERE agent_name IN ('test-rename-src', 'test-rename-dst', 'test-rename-collision')")
        .execute(&pool).await.unwrap();
}

#[tokio::test]
async fn test_scheduler_finalizes_terminal_run_closes_task() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    // Use a fixture repo + task. Cleanup any prior runs of the test.
    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-test'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-test', 'sched-test') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();

    // Create a task in_progress and insert a succeeded task_runs row.
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status)
           VALUES ($1, 1, 'sched fin test', 'in_progress')
           RETURNING task_id"#,
    )
    .bind(repo.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, output_commit_sha, ended_at)
           VALUES ($1, 1, 'run:test:1', 'succeeded', 'deadbeef', now())"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
        auto_approve: false,
    };
    let stats = ygg::scheduler::tick(&pool, &cfg).await.unwrap();
    assert!(
        stats.finalized >= 1,
        "finalize_terminal_runs should close the task"
    );

    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status::text, close_reason, result_blob_ref FROM tasks WHERE task_id = $1",
    )
    .bind(task.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "closed");
    assert!(row.1.unwrap().contains("succeeded"));
    assert_eq!(row.2.as_deref(), Some("deadbeef"));

    // Cleanup
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_schedules_runnable_task_to_ready() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-sched'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-sched', 'sched-sched') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status, runnable, approval_level)
           VALUES ($1, 1, 'sched runnable', 'open', TRUE, 'auto')
           RETURNING task_id"#,
    )
    .bind(repo.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
        auto_approve: false,
    };
    let scheduled = ygg::scheduler::schedule_ready_tasks(&pool, &cfg)
        .await
        .unwrap();
    assert!(
        scheduled >= 1,
        "runnable + auto-approved task should be scheduled"
    );

    let state: String = sqlx::query_scalar(
        "SELECT state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt DESC LIMIT 1",
    )
    .bind(task.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "ready");

    // A second tick shouldn't double-schedule (NOT EXISTS guard).
    let scheduled2 = ygg::scheduler::schedule_ready_tasks(&pool, &cfg)
        .await
        .unwrap();
    assert_eq!(scheduled2, 0, "should not double-schedule");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_advisory_lock_singleton() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let g1 = ygg::scheduler::acquire_advisory_lock(&pool).await.unwrap();
    let g2 = ygg::scheduler::acquire_advisory_lock(&pool).await;
    assert!(
        g2.is_err(),
        "second acquire must fail while first guard is alive"
    );
    drop(g1);
    let g3 = ygg::scheduler::acquire_advisory_lock(&pool).await;
    assert!(
        g3.is_ok(),
        "after dropping first guard, third acquire must succeed"
    );
}

#[tokio::test]
async fn test_scheduler_reaps_expired_heartbeat() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-reap'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-reap', 'sched-reap') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'reap', 'in_progress') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // running run with heartbeat 10 minutes ago and ttl 60s.
    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, heartbeat_at, heartbeat_ttl_s, started_at)
           VALUES ($1, 1, 'run:reap:1', 'running', now() - interval '10 minutes', 60, now() - interval '11 minutes')"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let reaped = ygg::scheduler::reap_expired_heartbeats(&pool)
        .await
        .unwrap();
    assert_eq!(reaped, 1);

    let state: String = sqlx::query_scalar("SELECT state::text FROM task_runs WHERE task_id = $1")
        .bind(task.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "crashed");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_enforces_deadline() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-dl'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-dl', 'sched-dl') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'dl', 'in_progress') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, deadline_at, started_at)
           VALUES ($1, 1, 'run:dl:1', 'running', now() - interval '1 minute', now() - interval '5 minutes')"#,
    ).bind(task.0).execute(&pool).await.unwrap();

    let cancelled = ygg::scheduler::enforce_deadlines(&pool).await.unwrap();
    assert_eq!(cancelled, 1);
    let (state, reason): (String, String) =
        sqlx::query_as("SELECT state::text, reason::text FROM task_runs WHERE task_id = $1")
            .bind(task.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "cancelled");
    assert_eq!(reason, "timeout");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_schedules_retry_with_previous_attempt() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-retry'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-retry', 'sched-retry') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status, runnable, max_attempts) VALUES ($1, 1, 'retry', 'in_progress', TRUE, 3) RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // Failed attempt 1, ended 10 minutes ago. Backoff is exp 60s base; well past.
    sqlx::query(
        r#"INSERT INTO task_runs
           (task_id, attempt, idempotency_key, state, reason,
            ended_at, error, max_attempts)
           VALUES ($1, 1, 'run:retry:1', 'failed', 'agent_error',
                   now() - interval '10 minutes',
                   '{"reason_code":"agent_error","message":"tests red"}'::jsonb, 3)"#,
    )
    .bind(task.0)
    .execute(&pool)
    .await
    .unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
        auto_approve: false,
    };
    let n = ygg::scheduler::schedule_retries(&pool, &cfg).await.unwrap();
    assert_eq!(n, 1);

    // Original is now in 'retrying'; new attempt 2 exists.
    let states: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt, state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt",
    )
    .bind(task.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].1, "retrying");
    assert_eq!(states[1].1, "ready");

    // Previous-attempt thread-through.
    let new_input: serde_json::Value =
        sqlx::query_scalar("SELECT input FROM task_runs WHERE task_id = $1 AND attempt = 2")
            .bind(task.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let prev = new_input.get("previous_attempt").unwrap();
    assert_eq!(prev["attempt"], 1);
    assert_eq!(prev["error"]["reason_code"], "agent_error");

    // Idempotency: second call should not double-retry (successor already exists).
    let n2 = ygg::scheduler::schedule_retries(&pool, &cfg).await.unwrap();
    assert_eq!(n2, 0);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// yggdrasil-112 regression: `tick()` must run schedule_retries before
/// finalize_terminal_runs. With the reverse order, finalize closes the task
/// and the retry's `WHERE t.status <> 'closed'` skips it (retry = dead code).
#[tokio::test]
async fn test_scheduler_tick_retries_before_finalize() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-retry-order'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-retry-order', 'sched-retry-order') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status, runnable, max_attempts) VALUES ($1, 1, 'retry-order', 'in_progress', TRUE, 3) RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // Failed attempt 1, ended 10 minutes ago — backoff window is well past.
    sqlx::query(
        r#"INSERT INTO task_runs
           (task_id, attempt, idempotency_key, state, reason,
            ended_at, error, max_attempts)
           VALUES ($1, 1, 'run:retry-order:1', 'failed', 'agent_error',
                   now() - interval '10 minutes',
                   '{"reason_code":"agent_error","message":"tests red"}'::jsonb, 3)"#,
    )
    .bind(task.0)
    .execute(&pool)
    .await
    .unwrap();

    // max_concurrent=0 keeps dispatch_ready from spawning a real agent for the
    // newly-queued retry attempt; we only care that retry fires before finalize
    // and that the parent task isn't closed mid-retry.
    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 0,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
        auto_approve: false,
    };
    // tick() scans global state, so its returned counters can include other
    // tests' rows. We only assert side effects on OUR task.
    let _ = ygg::scheduler::tick(&pool, &cfg).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status::text FROM tasks WHERE task_id = $1")
        .bind(task.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(status, "closed", "task must NOT be closed mid-retry");

    let states: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt, state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt",
    )
    .bind(task.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(states.len(), 2, "retry must have queued attempt 2");
    assert_eq!(states[1].1, "ready");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// yggdrasil-110 regression: heartbeat with an unresolvable agent_name must
/// NOT match runs whose agent_id is NULL via `IS NOT DISTINCT FROM`. The
/// previous query would cross-contaminate any two NULL-agent_id runs.
#[tokio::test]
async fn test_heartbeat_refuses_unresolvable_agent() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'hb-null'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'hb-null', 'hb-null') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'hb', 'in_progress') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // Run with NULL agent_id (e.g. created by some other unresolvable agent).
    sqlx::query(
        r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, agent_id, max_attempts, heartbeat_at)
           VALUES ($1, 1, 'run:hb-null:1', 'running', NULL, 1, now() - interval '1 hour')"#,
    )
    .bind(task.0)
    .execute(&pool)
    .await
    .unwrap();

    // Heartbeat from an unresolvable name must error, not silently update the
    // NULL-agent_id row.
    let result = ygg::cli::run_cmd::heartbeat_cli(&pool, None, "no-such-agent-12345").await;
    assert!(
        result.is_err(),
        "heartbeat with unresolvable agent must error"
    );

    let hb_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT heartbeat_at FROM task_runs WHERE task_id = $1")
            .bind(task.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let now = chrono::Utc::now();
    assert!(
        hb_at.unwrap() < now - chrono::Duration::minutes(30),
        "heartbeat_at must NOT have been bumped (cross-contamination check)"
    );

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_detect_loops_poisons_repeating_failures() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-loop'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-loop', 'sched-loop') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status, max_attempts) VALUES ($1, 1, 'flaky', 'in_progress', 5) RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();

    // Three failed attempts with identical error.
    for n in 1..=3 {
        sqlx::query(
            r#"INSERT INTO task_runs (task_id, attempt, idempotency_key, state, reason, error, ended_at)
               VALUES ($1, $2, $3, 'failed', 'agent_error',
                       '{"reason_code":"agent_error","message":"tests red"}'::jsonb,
                       now())"#,
        )
        .bind(task.0)
        .bind(n)
        .bind(format!("run:loop:{n}"))
        .execute(&pool).await.unwrap();
    }

    let poisoned = ygg::scheduler::detect_loops(&pool, 3).await.unwrap();
    assert_eq!(poisoned, 1);

    let status: String = sqlx::query_scalar("SELECT status::text FROM tasks WHERE task_id = $1")
        .bind(task.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "blocked");

    let states: Vec<(i32, String)> = sqlx::query_as(
        "SELECT attempt, state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt",
    )
    .bind(task.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    for (_, s) in &states {
        assert_eq!(s, "poison");
    }

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_unpoison_reopens_task() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-unpo'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-unpo', 'sched-unpo') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'unpo', 'blocked') RETURNING task_id"
    ).bind(repo.0).fetch_one(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO task_runs (task_id, attempt, idempotency_key, state, reason) VALUES ($1, 1, 'run:unpo:1', 'poison', 'loop_detected')"
    ).bind(task.0).execute(&pool).await.unwrap();

    ygg::scheduler::unpoison(&pool, task.0).await.unwrap();

    let status: String = sqlx::query_scalar("SELECT status::text FROM tasks WHERE task_id = $1")
        .bind(task.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "open");
    let run_state: String =
        sqlx::query_scalar("SELECT state::text FROM task_runs WHERE task_id = $1")
            .bind(task.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(run_state, "failed");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_approval_gate_blocks_then_unblocks() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-appr'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-appr', 'sched-appr') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status, runnable, approval_level)
           VALUES ($1, 1, 'gated', 'open', TRUE, 'approve_plan')
           RETURNING task_id"#,
    )
    .bind(repo.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let cfg = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
        auto_approve: false,
    };

    // Without approval, schedule_ready_tasks should NOT pick this up.
    let n = ygg::scheduler::schedule_ready_tasks(&pool, &cfg)
        .await
        .unwrap();
    assert_eq!(n, 0, "approval_level=approve_plan must block dispatch");

    // After approving, schedule should pick it up.
    ygg::scheduler::approve(&pool, task.0, None).await.unwrap();
    let n = ygg::scheduler::schedule_ready_tasks(&pool, &cfg)
        .await
        .unwrap();
    assert_eq!(n, 1);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

/// yggdrasil-109 regression: with `auto_approve=true`, a task at
/// approval_level=approve_plan and no approved_at must dispatch as if approved.
#[tokio::test]
async fn test_scheduler_auto_approve_short_circuits_gate() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-autoap'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-autoap', 'sched-autoap') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let task: (uuid::Uuid,) = sqlx::query_as(
        r#"INSERT INTO tasks (repo_id, seq, title, status, runnable, approval_level)
           VALUES ($1, 1, 'autoap', 'open', TRUE, 'approve_plan')
           RETURNING task_id"#,
    )
    .bind(repo.0)
    .fetch_one(&pool)
    .await
    .unwrap();

    let cfg_off = ygg::scheduler::SchedulerConfig {
        tick_interval: std::time::Duration::from_secs(1),
        max_concurrent: 4,
        default_max_attempts: 3,
        default_heartbeat_ttl_s: 300,
        poison_threshold: 3,
        auto_approve: false,
    };
    let n_blocked = ygg::scheduler::schedule_ready_tasks(&pool, &cfg_off)
        .await
        .unwrap();
    assert_eq!(n_blocked, 0, "approval gate blocks without auto_approve");

    let cfg_on = ygg::scheduler::SchedulerConfig {
        auto_approve: true,
        ..cfg_off.clone()
    };
    let n_passed = ygg::scheduler::schedule_ready_tasks(&pool, &cfg_on)
        .await
        .unwrap();
    assert_eq!(n_passed, 1, "auto_approve must short-circuit the gate");

    let state: String = sqlx::query_scalar(
        "SELECT state::text FROM task_runs WHERE task_id = $1 ORDER BY attempt DESC LIMIT 1",
    )
    .bind(task.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "ready");

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_bench_runner_independent_parallel_n_with_fake_claude() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    // Use the in-tree fake claude binary so the test doesn't burn real tokens.
    let fake =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures/fake-claude.sh");
    assert!(fake.exists(), "fake-claude.sh fixture must ship in-tree");

    let scenarios_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/scenarios");
    let manifest =
        ygg::bench::manifest::LoadedManifest::load(scenarios_dir.join("independent-parallel-n"))
            .unwrap();

    let workspace = std::env::temp_dir().join(format!("ygg-bench-test-{}", uuid::Uuid::new_v4()));
    let cfg = ygg::bench::runner::RunnerConfig {
        claude_bin: fake,
        task_timeout_s: 60,
        workspace_root: Some(workspace.clone()),
    };

    // Use the ygg driver to exercise the parallel path.
    let driver = ygg::bench::drivers::YggDriver {
        config: cfg.clone(),
    };
    let run_id = ygg::bench::runner::execute(&pool, &manifest, &driver, Some(42), &cfg)
        .await
        .unwrap();

    let repo = ygg::bench::BenchRepo::new(&pool);
    let bench = repo.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(
        bench.passed,
        Some(true),
        "fake claude should produce a passing run"
    );
    assert_eq!(bench.scenario, "independent-parallel-n");

    // All four task results must be marked passed.
    let results = repo.list_task_results(run_id).await.unwrap();
    assert_eq!(results.len(), 4);
    for r in &results {
        assert!(r.passed, "task #{} should pass", r.task_idx);
        assert!(r.tokens_in.is_some(), "fake claude emits a usage block");
    }

    // pass_power_k_for over k=1 should be 1.0 since this run passed.
    let p = ygg::cli::bench_cmd::pass_power_k_for(
        &pool,
        "independent-parallel-n",
        ygg::bench::Baseline::Ygg,
        1,
    )
    .await
    .unwrap();
    assert!(
        p >= 1.0 - 1e-9,
        "single passed run should yield pass^1 = 1.0, got {p}"
    );

    // Cleanup workspace.
    let _ = std::fs::remove_dir_all(&workspace);
    sqlx::query("DELETE FROM bench_runs WHERE run_id = $1")
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_bench_diff_refuses_overlapping_cis() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    // Create two bench runs of the same scenario with similar wall-clocks
    // so their bootstrap CIs overlap. The diff verdict should be Inconclusive,
    // proving we refuse to declare a winner from noise.
    let repo = ygg::bench::BenchRepo::new(&pool);
    let a = repo
        .create_run(
            "test-overlap",
            ygg::bench::Baseline::VanillaSingle,
            1,
            "fake",
            "0",
            None,
        )
        .await
        .unwrap();
    let b = repo
        .create_run(
            "test-overlap",
            ygg::bench::Baseline::Ygg,
            1,
            "fake",
            "0",
            None,
        )
        .await
        .unwrap();

    // Wall-clocks within ±2s (CIs definitely overlap).
    for (idx, sec) in [10, 11, 12, 11].iter().enumerate() {
        repo.write_task_result(
            a.run_id,
            &ygg::bench::BenchTaskResult {
                run_id: a.run_id,
                task_idx: idx as i32,
                passed: true,
                wall_clock_s: *sec,
                tokens_in: None,
                tokens_out: None,
                tokens_cache: None,
                usd: None,
                reopened: false,
            },
        )
        .await
        .unwrap();
    }
    for (idx, sec) in [11, 10, 12, 11].iter().enumerate() {
        repo.write_task_result(
            b.run_id,
            &ygg::bench::BenchTaskResult {
                run_id: b.run_id,
                task_idx: idx as i32,
                passed: true,
                wall_clock_s: *sec,
                tokens_in: None,
                tokens_out: None,
                tokens_cache: None,
                usd: None,
                reopened: false,
            },
        )
        .await
        .unwrap();
    }
    repo.finalize(a.run_id, true, None).await.unwrap();
    repo.finalize(b.run_id, true, None).await.unwrap();

    // The CLI's `bench diff` is a printer over these same primitives; assert
    // the math directly to keep the test stable against output formatting.
    let res_a = repo.list_task_results(a.run_id).await.unwrap();
    let res_b = repo.list_task_results(b.run_id).await.unwrap();
    let wall_a: Vec<f64> = res_a.iter().map(|r| r.wall_clock_s as f64).collect();
    let wall_b: Vec<f64> = res_b.iter().map(|r| r.wall_clock_s as f64).collect();
    let (_, lo_a, hi_a) = ygg::bench::stats::bootstrap_mean_ci(&wall_a, 1000);
    let (_, lo_b, hi_b) = ygg::bench::stats::bootstrap_mean_ci(&wall_b, 1000);
    let v = ygg::bench::stats::ci_diff_verdict(lo_a, hi_a, lo_b, hi_b);
    assert!(
        matches!(v, ygg::bench::stats::Verdict::Inconclusive),
        "overlapping CIs must produce Inconclusive verdict; got {v:?}"
    );

    sqlx::query("DELETE FROM bench_runs WHERE scenario = 'test-overlap'")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_learnings_surface_for_files_matches_glob_and_increments() {
    // yggdrasil-82: surface_for_files matches scoped learnings by file_glob,
    // includes repo-wide (NULL glob) hits, and bumps applied_count for each.
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'learn-surf'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'learn-surf', 'learn-surf') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();

    let lr = ygg::models::learning::LearningRepo::new(&pool);
    // `*.rs` translates to SQL LIKE `%.rs` which matches `src/main.rs`.
    // The model's translate(glob, '*?', '%_') doesn't do `**`-collapsing,
    // so use a single-`*` glob that LIKE handles natively.
    let default_scope = serde_json::json!({});
    let glob_match = lr
        .create(
            Some(repo.0),
            Some("*.rs"),
            None,
            "Use anyhow::Result on public functions.",
            None,
            None,
            &default_scope,
        )
        .await
        .unwrap();
    let repo_wide = lr
        .create(
            Some(repo.0),
            None,
            None,
            "Always run cargo fmt before pushing.",
            None,
            None,
            &default_scope,
        )
        .await
        .unwrap();
    let other_glob = lr
        .create(
            Some(repo.0),
            Some("docs/*.md"),
            None,
            "Mermaid sections should preserve ASCII fallback.",
            None,
            None,
            &default_scope,
        )
        .await
        .unwrap();

    let files = vec!["src/main.rs".to_string()];
    let lines = ygg::cli::learning_cmd::surface_for_files(&pool, Some(repo.0), &files, None, None)
        .await
        .unwrap();

    // src/main.rs matches src/**/*.rs (glob_match) and the repo-wide entry,
    // but NOT docs/*.md (other_glob). Order shouldn't matter for the assertion.
    let joined = lines.join("\n");
    assert!(
        joined.contains("anyhow::Result"),
        "glob match should surface"
    );
    assert!(
        joined.contains("cargo fmt"),
        "repo-wide learning should surface"
    );
    assert!(
        !joined.contains("Mermaid"),
        "non-matching glob must not surface"
    );

    // applied_count incremented for the surfaced ones.
    let n_glob: i32 =
        sqlx::query_scalar("SELECT applied_count FROM learnings WHERE learning_id = $1")
            .bind(glob_match.learning_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let n_repo: i32 =
        sqlx::query_scalar("SELECT applied_count FROM learnings WHERE learning_id = $1")
            .bind(repo_wide.learning_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let n_other: i32 =
        sqlx::query_scalar("SELECT applied_count FROM learnings WHERE learning_id = $1")
            .bind(other_glob.learning_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n_glob, 1);
    assert_eq!(n_repo, 1);
    assert_eq!(n_other, 0);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_task_create_runnable_default_by_kind() {
    // yggdrasil-105: new tasks of kind task|bug|feature|chore are runnable by
    // default; epics stay manual until they opt in via plan_strategy='llm'.
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'runnable-default'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'runnable-default', 'runnable-default') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();

    use ygg::models::task::{TaskCreate, TaskKind, TaskRepo};
    let task_repo = TaskRepo::new(&pool);

    for kind in [
        TaskKind::Task,
        TaskKind::Bug,
        TaskKind::Feature,
        TaskKind::Chore,
    ] {
        let kind_str = format!("{kind}");
        let task = task_repo
            .create(
                repo.0,
                None,
                TaskCreate {
                    title: &format!("auto-runnable {kind_str}"),
                    kind: kind.clone(),
                    priority: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let runnable: bool = sqlx::query_scalar("SELECT runnable FROM tasks WHERE task_id = $1")
            .bind(task.task_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            runnable,
            "kind={kind_str} should be runnable=TRUE by default"
        );
    }

    // Epics: not runnable by default.
    let epic = task_repo
        .create(
            repo.0,
            None,
            TaskCreate {
                title: "manual epic",
                kind: TaskKind::Epic,
                priority: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let runnable: bool = sqlx::query_scalar("SELECT runnable FROM tasks WHERE task_id = $1")
        .bind(epic.task_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !runnable,
        "epics must stay manual unless they opt in to plan_strategy"
    );

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_bench_runner_contention_with_fake_claude() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let fake =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/fixtures/fake-claude.sh");
    let scenarios_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/scenarios");
    let manifest =
        ygg::bench::manifest::LoadedManifest::load(scenarios_dir.join("contention")).unwrap();

    let workspace = std::env::temp_dir().join(format!("ygg-bench-cont-{}", uuid::Uuid::new_v4()));
    let cfg = ygg::bench::runner::RunnerConfig {
        claude_bin: fake,
        task_timeout_s: 30,
        workspace_root: Some(workspace.clone()),
    };

    // Use VanillaSingle (sequential) so both bumps land in Cargo.toml. The
    // scheduler-backed YggDriver with locks would also pass; the parallel
    // race-without-locks path is the negative test, requiring an explicit
    // mode flag — left for a follow-up.
    let driver = ygg::bench::drivers::VanillaSingleDriver {
        config: cfg.clone(),
    };
    let run_id = ygg::bench::runner::execute(&pool, &manifest, &driver, None, &cfg)
        .await
        .unwrap();

    let repo = ygg::bench::BenchRepo::new(&pool);
    let bench = repo.get_run(run_id).await.unwrap().unwrap();
    assert_eq!(
        bench.passed,
        Some(true),
        "sequential driver should land both bumps"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    sqlx::query("DELETE FROM bench_runs WHERE run_id = $1")
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_scheduler_backfill_idempotent() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'sched-bf'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'sched-bf', 'sched-bf') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_seq (repo_id, next_seq) VALUES ($1, 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tasks (repo_id, seq, title, status) VALUES ($1, 1, 'bf-ip', 'in_progress'), ($1, 2, 'bf-cl', 'closed')"
    ).bind(repo.0).execute(&pool).await.unwrap();

    let s1 = ygg::scheduler::backfill(&pool).await.unwrap();
    assert!(s1.in_progress_runs_created >= 1);
    assert!(s1.closed_runs_created >= 1);

    let s2 = ygg::scheduler::backfill(&pool).await.unwrap();
    assert_eq!(s2.in_progress_runs_created, 0);
    assert_eq!(s2.closed_runs_created, 0);

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_pending_migrations_returns_empty_when_up_to_date() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let pending = ygg::db::pending_migrations(&pool).await.unwrap();
    assert!(
        pending.is_empty(),
        "expected 0 pending after migrate, got {}: {:?}",
        pending.len(),
        pending,
    );
}

#[tokio::test]
async fn test_no_empty_user_id_agents() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE user_id = ''")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "found {count} agents with empty user_id after backfill migration"
    );
}

#[tokio::test]
async fn test_agent_list_returns_current_user_agents() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    let user_id = "test-user-visibility";
    let other_user = "other-user-visibility";

    let repo = ygg::models::agent::AgentRepo::new(&pool, user_id);
    let other_repo = ygg::models::agent::AgentRepo::new(&pool, other_user);

    let mine = repo.register("vis-test-mine").await.unwrap();
    let theirs = other_repo.register("vis-test-theirs").await.unwrap();

    let my_list = repo.list().await.unwrap();
    assert!(
        my_list.iter().any(|a| a.agent_name == "vis-test-mine"),
        "my agent should be visible"
    );
    assert!(
        !my_list.iter().any(|a| a.agent_name == "vis-test-theirs"),
        "other user's agent should NOT be visible"
    );

    sqlx::query("DELETE FROM agents WHERE agent_id IN ($1, $2)")
        .bind(mine.agent_id)
        .bind(theirs.agent_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_handoff_save_supersedes_and_clears() {
    // `ygg handoff`: one note per (repo, agent); save replaces; clear removes.
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();

    sqlx::query("DELETE FROM repos WHERE task_prefix = 'handoff-test'")
        .execute(&pool)
        .await
        .unwrap();
    let repo: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO repos (canonical_url, name, task_prefix) VALUES (NULL, 'handoff-test', 'handoff-test') RETURNING repo_id"
    ).fetch_one(&pool).await.unwrap();

    let hr = ygg::models::handoff::HandoffRepo::new(&pool);

    // First save, then read back.
    hr.save(Some(repo.0), None, "first note").await.unwrap();
    let got = hr.latest(Some(repo.0), None).await.unwrap().unwrap();
    assert_eq!(got.text, "first note");

    // A second save supersedes — latest is the new text, and only one row exists.
    hr.save(Some(repo.0), None, "second note").await.unwrap();
    let got = hr.latest(Some(repo.0), None).await.unwrap().unwrap();
    assert_eq!(got.text, "second note", "save replaces the prior handoff");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM handoffs WHERE repo_id = $1")
        .bind(repo.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "exactly one handoff per (repo, agent)");

    // Clear removes it.
    let cleared = hr.clear(Some(repo.0), None).await.unwrap();
    assert!(cleared, "clear reports it removed a handoff");
    assert!(
        hr.latest(Some(repo.0), None).await.unwrap().is_none(),
        "no handoff after clear"
    );

    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
}
