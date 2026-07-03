//! Typed HTTP client for the **hermes-gateway** router
//! (see `agent-sandbox/docs/hermes-gateway.md`).
//!
//! The gateway presents an OpenAI-compatible chat API plus an async **task**
//! API and an ops **dashboard** API. This is the first HTTP client in the
//! yggdrasil tree; it is deliberately small and typed only where callers need
//! structure — task records and the dashboard overview are large and evolve on
//! the gateway side, so those are surfaced as `serde_json::Value` for faithful
//! `--json` passthrough while the fields ygg reasons about (id, state, …) are
//! pulled out with small typed structs.
//!
//! Two independent bearer tokens:
//!   * `HERMES_GATEWAY_TOKEN`   — `/v1/*` (models + tasks); scope must allow the agent.
//!   * `HERMES_DASHBOARD_TOKEN` — `/dashboard/api/*` (ops-privileged overview).
//!
//! Errors map the gateway's `{"error":{"message":"...","type":"..."}}` envelope
//! to descriptive `anyhow` errors, including the HTTP status.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::AppConfig;

/// A live handle to a hermes-gateway router.
#[derive(Clone, Debug)]
pub struct HermesClient {
    base: String,
    http: reqwest::Client,
    gateway_token: Option<String>,
    dashboard_token: Option<String>,
}

// ── Request bodies ──────────────────────────────────────────────────────────

/// Body for `POST /v1/tasks`. The gateway accepts either `input` (a plain
/// prompt) or a full `request.messages` block; ygg only ever submits `input`.
#[derive(Debug, Clone, Serialize)]
pub struct TaskSubmit {
    pub agent: String,
    pub input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl TaskSubmit {
    /// Minimal submission: an agent and a prompt, gateway defaults for the rest.
    pub fn new(agent: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            input: input.into(),
            priority: None,
            timeout_s: None,
            deadline_s: None,
            max_attempts: None,
            not_before: None,
            session_id: None,
        }
    }
}

// ── Response bodies ─────────────────────────────────────────────────────────

/// One entry from `GET /v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelList {
    data: Vec<Model>,
}

/// A full task record (`POST /v1/tasks`, `GET /v1/tasks/{id}`,
/// `POST /v1/tasks/{id}/cancel`). Only the fields ygg reasons about are typed;
/// everything else is preserved in `extra` so `--json` output is faithful.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub state: String,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub attempts: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub already_terminal: Option<bool>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    /// Everything else the gateway sent (attempt_history, deadline, …).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl TaskRecord {
    /// Is this a terminal gateway state? (`succeeded|failed|cancelled|expired`)
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state.as_str(),
            "succeeded" | "failed" | "cancelled" | "expired"
        )
    }
}

/// One entry from `GET /v1/tasks` (a `task.summary`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub state: String,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub attempts: Option<i64>,
    #[serde(default)]
    pub age_s: Option<f64>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

/// Envelope from `GET /v1/tasks`.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskList {
    #[serde(default)]
    pub data: Vec<TaskSummary>,
    #[serde(default)]
    pub has_more: bool,
}

// ── Construction ────────────────────────────────────────────────────────────

