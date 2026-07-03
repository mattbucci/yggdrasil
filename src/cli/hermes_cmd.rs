//! `ygg hermes <action>` — control and monitor the hermes-gateway orchestrator
//! layer from yggdrasil. Every subcommand talks to a running gateway over HTTP
//! via [`crate::hermes::HermesClient`]; only `back` touches the local DB (it
//! marks a DAG task for hermes-backed dispatch — see `hermes_backend`).
//!
//! Read commands accept `--json` for raw passthrough (mirrors `rollup`'s Format
//! convention). The gateway needs `HERMES_GATEWAY_URL` plus a bearer token; the
//! `status` command additionally needs the separate `HERMES_DASHBOARD_TOKEN`.

use clap::Subcommand;
use serde_json::Value;

use crate::config::AppConfig;
use crate::hermes::{HermesClient, TaskSubmit};

#[derive(Subcommand)]
pub enum HermesAction {
    /// Fleet summary from the ops dashboard (per-agent VM/queue state, task
    /// counts, traffic). Needs HERMES_DASHBOARD_TOKEN.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List agents this gateway token is scoped to (GET /v1/models).
    Agents {
        #[arg(long)]
        json: bool,
    },
    /// Submit an async task to a gateway agent (POST /v1/tasks).
    Submit {
        /// Agent/model id (e.g. feature-dev, hermes).
        agent: String,
        /// Prompt sent as the task `input`.
        prompt: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long = "timeout-s")]
        timeout_s: Option<i64>,
        #[arg(long = "deadline-s")]
        deadline_s: Option<i64>,
        #[arg(long = "max-attempts")]
        max_attempts: Option<i64>,
        #[arg(long = "session-id")]
        session_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List tasks on the gateway (GET /v1/tasks).
    Tasks {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value = "50")]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Show a full task record (GET /v1/tasks/{id}).
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Stream a task's output spool to stdout (GET /v1/tasks/{id}/output).
    Output { id: String },
    /// Cancel a task (POST /v1/tasks/{id}/cancel; idempotent).
    Cancel {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete a terminal task (DELETE /v1/tasks/{id}).
    Rm { id: String },
    /// Gateway liveness (GET /health; no auth).
    Health {
        #[arg(long)]
        json: bool,
    },
    /// Mark a local DAG task for hermes-backed dispatch (or clear it). The
    /// scheduler then submits it to the gateway instead of spawning tmux.
    Back {
        /// Task reference (e.g. yggdrasil-42 or a task UUID).
        task_ref: String,
        /// Gateway agent to route to (defaults to YGG_HERMES_DEFAULT_AGENT
        /// or feature-dev).
        #[arg(long)]
        agent: Option<String>,
        /// Clear the hermes backend, restoring default tmux dispatch.
        #[arg(long)]
        off: bool,
    },
}

/// Dispatch a `ygg hermes` subcommand. Builds a fresh client per invocation.
pub async fn handle(config: &AppConfig, action: HermesAction) -> anyhow::Result<()> {
    match action {
        HermesAction::Back {
            task_ref,
            agent,
            off,
        } => back(config, &task_ref, agent.as_deref(), off).await,

        HermesAction::Health { json } => {
            let client = HermesClient::from_config(config)?;
            let v = client.health().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
                println!("gateway {}: {status}", client_base(config));
            }
            Ok(())
        }

        HermesAction::Agents { json } => {
            let client = HermesClient::from_config(config)?;
            let models = client.models().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&models)?);
            } else if models.is_empty() {
                println!("(no agents exposed to this token)");
            } else {
                println!("{} agent(s):", models.len());
                for m in models {
                    println!("  {}", m.id);
                }
            }
            Ok(())
        }

        HermesAction::Submit {
            agent,
            prompt,
            priority,
            timeout_s,
            deadline_s,
            max_attempts,
            session_id,
            json,
        } => {
            let client = HermesClient::from_config(config)?;
            let req = TaskSubmit {
                agent,
                input: prompt,
                priority,
                timeout_s,
                deadline_s,
                max_attempts,
                not_before: None,
                session_id,
            };
            let rec = client.task_submit(&req).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                println!(
                    "submitted {} [{}] state={}",
                    rec.id,
                    req_agent(&rec),
                    rec.state
                );
            }
            Ok(())
        }

        HermesAction::Tasks {
            agent,
            state,
            limit,
            json,
        } => {
            let client = HermesClient::from_config(config)?;
            let list = client
                .task_list(agent.as_deref(), state.as_deref(), Some(limit), None)
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list.data)?);
            } else {
                print_task_table(&list.data);
                if list.has_more {
                    println!("(more — raise --limit or page with the gateway API)");
                }
            }
            Ok(())
        }

        HermesAction::Show { id, json } => {
            let client = HermesClient::from_config(config)?;
            let rec = client.task_get(&id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                print_record(&rec);
            }
            Ok(())
        }

        HermesAction::Output { id } => {
            let client = HermesClient::from_config(config)?;
            let text = client.task_output(&id).await?;
            print!("{text}");
            Ok(())
        }

        HermesAction::Cancel { id, json } => {
            let client = HermesClient::from_config(config)?;
            let rec = client.task_cancel(&id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rec)?);
            } else {
                let terminal = rec.already_terminal.unwrap_or(false);
                let note = if terminal { " (already terminal)" } else { "" };
                println!("{} state={}{note}", rec.id, rec.state);
            }
            Ok(())
        }

        HermesAction::Rm { id } => {
            let client = HermesClient::from_config(config)?;
            client.task_delete(&id).await?;
            println!("deleted {id}");
            Ok(())
        }

        HermesAction::Status { json } => status(config, json).await,
    }
}

