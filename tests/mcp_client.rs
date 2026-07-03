//! End-to-end test for the MCP-over-SSE client ([`ygg::mcp::McpClient`]).
//!
//! `wiremock` can't easily model the endpoint-event handshake plus a long-lived
//! SSE stream whose frames are produced in reaction to POSTs on a *separate*
//! connection, so this stands up a tiny hand-rolled TCP server that speaks the
//! minimal transport: it opens the GET `/sse` stream, emits the `endpoint`
//! event, and — as JSON-RPC requests arrive by POST — writes the correlated
//! `event: message` responses back onto the open GET stream.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};

use ygg::mcp::McpClient;

/// Spawn the mock and return its `http://127.0.0.1:PORT/sse` URL.
async fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // The GET handler drains this receiver and writes each string as an SSE
    // frame; POST handlers push correlated responses in.
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    let rx = Arc::new(Mutex::new(Some(rx)));

    tokio::spawn(async move {
        loop {
            let (sock, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let tx = tx.clone();
            let rx = rx.clone();
            tokio::spawn(handle_conn(sock, tx, rx));
        }
    });

    format!("http://{addr}/sse")
}

async fn handle_conn(
    mut sock: TcpStream,
    tx: mpsc::UnboundedSender<String>,
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
) {
    // Read headers (up to the blank line).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let body_start = pos + 4;
            let rest = buf[body_start..].to_vec();
            return dispatch(sock, head, rest, tx, rx).await;
        }
    }
}

async fn dispatch(
    mut sock: TcpStream,
    head: String,
    mut body: Vec<u8>,
    tx: mpsc::UnboundedSender<String>,
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
) {
    let request_line = head.lines().next().unwrap_or("");

    if request_line.starts_with("GET") {
        // Open the SSE stream and emit the endpoint event immediately.
        let resp = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: keep-alive\r\n\r\n";
        if sock.write_all(resp.as_bytes()).await.is_err() {
            return;
        }
        let endpoint = "event: endpoint\ndata: /messages/?session_id=test\n\n";
        let _ = sock.write_all(endpoint.as_bytes()).await;
        let _ = sock.flush().await;

        // Drain response frames produced by POST handlers.
        let mut guard = rx.lock().await;
        if let Some(mut receiver) = guard.take() {
            drop(guard);
            while let Some(frame) = receiver.recv().await {
                if sock.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                let _ = sock.flush().await;
            }
        }
        return;
    }

    // POST /messages — read the JSON-RPC body (respecting Content-Length).
    let content_length = head
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
        })
        .unwrap_or(0);
    while body.len() < content_length {
        let mut tmp = [0u8; 1024];
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }

    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();

    // Notifications (no id) get a 202 and produce no SSE frame.
    if let Some(id) = id
        && let Some(response) = respond(method, &id, &req)
    {
        let frame = format!("event: message\ndata: {response}\n\n");
        let _ = tx.send(frame);
    }

    // `Connection: close` so reqwest opens a fresh connection per POST rather
    // than reusing this one, which the mock closes after a single request.
    let ok = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = sock.write_all(ok.as_bytes()).await;
    let _ = sock.flush().await;
}

/// Build the JSON-RPC response body for a method, or `None` when the mock
/// should stay silent.
fn respond(method: &str, id: &Value, req: &Value) -> Option<String> {
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "mock-mnemosyne", "version": "0"}
        }),
        "tools/list" => json!({
            "tools": [
                {"name": "mnemosyne_recall"},
                {"name": "mnemosyne_remember"},
                {"name": "mnemosyne_stats"}
            ]
        }),
        "tools/call" => {
            let name = req
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if name == "boom" {
                // JSON-RPC error object — the client must surface it as an error.
                let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "boom failed"}});
                return Some(err.to_string());
            }
            // A recall returns text content whose text is itself JSON.
            let inner = json!([
                {"id": "m1", "content": "the sky is blue", "importance": 0.9, "scope": "global"},
                {"id": "m2", "content": "grass is green", "importance": 0.4, "scope": "session"}
            ])
            .to_string();
            json!({"content": [{"type": "text", "text": inner}]})
        }
        _ => return None,
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn handshake_and_list_tools() {
    let url = spawn_mock().await;
    let client = McpClient::connect(&url, Some("tok".into()), std::time::Duration::from_secs(5))
        .await
        .expect("connect + handshake");

    let tools = client.list_tools().await.unwrap();
    assert!(
        tools.contains(&"mnemosyne_recall".to_string()),
        "tools: {tools:?}"
    );
    assert!(tools.contains(&"mnemosyne_remember".to_string()));
    assert_eq!(tools.len(), 3);
}

#[tokio::test]
async fn call_tool_unwraps_inner_json() {
    let url = spawn_mock().await;
    let client = McpClient::connect(&url, None, std::time::Duration::from_secs(5))
        .await
        .unwrap();

    let v = client
        .call_tool("mnemosyne_recall", json!({"query": "sky", "limit": 5}))
        .await
        .unwrap();

    // The inner text was JSON — an array of two records — and got parsed.
    let arr = v.as_array().expect("array result");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0].get("id").and_then(Value::as_str), Some("m1"));
    assert_eq!(
        arr[0].get("content").and_then(Value::as_str),
        Some("the sky is blue")
    );
}

#[tokio::test]
async fn call_tool_propagates_jsonrpc_error() {
    let url = spawn_mock().await;
    let client = McpClient::connect(&url, None, std::time::Duration::from_secs(5))
        .await
        .unwrap();

    let err = client
        .call_tool("boom", json!({}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("boom failed"), "err was: {err}");
}
