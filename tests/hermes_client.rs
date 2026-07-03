//! Integration tests for the hermes-gateway HTTP client. A `wiremock` server
//! stands in for a real gateway — no network, no gateway process. Exercises the
//! happy paths (submit/list/get/output/cancel/models/overview) plus the error
//! surfaces the client must translate (403 scope, 429 backpressure, the
//! `{"error":{...}}` envelope).

use ygg::hermes::{HermesClient, TaskSubmit};

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(base: &str) -> HermesClient {
    HermesClient::new(
        base.to_string(),
        Some("hgw_test".to_string()),
        Some("hgwd_test".to_string()),
    )
}

#[tokio::test]
async fn health_no_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status":"ok"})))
        .mount(&server)
        .await;

    let v = client(&server.uri()).health().await.unwrap();
    assert_eq!(v.get("status").and_then(|s| s.as_str()), Some("ok"));
}

#[tokio::test]
async fn models_lists_agents() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                {"id": "feature-dev", "object": "model", "owned_by": "hermes-gateway"},
                {"id": "hermes", "object": "model", "owned_by": "hermes-gateway"}
            ]
        })))
        .mount(&server)
        .await;

    let models = client(&server.uri()).models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "feature-dev");
    assert_eq!(models[1].id, "hermes");
}

#[tokio::test]
async fn task_submit_returns_record() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "t-20260702-abcd1234",
            "agent": "feature-dev",
            "state": "pending",
            "priority": 0,
            "attempts": 0,
            "session_id": "task:t-20260702-abcd1234"
        })))
        .mount(&server)
        .await;

    let rec = client(&server.uri())
        .task_submit(&TaskSubmit::new("feature-dev", "do the thing"))
        .await
        .unwrap();
    assert_eq!(rec.id, "t-20260702-abcd1234");
    assert_eq!(rec.state, "pending");
    assert_eq!(rec.agent.as_deref(), Some("feature-dev"));
}

#[tokio::test]
async fn task_submit_scope_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {"message": "token not scoped for agent secret", "type": "permission_error"}
        })))
        .mount(&server)
        .await;

    let err = client(&server.uri())
        .task_submit(&TaskSubmit::new("secret", "x"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("403"), "err was: {err}");
    assert!(err.contains("not scoped"), "err was: {err}");
    assert!(err.contains("permission_error"), "err was: {err}");
}

#[tokio::test]
async fn task_submit_rate_limited_surfaces_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "15")
                .set_body_json(serde_json::json!({
                    "error": {"message": "Agent feature-dev is busy and its queue is full",
                              "type": "rate_limit_error"}
                })),
        )
        .mount(&server)
        .await;

    let err = client(&server.uri())
        .task_submit(&TaskSubmit::new("feature-dev", "x"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("429"), "err was: {err}");
    assert!(err.contains("Retry-After: 15"), "err was: {err}");
    assert!(err.contains("rate_limit_error"), "err was: {err}");
}

#[tokio::test]
async fn task_list_filters_and_decodes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/tasks"))
        .and(query_param("agent", "feature-dev"))
        .and(query_param("state", "running"))
        .and(query_param("limit", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                {"id": "t-1", "object": "task.summary", "agent": "feature-dev",
                 "state": "running", "priority": 0, "attempts": 1, "age_s": 12.5},
                {"id": "t-2", "object": "task.summary", "agent": "feature-dev",
                 "state": "running", "priority": 2, "attempts": 1, "age_s": 3.0}
            ],
            "has_more": true
        })))
        .mount(&server)
        .await;

    let list = client(&server.uri())
        .task_list(Some("feature-dev"), Some("running"), Some(10), None)
        .await
        .unwrap();
    assert_eq!(list.data.len(), 2);
    assert!(list.has_more);
    assert_eq!(list.data[0].id, "t-1");
    assert_eq!(list.data[0].age_s, Some(12.5));
}