impl HermesClient {
    /// Build a client from application config. Errors clearly when
    /// `HERMES_GATEWAY_URL` is unset — every caller needs a base URL.
    pub fn from_config(cfg: &AppConfig) -> anyhow::Result<Self> {
        let base = cfg.hermes_gateway_url.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "HERMES_GATEWAY_URL is not set — point ygg at a hermes-gateway \
                 (e.g. HERMES_GATEWAY_URL=http://hermes-gateway.ph.ca:8642 in \
                 ~/.config/ygg/.env)"
            )
        })?;
        Ok(Self::new(
            base,
            cfg.hermes_gateway_token.clone(),
            cfg.hermes_dashboard_token.clone(),
        ))
    }

    /// Construct directly (used by tests against a mock gateway).
    pub fn new(
        base: impl Into<String>,
        gateway_token: Option<String>,
        dashboard_token: Option<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            http,
            gateway_token,
            dashboard_token,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn gateway_token(&self) -> anyhow::Result<&str> {
        self.gateway_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "HERMES_GATEWAY_TOKEN is not set — the gateway task/model API \
                 requires a bearer token"
                )
            })
    }

    fn dashboard_token(&self) -> anyhow::Result<&str> {
        self.dashboard_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "HERMES_DASHBOARD_TOKEN is not set — the ops dashboard API \
                     requires its own bearer token (separate from the gateway token)"
                )
            })
    }

    // ── Endpoints ───────────────────────────────────────────────────────────

    /// `GET /health` (no auth) — liveness probe.
    pub async fn health(&self) -> anyhow::Result<serde_json::Value> {
        let resp = self.http.get(self.url("/health")).send().await?;
        decode_json(resp).await
    }

    /// `GET /v1/models` — agents this token's scope may address.
    pub async fn models(&self) -> anyhow::Result<Vec<Model>> {
        let resp = self
            .http
            .get(self.url("/v1/models"))
            .bearer_auth(self.gateway_token()?)
            .send()
            .await?;
        let list: ModelList = decode_json(resp).await?;
        Ok(list.data)
    }

    /// `POST /v1/tasks` — submit an async task; returns the full record.
    pub async fn task_submit(&self, req: &TaskSubmit) -> anyhow::Result<TaskRecord> {
        let resp = self
            .http
            .post(self.url("/v1/tasks"))
            .bearer_auth(self.gateway_token()?)
            .json(req)
            .send()
            .await?;
        decode_json(resp).await
    }

    /// `GET /v1/tasks?agent=&state=&limit=&after=` — task summaries.
    pub async fn task_list(
        &self,
        agent: Option<&str>,
        state: Option<&str>,
        limit: Option<u32>,
        after: Option<&str>,
    ) -> anyhow::Result<TaskList> {
        let mut req = self
            .http
            .get(self.url("/v1/tasks"))
            .bearer_auth(self.gateway_token()?);
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(a) = agent {
            query.push(("agent", a.to_string()));
        }
        if let Some(s) = state {
            query.push(("state", s.to_string()));
        }
        if let Some(l) = limit {
            query.push(("limit", l.to_string()));
        }
        if let Some(a) = after {
            query.push(("after", a.to_string()));
        }
        if !query.is_empty() {
            req = req.query(&query);
        }
        decode_json(req.send().await?).await
    }

    /// `GET /v1/tasks/{id}` — full record.
    pub async fn task_get(&self, id: &str) -> anyhow::Result<TaskRecord> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/tasks/{id}")))
            .bearer_auth(self.gateway_token()?)
            .send()
            .await?;
        decode_json(resp).await
    }

    /// `GET /v1/tasks/{id}/output` — the raw text output spool.
    pub async fn task_output(&self, id: &str) -> anyhow::Result<String> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/tasks/{id}/output")))
            .bearer_auth(self.gateway_token()?)
            .send()
            .await?;
        decode_text(resp).await
    }

    /// `POST /v1/tasks/{id}/cancel` — idempotent cancel; returns updated record.
    pub async fn task_cancel(&self, id: &str) -> anyhow::Result<TaskRecord> {
        let resp = self
            .http
            .post(self.url(&format!("/v1/tasks/{id}/cancel")))
            .bearer_auth(self.gateway_token()?)
            .send()
            .await?;
        decode_json(resp).await
    }

    /// `DELETE /v1/tasks/{id}` — remove a terminal task (409 if not terminal).
    pub async fn task_delete(&self, id: &str) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .http
            .delete(self.url(&format!("/v1/tasks/{id}")))
            .bearer_auth(self.gateway_token()?)
            .send()
            .await?;
        decode_json(resp).await
    }

    /// `GET /dashboard/api/overview` — fleet summary (separate dashboard token).
    pub async fn overview(&self) -> anyhow::Result<serde_json::Value> {
        let resp = self
            .http
            .get(self.url("/dashboard/api/overview"))
            .bearer_auth(self.dashboard_token()?)
            .send()
            .await?;
        decode_json(resp).await
    }
}

