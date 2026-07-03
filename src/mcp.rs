//! Minimal **MCP-over-SSE** client — the yggdrasil tree's first Model Context
//! Protocol client. It speaks the transport the `mnemosyne` agent-memory
//! service exposes (see `agent-sandbox/docs/hermes-gateway.md`
//! "## Agent memory (mnemosyne)" and ADR 0002), used by `ygg memory`.
//!
//! ## Transport (JSON-RPC 2.0 over SSE)
//! A single long-lived `GET <url>` opens an `text/event-stream`. The server's
//! **first** event is `event: endpoint` whose `data:` is the relative URL to
//! POST JSON-RPC requests to (a `/messages/?session_id=…` path). Every request
//! is `POST`ed there (the POST returns `202 Accepted` with no useful body); the
//! matching JSON-RPC **response** arrives back as an `event: message` frame on
//! the still-open GET stream. Requests and responses are correlated by the
//! JSON-RPC `id`.
//!
//! ## Design
//! `connect` opens the GET stream and spawns one background reader task that
//! owns the byte stream. The reader parses SSE frames and routes each response
//! to a per-`id` [`oneshot`] channel held in a shared map; the very first
//! `endpoint` event is delivered to `connect` over its own channel. Callers
//! ([`McpClient::request`] / [`call_tool`](McpClient::call_tool)) register a
//! sender, POST the request, and await the reply with a timeout. This supports
//! concurrent in-flight calls, though `ygg memory` only ever issues one at a
//! time.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::config::AppConfig;

/// Default correlation timeout for a single request/response round-trip.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

// ── SSE frame parsing ───────────────────────────────────────────────────────

/// One decoded Server-Sent Event: an optional `event:` type and the joined
/// `data:` payload. The default event type (no `event:` line) is `message`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser. Bytes arrive in arbitrary chunks off the wire, so
/// this buffers until it sees an event terminator (a blank line) and only then
/// emits complete [`SseEvent`]s. Line endings are normalised to `\n` so `\r\n`
/// and lone `\r` are handled per the SSE spec.
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    /// Feed a chunk of the stream; return any events completed by it.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        // Normalise line endings so the boundary search only looks for "\n\n".
        let normalised = chunk.replace("\r\n", "\n").replace('\r', "\n");
        self.buf.push_str(&normalised);

        let mut out = Vec::new();
        while let Some(idx) = self.buf.find("\n\n") {
            let block: String = self.buf.drain(..idx).collect();
            // Drop the "\n\n" terminator.
            self.buf.drain(..2);
            if let Some(ev) = parse_event_block(&block) {
                out.push(ev);
            }
        }
        out
    }
}

/// Parse a single event block (lines between blank-line boundaries) into an
/// [`SseEvent`]. Returns `None` for comment-only / empty blocks.
fn parse_event_block(block: &str) -> Option<SseEvent> {
    let mut event: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    let mut saw_field = false;

    for line in block.split('\n') {
        if line.is_empty() || line.starts_with(':') {
            continue; // blank or comment line
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""), // a field name with no colon => empty value
        };
        match field {
            "event" => {
                saw_field = true;
                event = Some(value.to_string());
            }
            "data" => {
                saw_field = true;
                data_lines.push(value);
            }
            // id / retry and unknown fields are irrelevant to this client.
            _ => {}
        }
    }

    if !saw_field {
        return None;
    }
    Some(SseEvent {
        event,
        data: data_lines.join("\n"),
    })
}

// ── JSON-RPC result unwrapping ──────────────────────────────────────────────

/// Unwrap an MCP `tools/call` result into a JSON value. MCP returns
/// `{"content":[{"type":"text","text":"<…>"}], "isError": bool}`; mnemosyne's
/// text is itself a JSON document, so we parse it. When the text is not JSON we
/// return it as a JSON string so callers still get something faithful. An
/// `isError: true` result is surfaced as an error.
pub fn unwrap_tool_result(result: &Value) -> anyhow::Result<Value> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tool result missing content array: {result}"))?;

    let text = content
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("tool result has no text content: {result}"))?;

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        bail!("mnemosyne tool error: {text}");
    }

    // The text is usually JSON; fall back to a plain string if not.
    Ok(serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_string())))
}

