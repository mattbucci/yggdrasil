//! Regression tests for the scheduler-as-sole-writer-of-crashed split
//! (yggdrasil-140).
//!
//! 1. Scheduler's heartbeat-reap is the only path that transitions a run to
//!    `crashed`. Calling it on a stale running run produces exactly one
//!    `crashed` row.
//! 2. The watcher's `flag_stale_agents` is observation-only: it must NOT
//!    transition agent state, and it MUST emit `agent_stale_warning` events.
//!

mod common;
use ygg::config::AppConfig;
use ygg::models::agent::{AgentRepo, AgentState};
use ygg::models::event::EventKind;
use ygg::models::repo::RepoRepo;
use ygg::models::task::{TaskCreate, TaskKind, TaskRepo};
use ygg::models::task_run::{RunState, TaskRunCreate, TaskRunRepo};
use ygg::watcher::Watcher;

async fn setup(pool: &ygg::db::DbPool, suffix: &str) -> (String, String, String, String) {
    let prefix = format!("watcher{suffix}");
    let repo = RepoRepo::new(pool)
        .register(None, &prefix, &prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap();

    let labels: [String; 0] = [];
    let task = TaskRepo::new(pool)
        .create(
            &repo.repo_id,
            None,
            TaskCreate {
                title: &format!("watcher-test-{suffix}"),
                description: "",
                acceptance: None,
                design: None,
                notes: None,
                kind: TaskKind::Task,
                priority: 2,
                assignee: None,
                labels: &labels,
                external_ref: None,
                agent_slug: None,
            },
        )
        .await
        .unwrap();

    let agent = AgentRepo::new(pool, "test")
        .register(&format!("watcher-agent-{suffix}"))
        .await
        .unwrap();

    let run = TaskRunRepo::new(pool)
        .create(TaskRunCreate {
            task_id: task.task_id.clone(),
            attempt: 1,
            input: serde_json::json!({}),
            ..Default::default()
        })
        .await
        .unwrap();

    (repo.repo_id, task.task_id, agent.agent_id, run.run_id)
}

async fn teardown(pool: &ygg::db::DbPool, repo_id: String, agent_id: String) {
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM agents WHERE agent_id = $1")
        .bind(agent_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn scheduler_heartbeat_reap_produces_exactly_one_crashed_transition() {
    let pool = common::test_db().await;
    let (repo_id, _task_id, agent_id, run_id) = setup(&pool, "soleowner").await;

    // Pin the run to running with a stale heartbeat.
    sqlx::query(
        r#"UPDATE task_runs
           SET state = 'running',
               agent_id = $2,
               started_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-1 hour'),
               heartbeat_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-1 hour'),
               heartbeat_ttl_s = 60
           WHERE run_id = $1"#,
    )
    .bind(&run_id)
    .bind(&agent_id)
    .execute(&*pool)
    .await
    .unwrap();

    // Run the reap once.
    let reaped = ygg::scheduler::reap_expired_heartbeats(&pool)
        .await
        .unwrap();
    assert!(reaped >= 1, "expected at least one reap, got {reaped}");

    let row: (RunState,) = sqlx::query_as("SELECT state FROM task_runs WHERE run_id = $1")
        .bind(run_id)
        .fetch_one(&*pool)
        .await
        .unwrap();
    assert_eq!(row.0, RunState::Crashed);

    // A second reap on the same row must be a no-op (already terminal).
    let second = ygg::scheduler::reap_expired_heartbeats(&pool)
        .await
        .unwrap();
    assert_eq!(second, 0, "second reap should not retransition");

    teardown(&pool, repo_id, agent_id).await;
}

#[tokio::test]
async fn watcher_flag_stale_does_not_mutate_agent_state() {
    let pool = common::test_db().await;
    let (repo_id, _task_id, agent_id, _run_id) = setup(&pool, "noop").await;

    // Force the agent into a stale executing state.
    AgentRepo::new(&pool, "test")
        .force_state(&agent_id, AgentState::Executing, None)
        .await
        .unwrap();
    sqlx::query("UPDATE agents SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-2 hours') WHERE agent_id = $1")
        .bind(&agent_id)
        .execute(&*pool)
        .await
        .unwrap();

    // Snapshot the event-id high-water-mark so we can scope our event check.
    let watermark: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%f+00:00','now')")
            .fetch_one(&*pool)
            .await
            .unwrap();

    // Boot a watcher with a tight enough lock_ttl_secs that "2 hours stale"
    // crosses the (lock_ttl_secs * 2) threshold easily.
    let mut config = AppConfig::from_env().unwrap();
    config.lock_ttl_secs = 60;
    let watcher = Watcher::new(pool.clone(), config);
    let flagged = watcher.flag_stale_agents().await.unwrap();
    assert!(flagged >= 1, "expected at least one stale flag");

    // Agent state must still be Executing — the watcher is observation-only.
    let still: AgentState =
        sqlx::query_scalar("SELECT current_state FROM agents WHERE agent_id = $1")
            .bind(&agent_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert_eq!(
        still,
        AgentState::Executing,
        "watcher must not mutate agent state"
    );

    // Exactly one AgentStaleWarning event must have been emitted for this
    // agent since the watermark.
    let count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM events
           WHERE event_kind = 'agent_stale_warning'
             AND agent_id = $1
             AND created_at >= $2"#,
    )
    .bind(&agent_id)
    .bind(watermark)
    .fetch_one(&*pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "expected exactly one stale-warning event");

    teardown(&pool, repo_id, agent_id).await;
}

#[tokio::test]
async fn agent_stale_warning_is_a_known_event_kind() {
    // Compile-time guard that the variant exists with the right label —
    // catches accidental rename without rebuilding migrations.
    assert_eq!(EventKind::AgentStaleWarning.label(), "agent_stale_warning");
}
