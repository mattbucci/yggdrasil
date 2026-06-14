//! Regression tests for `ygg task move` — reassign a task to another repo,
//! renumbering its per-repo seq while keeping the task_id stable.
//!
//! Requires Postgres + migrations applied:
//!     DATABASE_URL=postgres://ng@localhost:5432/ygg cargo test --test task_move -- --test-threads=1

use std::env;
use uuid::Uuid;
use ygg::models::repo::RepoRepo;
use ygg::models::task::{TaskCreate, TaskKind, TaskRepo};

async fn make_repo(pool: &sqlx::PgPool, prefix: &str) -> Uuid {
    RepoRepo::new(pool)
        .register(None, prefix, prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap()
        .repo_id
}

async fn teardown(pool: &sqlx::PgPool, repo_ids: &[Uuid]) {
    for id in repo_ids {
        sqlx::query("DELETE FROM repos WHERE repo_id = $1")
            .bind(id)
            .execute(pool)
            .await
            .ok();
    }
}

#[tokio::test]
async fn move_reassigns_repo_and_renumbers_seq() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let src = make_repo(&pool, "movesrc").await;
    let dst = make_repo(&pool, "movedst").await;
    let repo = TaskRepo::new(&pool);

    let labels: [String; 0] = [];
    let task = repo
        .create(
            src,
            None,
            TaskCreate {
                title: "filed in wrong repo",
                description: "",
                kind: TaskKind::Bug,
                priority: 2,
                labels: &labels,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let task_id = task.task_id;

    let new_seq = repo.move_to_repo(task_id, dst, None).await.unwrap();

    // task_id is stable; repo_id + seq follow the destination.
    let moved = repo.get(task_id).await.unwrap().unwrap();
    assert_eq!(
        moved.task_id, task_id,
        "task_id must be stable across a move"
    );
    assert_eq!(moved.repo_id, dst, "repo_id must point at the destination");
    assert_eq!(moved.seq, new_seq, "row seq must match the allocated seq");

    // The new human ref resolves in the destination repo.
    let by_ref = repo.get_by_ref(dst, new_seq).await.unwrap().unwrap();
    assert_eq!(by_ref.task_id, task_id);

    // It no longer resolves under its old (repo, seq).
    assert!(
        repo.get_by_ref(src, task.seq).await.unwrap().is_none(),
        "old ref must stop resolving in the source repo"
    );

    teardown(&pool, &[src, dst]).await;
}

#[tokio::test]
async fn move_records_a_moved_event() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let src = make_repo(&pool, "moveevsrc").await;
    let dst = make_repo(&pool, "moveevdst").await;
    let repo = TaskRepo::new(&pool);

    let labels: [String; 0] = [];
    let task = repo
        .create(
            src,
            None,
            TaskCreate {
                title: "event check",
                description: "",
                kind: TaskKind::Task,
                priority: 2,
                labels: &labels,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    repo.move_to_repo(task.task_id, dst, None).await.unwrap();

    let moved_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_events WHERE task_id = $1 AND kind = 'moved'",
    )
    .bind(task.task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(moved_events, 1, "a single 'moved' event must be recorded");

    teardown(&pool, &[src, dst]).await;
}