fn client_base(config: &AppConfig) -> &str {
    config.hermes_gateway_url.as_deref().unwrap_or("<unset>")
}

fn req_agent(rec: &crate::hermes::TaskRecord) -> String {
    rec.agent.clone().unwrap_or_else(|| "?".to_string())
}

/// Render the dashboard overview. Uses the raw Value so the client stays robust
/// to gateway-side shape drift; `--json` prints it verbatim.
async fn status(config: &AppConfig, json: bool) -> anyhow::Result<()> {
    if config.hermes_dashboard_token.is_none() {
        anyhow::bail!(
            "HERMES_DASHBOARD_TOKEN is not set — `ygg hermes status` reads the ops \
             dashboard API, which requires its own bearer token (separate from \
             HERMES_GATEWAY_TOKEN). Set it in ~/.config/ygg/.env."
        );
    }
    let client = HermesClient::from_config(config)?;
    let ov = client.overview().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ov)?);
        return Ok(());
    }

    println!("hermes fleet @ {}", client_base(config));
    if let Some(gw) = ov.get("gateway") {
        let ver = gw.get("version").and_then(Value::as_str).unwrap_or("?");
        let up = gw.get("uptime_s").and_then(Value::as_i64).unwrap_or(0);
        let pid = gw.get("pid").and_then(Value::as_i64).unwrap_or(0);
        println!("  gateway: v{ver}  up {up}s  pid {pid}");
    }

    if let Some(agents) = ov.get("agents").and_then(Value::as_array) {
        println!("  agents ({}):", agents.len());
        for a in agents {
            let name = a.get("agent").and_then(Value::as_str).unwrap_or("?");
            let vm_alive = a
                .get("vm")
                .and_then(|v| v.get("alive"))
                .and_then(Value::as_bool);
            let vm = match vm_alive {
                Some(true) => "vm:up",
                Some(false) => "vm:down",
                None => "vm:none",
            };
            let limit = a.get("limit").and_then(Value::as_i64).unwrap_or(0);
            let running = a
                .get("running")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let waiting = a
                .get("waiting")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let last_err = a
                .get("last_error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let err = last_err
                .map(|e| format!("  last_error: {e}"))
                .unwrap_or_default();
            println!(
                "    {name:<16} {vm:<8} limit {limit}  running {running}  waiting {waiting}{err}"
            );
        }
    }

    if let Some(tbs) = ov.get("tasks_by_state").and_then(Value::as_object) {
        let parts: Vec<String> = tbs
            .iter()
            .map(|(k, v)| format!("{k} {}", v.as_i64().unwrap_or(0)))
            .collect();
        println!("  tasks: {}", parts.join("  "));
    }

    if let Some(t) = ov.get("totals") {
        let reqs = t.get("reqs_1m").and_then(Value::as_i64).unwrap_or(0);
        let errs = t.get("errors_1m").and_then(Value::as_i64).unwrap_or(0);
        let p95 = t.get("p95_ms_1m").and_then(Value::as_f64).unwrap_or(0.0);
        println!("  totals(1m): reqs {reqs}  errors {errs}  p95 {p95:.0}ms");
    }
    Ok(())
}

