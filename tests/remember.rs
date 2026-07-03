//! Regression tests for `ygg remember` — durable notes (post-ADR-0015, no
//! embeddings). Verifies repo-scoped vs global write and the prime/list
//! retrieval semantics (repo notes + global, newest-first).
//!

mod common;
use ygg::models::memory::MemoryRepo;
use ygg::models::repo::RepoRepo;

async fn make_repo(pool: &ygg::db::DbPool, prefix: &str) -> String {
    RepoRepo::new(pool)
        .register(None, prefix, prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap()
        .repo_id
}

async fn teardown(pool: &ygg::db::DbPool, repo_ids: &[String]) {
    // memories cascade on repo delete; global notes are removed explicitly.
    for id in repo_ids {
        sqlx::query("DELETE FROM repos WHERE repo_id = $1")
            .bind(id)
            .execute(pool)
            .await
            .ok();
    }
}

#[tokio::test]
async fn repo_scoped_list_includes_global_notes() {
    let pool = common::test_db().await;
    let repo_a = make_repo(&pool, "memrepoa").await;
    let repo_b = make_repo(&pool, "memrepob").await;
    let mem = MemoryRepo::new(&pool);

    let a_note = mem
        .create(Some(repo_a.clone()), "note in A", None)
        .await
        .unwrap();
    let b_note = mem
        .create(Some(repo_b.clone()), "note in B", None)
        .await
        .unwrap();
    let g_note = mem.create(None, "global note", None).await.unwrap();

    // Repo A's view: its own note + the global one, never repo B's.
    let view = mem.list(Some(repo_a.clone()), false, 50).await.unwrap();
    let ids: Vec<String> = view.iter().map(|m| m.memory_id.clone()).collect();
    assert!(ids.contains(&a_note.memory_id), "repo A note must appear");
    assert!(ids.contains(&g_note.memory_id), "global note must appear");
    assert!(
        !ids.contains(&b_note.memory_id),
        "repo B note must not leak into repo A's view"
    );

    // Clean up the global note (not cascaded by repo delete).
    mem.delete(g_note.memory_id).await.unwrap();
    teardown(&pool, &[repo_a, repo_b]).await;
}

#[tokio::test]
async fn all_flag_crosses_repos_and_newest_first() {
    let pool = common::test_db().await;
    let repo_a = make_repo(&pool, "memalla").await;
    let repo_b = make_repo(&pool, "memallb").await;
    let mem = MemoryRepo::new(&pool);

    let first = mem
        .create(Some(repo_a.clone()), "older", None)
        .await
        .unwrap();
    let second = mem
        .create(Some(repo_b.clone()), "newer", None)
        .await
        .unwrap();

    let all = mem.list(None, true, 50).await.unwrap();
    let ids: Vec<String> = all.iter().map(|m| m.memory_id.clone()).collect();
    assert!(ids.contains(&first.memory_id));
    assert!(ids.contains(&second.memory_id));

    // Newest-first ordering: `second` precedes `first` in the result.
    let pos_first = ids.iter().position(|id| *id == first.memory_id).unwrap();
    let pos_second = ids.iter().position(|id| *id == second.memory_id).unwrap();
    assert!(
        pos_second < pos_first,
        "list must be newest-first (created_at DESC)"
    );

    teardown(&pool, &[repo_a, repo_b]).await;
}
