//! Regression test for the unified task_runs JSONB output gate (yggdrasil-137).
//!
//! Acceptance: writers route through one entry point that enforces 16 KiB;
//! oversize payloads spill to the content-addressed blob store. Inserting a
//! 100 KiB output through the gate must leave `task_runs.output` NULL and
//! `task_runs.output_blob_ref` populated with the corresponding blob ref.
//!

mod common;
use ygg::blob::BlobStore;
use ygg::models::repo::RepoRepo;
use ygg::models::task::{TaskCreate, TaskKind, TaskRepo};
use ygg::models::task_run::{
    MAX_INLINE_PAYLOAD_BYTES, PayloadSink, TaskRunCreate, TaskRunRepo, route_payload,
};

async fn setup_run(pool: &ygg::db::DbPool, suffix: &str) -> (String, String, String) {
    let prefix = format!("outsize{suffix}");
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
                title: &format!("output-cap-{suffix}"),
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

    let run = TaskRunRepo::new(pool)
        .create(TaskRunCreate {
            task_id: task.task_id.clone(),
            attempt: 1,
            input: serde_json::json!({}),
            ..Default::default()
        })
        .await
        .unwrap();

    (repo.repo_id, task.task_id, run.run_id)
}

async fn teardown(pool: &ygg::db::DbPool, repo_id: String) {
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo_id)
        .execute(pool)
        .await
        .ok();
}

#[test]
fn route_payload_inline_when_small() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let v = serde_json::json!({"k": "small"});
    match route_payload(&v, &store).unwrap() {
        PayloadSink::Inline(out) => assert_eq!(out, v),
        PayloadSink::Blob { .. } => panic!("small payload should stay inline"),
    }
}

#[test]
fn route_payload_spills_when_over_cap() {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let big = "x".repeat(100 * 1024);
    let v = serde_json::json!({"transcript": big});
    match route_payload(&v, &store).unwrap() {
        PayloadSink::Inline(_) => panic!("100KiB payload should spill"),
        PayloadSink::Blob {
            blob_ref,
            original_bytes,
        } => {
            assert_eq!(blob_ref.len(), 64);
            assert!(original_bytes > MAX_INLINE_PAYLOAD_BYTES);
        }
    }
}

#[tokio::test]
async fn write_output_inline_for_small_payload() {
    let pool = common::test_db().await;
    let (repo_id, _task_id, run_id) = setup_run(&pool, "inline").await;

    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let v = serde_json::json!({"summary": "all good"});

    let sink = TaskRunRepo::new(&pool)
        .write_output(&run_id, &v, &store)
        .await
        .unwrap();
    assert!(matches!(sink, PayloadSink::Inline(_)));

    let row: (Option<serde_json::Value>, Option<String>) =
        sqlx::query_as("SELECT output, output_blob_ref FROM task_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert_eq!(row.0, Some(v));
    assert!(row.1.is_none(), "blob_ref must be NULL on inline path");

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn write_output_spills_100kib_to_blob_store() {
    let pool = common::test_db().await;
    let (repo_id, _task_id, run_id) = setup_run(&pool, "spill").await;

    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();

    let big = "x".repeat(100 * 1024);
    let payload = serde_json::json!({"transcript": big});
    let serialized_len = serde_json::to_vec(&payload).unwrap().len();
    assert!(serialized_len > MAX_INLINE_PAYLOAD_BYTES);

    let sink = TaskRunRepo::new(&pool)
        .write_output(&run_id, &payload, &store)
        .await
        .unwrap();
    let blob_ref = match sink {
        PayloadSink::Blob { blob_ref, .. } => blob_ref,
        PayloadSink::Inline(_) => panic!("expected blob spill"),
    };

    let row: (Option<serde_json::Value>, Option<String>) =
        sqlx::query_as("SELECT output, output_blob_ref FROM task_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert!(row.0.is_none(), "inline output must be NULL on spill path");
    assert_eq!(row.1.as_deref(), Some(blob_ref.as_str()));

    let stored = store
        .get(&ygg::blob::BlobRef::parse(blob_ref).unwrap())
        .unwrap();
    assert_eq!(stored.len(), serialized_len);
    let round_trip: serde_json::Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(round_trip, payload);

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn write_output_clears_blob_ref_when_overwritten_with_small_payload() {
    let pool = common::test_db().await;
    let (repo_id, _task_id, run_id) = setup_run(&pool, "overwrite").await;

    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let runs = TaskRunRepo::new(&pool);

    // First write a big payload — lands in blob.
    let big = serde_json::json!({"t": "x".repeat(100 * 1024)});
    runs.write_output(&run_id, &big, &store).await.unwrap();
    // Now overwrite with something small — must clear the stale blob ref.
    let small = serde_json::json!({"summary": "redacted"});
    runs.write_output(&run_id, &small, &store).await.unwrap();

    let row: (Option<serde_json::Value>, Option<String>) =
        sqlx::query_as("SELECT output, output_blob_ref FROM task_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert_eq!(row.0, Some(small));
    assert!(
        row.1.is_none(),
        "stale blob_ref must be cleared when row is overwritten inline"
    );

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn write_error_inline_for_typical_message() {
    let pool = common::test_db().await;
    let (repo_id, _task_id, run_id) = setup_run(&pool, "errsmall").await;

    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::new(dir.path()).unwrap();
    let v = serde_json::json!({"reason_code": "agent_error", "hint": "tests failed"});

    TaskRunRepo::new(&pool)
        .write_error(&run_id, &v, &store)
        .await
        .unwrap();

    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT error FROM task_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&*pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(v));

    teardown(&pool, repo_id).await;
}
