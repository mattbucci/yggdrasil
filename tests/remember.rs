//! Regression tests for `ygg remember` — durable notes (post-ADR-0015, no
//! embeddings). Verifies repo-scoped vs global write and the prime/list
//! retrieval semantics (repo notes + global, newest-first).
//!
//! Requires Postgres + migrations applied:
//!     DATABASE_URL=postgres://ng@localhost:5432/ygg cargo test --test remember -- --test-threads=1

use std::env;
use uuid::Uuid;
use ygg::models::memory::MemoryRepo;
use ygg::models::repo::RepoRepo;

async fn make_repo(pool: &sqlx::PgPool, prefix: &str) -> Uuid {
    RepoRepo::new(pool)
        .register(None, prefix, prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap()
        .repo_id
}

async fn teardown(pool: &sqlx::PgPool, repo_ids: &[Uuid]) {
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
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let repo_a = make_repo(&pool, "memrepoa").await;
    let repo_b = make_repo(&pool, "memrepob").await;
    let mem = MemoryRepo::new(&pool);

    let a_note = mem.create(Some(repo_a), "note in A", None).await.unwrap();
    let b_note = mem.create(Some(repo_b), "note in B", None).await.unwrap();
    let g_note = mem.create(None, "global note", None).await.unwrap();

    // Repo A's view: its own note + the global one, never repo B's.
    let view = mem.list(Some(repo_a), false, 50).await.unwrap();
    let ids: Vec<Uuid> = view.iter().map(|m| m.memory_id).collect();
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
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let repo_a = make_repo(&pool, "memalla").await;
    let repo_b = make_repo(&pool, "memallb").await;
    let mem = MemoryRepo::new(&pool);

    let first = mem.create(Some(repo_a), "older", None).await.unwrap();
    let second = mem.create(Some(repo_b), "newer", None).await.unwrap();

    let all = mem.list(None, true, 50).await.unwrap();
    let ids: Vec<Uuid> = all.iter().map(|m| m.memory_id).collect();
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
