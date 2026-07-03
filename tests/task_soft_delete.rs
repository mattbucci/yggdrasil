//! Regression tests for soft-delete + trash + purge (yggdrasil-123).
//!

mod common;
use ygg::models::repo::RepoRepo;
use ygg::models::task::{TaskCreate, TaskKind, TaskRepo, TaskStatus};

async fn make_task(pool: &ygg::db::DbPool, suffix: &str, idx: usize) -> (String, String) {
    let prefix = format!("trash{suffix}");
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
                title: &format!("trash-test-{suffix}-{idx}"),
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
    (repo.repo_id, task.task_id)
}

async fn teardown(pool: &ygg::db::DbPool, repo_id: String) {
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn soft_delete_hides_from_list_ready_blocked() {
    let pool = common::test_db().await;
    let (repo_id, task_id) = make_task(&pool, "hides", 1).await;
    let repo = TaskRepo::new(&pool);

    // Pre-delete: task surfaces in list + ready.
    let before_list = repo
        .list(Some(repo_id.clone()), Some(TaskStatus::Open))
        .await
        .unwrap();
    assert!(before_list.iter().any(|t| t.task_id == task_id));
    let before_ready = repo.ready(&repo_id).await.unwrap();
    assert!(before_ready.iter().any(|t| t.task_id == task_id));

    // Soft-delete.
    assert!(repo.soft_delete(&task_id).await.unwrap());

    // Post-delete: gone from list + ready.
    let after_list = repo
        .list(Some(repo_id.clone()), Some(TaskStatus::Open))
        .await
        .unwrap();
    assert!(!after_list.iter().any(|t| t.task_id == task_id));
    let after_ready = repo.ready(&repo_id).await.unwrap();
    assert!(!after_ready.iter().any(|t| t.task_id == task_id));

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn restore_brings_task_back_into_views() {
    let pool = common::test_db().await;
    let (repo_id, task_id) = make_task(&pool, "restore", 1).await;
    let repo = TaskRepo::new(&pool);

    repo.soft_delete(&task_id).await.unwrap();
    assert!(repo.restore(&task_id).await.unwrap());

    let after = repo
        .list(Some(repo_id.clone()), Some(TaskStatus::Open))
        .await
        .unwrap();
    assert!(after.iter().any(|t| t.task_id == task_id));

    // Restoring an already-live row is a no-op.
    assert!(!repo.restore(&task_id).await.unwrap());

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn list_trashed_returns_only_deleted_rows() {
    let pool = common::test_db().await;
    let (repo_id, alive_id) = make_task(&pool, "trashlist", 1).await;
    let repo = TaskRepo::new(&pool);

    // Create a second task in the same repo, then trash it.
    let labels: [String; 0] = [];
    let trashed = repo
        .create(
            &repo_id,
            None,
            TaskCreate {
                title: "trashlist-victim",
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
    repo.soft_delete(&trashed.task_id).await.unwrap();

    let bin = repo.list_trashed(Some(repo_id.clone())).await.unwrap();
    let ids: Vec<String> = bin.iter().map(|t| t.task_id.clone()).collect();
    assert!(ids.contains(&trashed.task_id));
    assert!(!ids.contains(&alive_id));

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn purge_drops_only_old_trashed_rows() {
    let pool = common::test_db().await;
    let (repo_id, fresh_id) = make_task(&pool, "purge", 1).await;
    let (_, old_id) = make_task(&pool, "purge", 2).await;
    let repo = TaskRepo::new(&pool);

    // Trash both, then back-date one beyond the purge horizon.
    repo.soft_delete(&fresh_id).await.unwrap();
    repo.soft_delete(&old_id).await.unwrap();
    sqlx::query("UPDATE tasks SET deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-40 days') WHERE task_id = $1")
        .bind(&old_id)
        .execute(&*pool)
        .await
        .unwrap();

    // Purge anything older than 30 days, scoped to this repo.
    let n = repo
        .purge_older_than(30, Some(repo_id.clone()))
        .await
        .unwrap();
    assert_eq!(n, 1, "exactly the back-dated row must purge");

    let still_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id = $1)")
            .bind(fresh_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert!(still_exists, "fresh trashed row must survive 30d purge");

    let old_gone: bool =
        sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM tasks WHERE task_id = $1)")
            .bind(old_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert!(old_gone, "back-dated row must be hard-deleted");

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn ready_skips_deleted_blockers_so_dependents_unblock() {
    let pool = common::test_db().await;
    let (repo_id, task_id) = make_task(&pool, "blockerdel", 1).await;
    let repo = TaskRepo::new(&pool);

    // Create a blocker; declare task depends on it.
    let labels: [String; 0] = [];
    let blocker = repo
        .create(
            &repo_id,
            None,
            TaskCreate {
                title: "blockerdel-blocker",
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
    repo.add_dep(&task_id, &blocker.task_id).await.unwrap();

    // Pre-delete: task is blocked.
    let pre = repo.ready(&repo_id).await.unwrap();
    assert!(!pre.iter().any(|t| t.task_id == task_id));

    // Trash the blocker → task unblocks.
    repo.soft_delete(&blocker.task_id).await.unwrap();
    let post = repo.ready(&repo_id).await.unwrap();
    assert!(post.iter().any(|t| t.task_id == task_id));

    teardown(&pool, repo_id).await;
}