#[tokio::test]
async fn task_get_full_record_with_result() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/tasks/t-9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "t-9",
            "agent": "feature-dev",
            "state": "succeeded",
            "result": {"content": "all done", "finish_reason": "stop",
                       "usage": {"prompt_tokens": 12, "completion_tokens": 34}},
            "attempt_history": [{"outcome": "succeeded"}]
        })))
        .mount(&server)
        .await;

    let rec = client(&server.uri()).task_get("t-9").await.unwrap();
    assert_eq!(rec.state, "succeeded");
    assert!(rec.is_terminal());
    assert_eq!(
        rec.result
            .as_ref()
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_str()),
        Some("all done")
    );
    // Unmodeled fields are preserved for faithful --json output.
    assert!(rec.extra.contains_key("attempt_history"));
}

#[tokio::test]
async fn task_get_not_found_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/tasks/nope"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {"message": "No such task", "type": "invalid_request_error"}
        })))
        .mount(&server)
        .await;

    let err = client(&server.uri())
        .task_get("nope")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("404"), "err was: {err}");
    assert!(err.contains("No such task"), "err was: {err}");
}

#[tokio::test]
async fn task_output_returns_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/tasks/t-1/output"))
        .respond_with(ResponseTemplate::new(200).set_body_string("line 1\nline 2\n"))
        .mount(&server)
        .await;

    let out = client(&server.uri()).task_output("t-1").await.unwrap();
    assert_eq!(out, "line 1\nline 2\n");
}

#[tokio::test]
async fn task_cancel_idempotent_terminal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tasks/t-1/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "t-1",
            "state": "succeeded",
            "already_terminal": true
        })))
        .mount(&server)
        .await;

    let rec = client(&server.uri()).task_cancel("t-1").await.unwrap();
    assert_eq!(rec.state, "succeeded");
    assert_eq!(rec.already_terminal, Some(true));
}

#[tokio::test]
async fn task_delete_ok() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v1/tasks/t-1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"deleted": true})),
        )
        .mount(&server)
        .await;

    let v = client(&server.uri()).task_delete("t-1").await.unwrap();
    assert_eq!(v.get("deleted").and_then(|d| d.as_bool()), Some(true));
}

#[tokio::test]
async fn task_delete_conflict_when_not_terminal() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v1/tasks/t-1"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": {"message": "task is not terminal", "type": "invalid_request_error"}
        })))
        .mount(&server)
        .await;

    let err = client(&server.uri())
        .task_delete("t-1")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("409"), "err was: {err}");
    assert!(err.contains("not terminal"), "err was: {err}");
}

#[tokio::test]
async fn overview_uses_dashboard_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dashboard/api/overview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "gateway": {"version": "1.2.3", "uptime_s": 3600, "pid": 42},
            "agents": [
                {"agent": "feature-dev", "vm": {"alive": true}, "limit": 1,
                 "running": [{"run_id": "r1"}], "waiting": [], "last_error": ""}
            ],
            "tasks_by_state": {"pending": 1, "running": 2, "succeeded": 10},
            "totals": {"reqs_1m": 5, "errors_1m": 0, "p95_ms_1m": 120.0}
        })))
        .mount(&server)
        .await;

    let ov = client(&server.uri()).overview().await.unwrap();
    assert_eq!(
        ov.get("gateway")
            .and_then(|g| g.get("version"))
            .and_then(|v| v.as_str()),
        Some("1.2.3")
    );
    assert_eq!(
        ov.get("agents").and_then(|a| a.as_array()).map(|a| a.len()),
        Some(1)
    );
}

#[tokio::test]
async fn overview_fails_closed_without_dashboard_token() {
    // A client with no dashboard token must error locally (before any request)
    // with a clear message, mirroring the gateway's fail-closed posture.
    let server = MockServer::start().await;
    let c = HermesClient::new(server.uri(), Some("hgw_test".into()), None);
    let err = c.overview().await.unwrap_err().to_string();
    assert!(err.contains("HERMES_DASHBOARD_TOKEN"), "err was: {err}");
}

#[tokio::test]
async fn gateway_calls_require_token() {
    let server = MockServer::start().await;
    let c = HermesClient::new(server.uri(), None, None);
    let err = c.models().await.unwrap_err().to_string();
    assert!(err.contains("HERMES_GATEWAY_TOKEN"), "err was: {err}");
}