// ── Client ──────────────────────────────────────────────────────────────────

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// A connected MCP-over-SSE session. Cheap to hold; the background reader is
/// aborted on drop.
pub struct McpClient {
    http: reqwest::Client,
    /// Absolute URL to POST JSON-RPC requests to (resolved from the `endpoint`
    /// event against the SSE base URL).
    post_url: String,
    token: Option<String>,
    pending: Pending,
    next_id: AtomicI64,
    timeout: Duration,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl McpClient {
    /// Connect using `MNEMOSYNE_MCP_URL` / `MNEMOSYNE_MCP_TOKEN` from config.
    pub async fn connect_from_config(cfg: &AppConfig) -> anyhow::Result<Self> {
        let url = cfg.mnemosyne_mcp_url.clone().ok_or_else(|| {
            anyhow!(
                "MNEMOSYNE_MCP_URL is not set — point ygg at the mnemosyne MCP \
                 service (e.g. MNEMOSYNE_MCP_URL=http://hermes-gateway.ph.ca:8077/sse \
                 in ~/.config/ygg/.env)"
            )
        })?;
        Self::connect(&url, cfg.mnemosyne_mcp_token.clone(), DEFAULT_TIMEOUT).await
    }

    /// Open the SSE stream, complete the `initialize` / `notifications/initialized`
    /// handshake, and return a ready client.
    pub async fn connect(
        sse_url: &str,
        token: Option<String>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        // A dedicated client with no response timeout: the SSE GET is a
        // long-lived stream. Per-request timeouts are enforced by correlation.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut req = http
            .get(sse_url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(t) = token.as_deref().filter(|s| !s.is_empty()) {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("opening MCP SSE stream at {sse_url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP SSE stream {status}: {body}");
        }

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();
        let stream = resp.bytes_stream();
        let reader = tokio::spawn(reader_loop(stream, endpoint_tx, pending.clone()));

        // The endpoint event must be the first frame; bound the wait.
        let endpoint_rel = tokio::time::timeout(timeout, endpoint_rx)
            .await
            .map_err(|_| anyhow!("timed out waiting for MCP endpoint event"))?
            .map_err(|_| anyhow!("MCP stream closed before sending endpoint event"))?;

        let post_url = reqwest::Url::parse(sse_url)
            .with_context(|| format!("invalid MNEMOSYNE_MCP_URL: {sse_url}"))?
            .join(&endpoint_rel)
            .with_context(|| format!("resolving MCP endpoint {endpoint_rel}"))?
            .to_string();

        let client = Self {
            http,
            post_url,
            token,
            pending,
            next_id: AtomicI64::new(1),
            timeout,
            reader,
        };

        // MCP handshake.
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "ygg", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .context("MCP initialize failed")?;
        client
            .notify("notifications/initialized", Value::Null)
            .await
            .context("MCP notifications/initialized failed")?;

        Ok(client)
    }

    /// Send a JSON-RPC request and await the correlated response. Returns the
    /// `result` value; surfaces a JSON-RPC `error` object as an error.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap();
            p.insert(id, tx);
        }

        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.post(&body).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        let value = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                bail!("MCP stream closed before responding to {method} (id {id})")
            }
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("MCP request {method} (id {id}) timed out");
            }
        };

        if let Some(err) = value.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            bail!("MCP error from {method}: {msg} (code {code})");
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Fire a JSON-RPC notification (no id, no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let mut body = json!({"jsonrpc": "2.0", "method": method});
        if !params.is_null() {
            body["params"] = params;
        }
        self.post(&body).await
    }

    /// List available tool names (`tools/list`).
    pub async fn list_tools(&self) -> anyhow::Result<Vec<String>> {
        let result = self.request("tools/list", json!({})).await?;
        let names = result
            .get("tools")
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(names)
    }

    /// Call a tool (`tools/call`) and unwrap its text-content JSON result.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        unwrap_tool_result(&result)
    }

    /// POST a JSON-RPC envelope to the message endpoint (expects `202`/`200`).
    async fn post(&self, body: &Value) -> anyhow::Result<()> {
        let mut req = self.http.post(&self.post_url).json(body);
        if let Some(t) = self.token.as_deref().filter(|s| !s.is_empty()) {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.context("POSTing MCP request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("MCP message endpoint {status}: {text}");
        }
        Ok(())
    }
}