// ── Response decoding ───────────────────────────────────────────────────────

/// Map a gateway response into `T`, translating non-2xx bodies through the
/// `{"error":{"message","type"}}` envelope into a descriptive error.
async fn decode_json<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> anyhow::Result<T> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!("hermes-gateway: could not decode {status} response: {e}; body: {body}")
        })
    } else {
        Err(envelope_error(status, &body, retry_after))
    }
}

async fn decode_text(resp: reqwest::Response) -> anyhow::Result<String> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(body)
    } else {
        Err(envelope_error(status, &body, retry_after))
    }
}

/// Build an error from a non-2xx response, preferring the gateway's
/// `{"error":{"message","type"}}` envelope and appending `Retry-After` when the
/// gateway signalled backpressure (429/503).
pub fn envelope_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after: Option<String>,
) -> anyhow::Error {
    let parsed: Option<(String, String)> = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let err = v.get("error")?;
            let msg = err.get("message").and_then(|m| m.as_str())?.to_string();
            let ty = err
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            Some((msg, ty))
        });
    let retry = retry_after
        .map(|r| format!(" (Retry-After: {r})"))
        .unwrap_or_default();
    match parsed {
        Some((msg, ty)) if !ty.is_empty() => {
            anyhow::anyhow!("hermes-gateway {status}: {msg} [{ty}]{retry}")
        }
        Some((msg, _)) => anyhow::anyhow!("hermes-gateway {status}: {msg}{retry}"),
        None => anyhow::anyhow!("hermes-gateway {status}: {body}{retry}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_trims_trailing_slash() {
        let c = HermesClient::new("http://gw:8642/", None, None);
        assert_eq!(c.url("/health"), "http://gw:8642/health");
    }

    #[test]
    fn from_config_requires_url() {
        let cfg = AppConfig {
            database_url: "x".into(),
            context_limit_tokens: 1,
            context_hard_cap_tokens: 1,
            lock_ttl_secs: 1,
            heartbeat_interval_secs: 1,
            watcher_interval_secs: 1,
            rtk_binary_path: "rtk".into(),
            hermes_gateway_url: None,
            hermes_gateway_token: Some("t".into()),
            hermes_dashboard_token: None,
            mnemosyne_mcp_url: None,
            mnemosyne_mcp_token: None,
            mnemosyne_mcp_bank: None,
            mnemosyne_export_dir: None,
        };
        let err = HermesClient::from_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("HERMES_GATEWAY_URL"));
    }

    #[test]
    fn envelope_error_prefers_typed_message() {
        let body = r#"{"error":{"message":"Agent x is busy","type":"rate_limit_error"}}"#;
        let e = envelope_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            body,
            Some("15".into()),
        )
        .to_string();
        assert!(e.contains("Agent x is busy"));
        assert!(e.contains("rate_limit_error"));
        assert!(e.contains("Retry-After: 15"));
    }

    #[test]
    fn envelope_error_falls_back_to_body() {
        let e = envelope_error(reqwest::StatusCode::BAD_GATEWAY, "no vm", None).to_string();
        assert!(e.contains("502"));
        assert!(e.contains("no vm"));
    }

    #[test]
    fn task_record_terminal_classification() {
        for (s, term) in [
            ("succeeded", true),
            ("failed", true),
            ("cancelled", true),
            ("expired", true),
            ("pending", false),
            ("running", false),
        ] {
            let r = TaskRecord {
                id: "t".into(),
                agent: None,
                state: s.into(),
                priority: None,
                attempts: None,
                created_at: None,
                updated_at: None,
                session_id: None,
                already_terminal: None,
                result: None,
                error: None,
                extra: Default::default(),
            };
            assert_eq!(r.is_terminal(), term, "state {s}");
        }
    }
}
