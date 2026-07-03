//! Regression for the dep-cycle guard + lint discovery (yggdrasil-122).
//!
//! Exercises both halves: `add_dep` rejects an edge that would close a cycle,
//! and `find_cycles` surfaces pre-existing cycles inserted via raw SQL
//! (the route by which un-guarded data sneaks in).
//!

mod common;
use ygg::models::repo::RepoRepo;
use ygg::models::task::{TaskCreate, TaskKind, TaskRepo};

async fn setup_repo(pool: &ygg::db::DbPool, suffix: &str) -> (String, [String; 3]) {
    let prefix = format!("cyclelint{suffix}");
    let repo = RepoRepo::new(pool)
        .register(None, &prefix, &prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap();

    let task_repo = TaskRepo::new(pool);
    let mut ids = [const { String::new() }; 3];
    let labels: [String; 0] = [];
    for (i, slot) in ids.iter_mut().enumerate() {
        let title = format!("cycle-test-{suffix}-{i}");
        let t = task_repo
            .create(
                &repo.repo_id,
                None,
                TaskCreate {
                    title: &title,
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
        *slot = t.task_id;
    }
    (repo.repo_id, ids)
}

async fn teardown(pool: &ygg::db::DbPool, repo_id: String) {
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn add_dep_rejects_two_node_cycle() {
    let pool = common::test_db().await;
    let (repo_id, [a, b, _]) = setup_repo(&pool, "2node").await;
    let task_repo = TaskRepo::new(&pool);

    task_repo.add_dep(&a, &b).await.unwrap();
    let err = task_repo.add_dep(&b, &a).await.unwrap_err();
    assert!(
        format!("{err}").contains("cycle"),
        "expected cycle error, got: {err}"
    );

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn add_dep_rejects_three_node_cycle() {
    let pool = common::test_db().await;
    let (repo_id, [a, b, c]) = setup_repo(&pool, "3node").await;
    let task_repo = TaskRepo::new(&pool);

    // a → b → c (legal chain).
    task_repo.add_dep(&a, &b).await.unwrap();
    task_repo.add_dep(&b, &c).await.unwrap();
    // c → a would close the loop.
    let err = task_repo.add_dep(&c, &a).await.unwrap_err();
    assert!(format!("{err}").contains("cycle"));

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn find_cycles_empty_when_dag_is_acyclic() {
    let pool = common::test_db().await;
    let (repo_id, [a, b, c]) = setup_repo(&pool, "acyclic").await;
    let task_repo = TaskRepo::new(&pool);

    task_repo.add_dep(&a, &b).await.unwrap();
    task_repo.add_dep(&b, &c).await.unwrap();
    let cycles = task_repo.find_cycles(Some(repo_id.clone())).await.unwrap();
    assert!(cycles.is_empty(), "expected no cycles, got {cycles:?}");

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn find_cycles_surfaces_preexisting_cycle_inserted_via_raw_sql() {
    let pool = common::test_db().await;
    let (repo_id, [a, b, c]) = setup_repo(&pool, "raw").await;

    // Bypass add_dep entirely — this is how stale data sneaks in.
    for (t, blocker) in [
        (a.clone(), b.clone()),
        (b.clone(), c.clone()),
        (c.clone(), a.clone()),
    ] {
        sqlx::query("INSERT INTO task_deps (task_id, blocker_id) VALUES ($1, $2)")
            .bind(t)
            .bind(blocker)
            .execute(&*pool)
            .await
            .unwrap();
    }

    let cycles = TaskRepo::new(&pool)
        .find_cycles(Some(repo_id.clone()))
        .await
        .unwrap();
    assert_eq!(cycles.len(), 1, "expected one canonical cycle: {cycles:?}");
    let cycle = &cycles[0];
    assert_eq!(cycle.len(), 3);
    let mut sorted = cycle.clone();
    sorted.sort();
    let mut expected = vec![a, b, c];
    expected.sort();
    assert_eq!(sorted, expected);

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn find_cycles_dedups_rotations_of_same_cycle() {
    let pool = common::test_db().await;
    let (repo_id, [a, b, c]) = setup_repo(&pool, "rotdedup").await;

    for (t, blocker) in [(a.clone(), b.clone()), (b, c.clone()), (c, a)] {
        sqlx::query("INSERT INTO task_deps (task_id, blocker_id) VALUES ($1, $2)")
            .bind(t)
            .bind(blocker)
            .execute(&*pool)
            .await
            .unwrap();
    }

    // The recursive CTE finds the cycle starting from each of a, b, c
    // (three different rotations). Canonicalization must collapse them.
    let cycles = TaskRepo::new(&pool)
        .find_cycles(Some(repo_id.clone()))
        .await
        .unwrap();
    assert_eq!(cycles.len(), 1, "rotations not deduped: {cycles:?}");

    teardown(&pool, repo_id).await;
}