/// Background task: read the SSE byte stream, parse frames, and route them.
/// The first `endpoint` event goes to `endpoint_tx`; every JSON-RPC response
/// (an `event: message` frame carrying an `id`) is delivered to the waiting
/// sender in `pending`.
async fn reader_loop<B: AsRef<[u8]>>(
    stream: impl futures::Stream<Item = reqwest::Result<B>>,
    endpoint_tx: oneshot::Sender<String>,
    pending: Pending,
) {
    tokio::pin!(stream);
    let mut parser = SseParser::default();
    let mut endpoint_tx = Some(endpoint_tx);

    while let Some(item) = stream.next().await {
        let bytes = match item {
            Ok(b) => b,
            Err(_) => break, // stream error — drop pending senders, callers see "closed"
        };
        let text = String::from_utf8_lossy(bytes.as_ref());
        for ev in parser.push(&text) {
            if ev.data == "[DONE]" {
                continue;
            }
            match ev.event.as_deref() {
                Some("endpoint") => {
                    if let Some(tx) = endpoint_tx.take() {
                        let _ = tx.send(ev.data);
                    }
                }
                // Default event type (or "message") carries JSON-RPC responses.
                // id-less frames are server notifications and fall through.
                _ => {
                    if let Ok(val) = serde_json::from_str::<Value>(&ev.data)
                        && let Some(id) = val.get("id").and_then(Value::as_i64)
                        && let Some(tx) = pending.lock().unwrap().remove(&id)
                    {
                        let _ = tx.send(val);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_then_message() {
        let mut p = SseParser::default();
        let evs = p.push("event: endpoint\ndata: /messages/?session_id=abc\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("endpoint"));
        assert_eq!(evs[0].data, "/messages/?session_id=abc");
    }

    #[test]
    fn parses_split_across_chunks() {
        let mut p = SseParser::default();
        assert!(p.push("event: mess").is_empty());
        assert!(p.push("age\ndata: {\"id\":1").is_empty());
        let evs = p.push("}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message"));
        assert_eq!(evs[0].data, "{\"id\":1}");
    }

    #[test]
    fn joins_multiline_data() {
        let mut p = SseParser::default();
        let evs = p.push("data: line1\ndata: line2\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "line1\nline2");
        assert_eq!(evs[0].event, None);
    }

    #[test]
    fn handles_crlf_and_comments() {
        let mut p = SseParser::default();
        let evs = p.push(": keep-alive\r\nevent: message\r\ndata: hi\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message"));
        assert_eq!(evs[0].data, "hi");
    }

    #[test]
    fn emits_multiple_events_in_one_chunk() {
        let mut p = SseParser::default();
        let evs = p.push("event: a\ndata: 1\n\nevent: b\ndata: 2\n\n");
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "1");
        assert_eq!(evs[1].data, "2");
    }

    #[test]
    fn done_marker_is_a_normal_data_frame() {
        // The parser is transport-neutral; [DONE] filtering happens in the
        // reader loop, so the parser still surfaces it as data.
        let mut p = SseParser::default();
        let evs = p.push("data: [DONE]\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "[DONE]");
    }

    #[test]
    fn unwrap_parses_inner_json() {
        let result = json!({
            "content": [{"type": "text", "text": "{\"id\":\"m1\",\"importance\":0.9}"}]
        });
        let v = unwrap_tool_result(&result).unwrap();
        assert_eq!(v.get("id").and_then(Value::as_str), Some("m1"));
        assert_eq!(v.get("importance").and_then(Value::as_f64), Some(0.9));
    }

    #[test]
    fn unwrap_falls_back_to_plain_string() {
        let result = json!({"content": [{"type": "text", "text": "just words"}]});
        let v = unwrap_tool_result(&result).unwrap();
        assert_eq!(v.as_str(), Some("just words"));
    }

    #[test]
    fn unwrap_surfaces_is_error() {
        let result = json!({
            "isError": true,
            "content": [{"type": "text", "text": "memory not found"}]
        });
        let err = unwrap_tool_result(&result).unwrap_err().to_string();
        assert!(err.contains("memory not found"), "err was: {err}");
    }

    #[test]
    fn unwrap_errors_without_content() {
        let err = unwrap_tool_result(&json!({"foo": 1}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("content"), "err was: {err}");
    }
}