fn print_task_table(rows: &[crate::hermes::TaskSummary]) {
    if rows.is_empty() {
        println!("(no tasks)");
        return;
    }
    println!(
        "{:<28} {:<14} {:<10} {:>4} {:>7} {:>4}",
        "ID", "AGENT", "STATE", "PRIO", "AGE_S", "ATT"
    );
    for r in rows {
        let agent = r.agent.as_deref().unwrap_or("-");
        let age = r.age_s.map(|a| format!("{a:.0}")).unwrap_or_default();
        println!(
            "{:<28} {:<14} {:<10} {:>4} {:>7} {:>4}",
            truncate(&r.id, 28),
            truncate(agent, 14),
            truncate(&r.state, 10),
            r.priority.unwrap_or(0),
            age,
            r.attempts.unwrap_or(0),
        );
    }
}

fn print_record(rec: &crate::hermes::TaskRecord) {
    println!("task {}", rec.id);
    println!("  agent:    {}", rec.agent.as_deref().unwrap_or("-"));
    println!("  state:    {}", rec.state);
    if let Some(p) = rec.priority {
        println!("  priority: {p}");
    }
    if let Some(a) = rec.attempts {
        println!("  attempts: {a}");
    }
    if let Some(c) = &rec.created_at {
        println!("  created:  {c}");
    }
    if let Some(u) = &rec.updated_at {
        println!("  updated:  {u}");
    }
    if let Some(s) = &rec.session_id {
        println!("  session:  {s}");
    }
    if let Some(err) = &rec.error {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(&err.to_string())
            .to_string();
        println!("  error:    {msg}");
    }
    if let Some(res) = &rec.result {
        if let Some(fr) = res.get("finish_reason").and_then(Value::as_str) {
            println!("  finish:   {fr}");
        }
        if let Some(content) = res.get("content").and_then(Value::as_str) {
            println!("  result:   {}", truncate(content, 200));
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let taken: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{taken}…")
}

/// Mark (or clear) a local DAG task's hermes backend selector. Resolves a task
/// reference (`<prefix>-<seq>` or a task UUID) to a `tasks` row and sets its
/// `backend` column; the scheduler reads it at dispatch time.
async fn back(
    config: &AppConfig,
    task_ref: &str,
    agent: Option<&str>,
    off: bool,
) -> anyhow::Result<()> {
    let pool = crate::db::create_pool(&config.database_url).await?;
    let task_id = resolve_task_id(&pool, task_ref).await?;

    if off {
        sqlx::query("UPDATE tasks SET backend = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE task_id = $1")
            .bind(&task_id)
            .execute(&pool)
            .await?;
        println!("{task_ref}: backend cleared (tmux)");
        return Ok(());
    }

    let agent = agent
        .map(str::to_string)
        .unwrap_or_else(crate::cli::hermes_backend::default_agent);
    let backend = format!("hermes:{agent}");
    sqlx::query("UPDATE tasks SET backend = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE task_id = $1")
        .bind(&task_id)
        .bind(&backend)
        .execute(&pool)
        .await?;
    println!("{task_ref}: backend set to {backend}");
    Ok(())
}

/// Resolve a task reference to a task_id. Accepts a full/short UUID or the
/// `<prefix>-<seq>` human ref (mirrors task_cmd's resolver, kept local so the
/// hermes module has no cross-CLI dependency).
async fn resolve_task_id(pool: &crate::db::DbPool, reference: &str) -> anyhow::Result<String> {
    // UUID / short-hex prefix.
    let hex = reference.strip_prefix("ygg-").unwrap_or(reference);
    if hex.len() >= 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let matches: Vec<String> =
            sqlx::query_scalar("SELECT task_id FROM tasks WHERE task_id LIKE $1 LIMIT 5")
                .bind(format!("{hex}%"))
                .fetch_all(pool)
                .await?;
        match matches.len() {
            1 => return Ok(matches.into_iter().next().unwrap()),
            0 => {}
            n => anyhow::bail!("ambiguous task ref '{reference}' ({n} matches)"),
        }
    }
    // <prefix>-<seq>.
    let (prefix, seq_str) = reference
        .rsplit_once('-')
        .ok_or_else(|| anyhow::anyhow!("expected <prefix>-<seq> or a UUID, got {reference}"))?;
    let seq: i32 = seq_str
        .parse()
        .map_err(|_| anyhow::anyhow!("sequence must be an integer: {seq_str}"))?;
    let task_id: Option<String> = sqlx::query_scalar(
        r#"SELECT t.task_id FROM tasks t JOIN repos r USING (repo_id)
           WHERE r.task_prefix = $1 AND t.seq = $2"#,
    )
    .bind(prefix)
    .bind(seq)
    .fetch_optional(pool)
    .await?;
    task_id.ok_or_else(|| anyhow::anyhow!("task {prefix}-{seq} not found"))
}
