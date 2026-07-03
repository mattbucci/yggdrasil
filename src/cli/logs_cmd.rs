use crate::db::DbPool;
use chrono::{DateTime, Utc};

use crate::models::event::{Event, EventKind, EventRepo};

// ANSI colors
const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[38;5;114m";
const YELLOW: &str = "\x1b[38;5;221m";
const CYAN: &str = "\x1b[38;5;81m";
const ORANGE: &str = "\x1b[38;5;208m";
const RED: &str = "\x1b[38;5;203m";
const BLUE: &str = "\x1b[38;5;111m";
const GRAY: &str = "\x1b[38;5;245m";

pub async fn execute(
    pool: &DbPool,
    follow: bool,
    tail: i64,
    agent_name: Option<&str>,
    kinds: Option<Vec<String>>,
    cc_session_id: Option<&str>,
) -> Result<(), anyhow::Error> {
    let _ = EventRepo::new(pool); // reserved for back-compat paths
    let divider = format!("{DIM}{}{RESET}", "─".repeat(72));

    let header_suffix = {
        let mut parts = Vec::new();
        if let Some(n) = agent_name {
            parts.push(format!("agent={n}"));
        }
        if let Some(ks) = &kinds {
            parts.push(format!("kind={}", ks.join(",")));
        }
        if let Some(s) = cc_session_id {
            parts.push(format!("session={s}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("  {DIM}{}{RESET}", parts.join(" · "))
        }
    };
    println!("{divider}");
    println!("  {BOLD}ygg event stream{RESET}{header_suffix}");
    println!("{divider}");

    let mut recent =
        filtered_recent(pool, tail, agent_name, kinds.as_deref(), cc_session_id).await?;
    recent.reverse();
    for event in &recent {
        print_event(event);
    }

    if !follow {
        println!("{DIM}  (use --follow to stream live){RESET}");
        return Ok(());
    }

    println!("{DIM}  streaming… (ctrl-c to exit){RESET}");

    let mut cursor: DateTime<Utc> = recent.last().map(|e| e.created_at).unwrap_or_else(Utc::now);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let new_events = filtered_since(
            pool,
            cursor,
            agent_name,
            kinds.as_deref(),
            cc_session_id,
            50,
        )
        .await?;
        for event in &new_events {
            print_event(event);
            cursor = event.created_at;
        }
    }
}

async fn filtered_recent(
    pool: &DbPool,
    tail: i64,
    agent_name: Option<&str>,
    kinds: Option<&[String]>,
    cc_session_id: Option<&str>,
) -> Result<Vec<Event>, anyhow::Error> {
    let kinds_owned: Vec<String> = kinds.map(|k| k.to_vec()).unwrap_or_default();
    let rows: Vec<Event> = sqlx::query_as::<_, Event>(
        r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
             FROM events
            WHERE ($1 IS NULL OR agent_name = $1)
              AND ($2 IS NULL OR event_kind IN (SELECT value FROM json_each($2)))
              AND ($3 IS NULL OR cc_session_id = $3)
            ORDER BY created_at DESC
            LIMIT $4"#,
    )
    .bind(agent_name)
    .bind(if kinds_owned.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&kinds_owned).unwrap_or_default())
    })
    .bind(cc_session_id)
    .bind(tail)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

async fn filtered_since(
    pool: &DbPool,
    since: DateTime<Utc>,
    agent_name: Option<&str>,
    kinds: Option<&[String]>,
    cc_session_id: Option<&str>,
    limit: i64,
) -> Result<Vec<Event>, anyhow::Error> {
    let kinds_owned: Vec<String> = kinds.map(|k| k.to_vec()).unwrap_or_default();
    let rows: Vec<Event> = sqlx::query_as::<_, Event>(
        r#"SELECT id, event_kind, agent_id, agent_name, payload, created_at
             FROM events
            WHERE created_at > $1
              AND ($2 IS NULL OR agent_name = $2)
              AND ($3 IS NULL OR event_kind IN (SELECT value FROM json_each($3)))
              AND ($4 IS NULL OR cc_session_id = $4)
            ORDER BY created_at ASC
            LIMIT $5"#,
    )
    .bind(since)
    .bind(agent_name)
    .bind(if kinds_owned.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&kinds_owned).unwrap_or_default())
    })
    .bind(cc_session_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Render one event as a single line. We deliberately avoid column
/// alignment: variable-width data (agent names, snippets, numeric counts)
/// makes columnar layouts sheer in ways that are annoying to fix and
/// never look good across every event kind. Instead we emit a scannable
/// delimited form — `time · symbol label · agent · detail` — each field
/// stands on its own, colors segment the fields visually.
fn print_event(event: &Event) {
    let ts = event
        .created_at
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S");
    let (color, symbol) = kind_style(&event.event_kind);
    let label = event.event_kind.label();
    let agent = truncate(&event.agent_name, 24);
    let detail = format_payload(&event.event_kind, &event.payload);

    println!(
        "{DIM}{ts}{RESET} {color}{symbol}{label}{RESET} {SEP} {GRAY}{agent}{RESET} {SEP} {detail}"
    );
}

const SEP: &str = "\x1b[38;5;240m·\x1b[0m";

fn kind_style(kind: &EventKind) -> (&'static str, &'static str) {
    match kind {
        EventKind::LockAcquired => (YELLOW, "⚿"),
        EventKind::LockReleased => (DIM, "○"),
        EventKind::HookFired => (ORANGE, "▸"),
        EventKind::TaskCreated => (GREEN, "✚"),
        EventKind::TaskStatusChanged => (YELLOW, "◆"),
        EventKind::AgentStateChanged => (BLUE, "↪"),
        EventKind::Message => (CYAN, "✉"),
        EventKind::RunScheduled => (DIM, "□"),
        EventKind::RunClaimed => (BLUE, "▶"),
        EventKind::RunTerminal => (GREEN, "■"),
        EventKind::RunRetry => (YELLOW, "↻"),
        EventKind::SchedulerTick => (DIM, "·"),
        EventKind::SchedulerError => (RED, "!"),
        EventKind::AgentStaleWarning => (YELLOW, "⌛"),
    }
}

fn format_payload(kind: &EventKind, p: &serde_json::Value) -> String {
    match kind {
        EventKind::LockAcquired | EventKind::LockReleased => p["resource"]
            .as_str()
            .map(|r| truncate(r, 60).to_string())
            .unwrap_or_default(),
        EventKind::HookFired => p["hook"].as_str().unwrap_or("").to_string(),
        EventKind::TaskCreated => {
            let rref = p["ref"].as_str().unwrap_or("?");
            let title = p["title"].as_str().unwrap_or("");
            let kind = p["kind"].as_str().unwrap_or("task");
            let pri = p["priority"].as_i64().unwrap_or(2);
            format!(
                "{GREEN}{rref}{RESET}  {DIM}P{pri} {kind}{RESET}  {}",
                truncate(title, 50)
            )
        }
        EventKind::TaskStatusChanged => {
            let rref = p["ref"].as_str().unwrap_or("?");
            let to = p["to"].as_str().unwrap_or("?");
            let reason = p["reason"].as_str();
            let extra = reason
                .map(|r| format!("  {DIM}{}{RESET}", truncate(r, 40)))
                .unwrap_or_default();
            format!("{YELLOW}{rref}{RESET} → {to}{extra}")
        }
        EventKind::AgentStateChanged => {
            let from = p["from"].as_str().unwrap_or("?");
            let to = p["to"].as_str().unwrap_or("?");
            let tool = p["tool"].as_str();
            let suffix = tool
                .map(|t| format!(" {DIM}({t}){RESET}"))
                .unwrap_or_default();
            format!("{BLUE}{from}{RESET} → {to}{suffix}")
        }
        EventKind::Message => {
            let body = p["body"].as_str().unwrap_or("");
            format!("{CYAN}msg{RESET}  {}", truncate(body, 60))
        }
        EventKind::RunScheduled => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let attempt = p["attempt"].as_i64().unwrap_or(1);
            format!("{DIM}{task} attempt={attempt}{RESET}")
        }
        EventKind::RunClaimed => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let agent = p["agent"].as_str().unwrap_or("?");
            format!("{BLUE}{task}{RESET} → {agent}")
        }
        EventKind::RunTerminal => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let state = p["state"].as_str().unwrap_or("?");
            let reason = p["reason"].as_str();
            let color = match state {
                "succeeded" => GREEN,
                "failed" | "crashed" | "poison" => RED,
                "cancelled" => DIM,
                _ => GRAY,
            };
            let reason_extra = reason
                .filter(|r| *r != "ok")
                .map(|r| format!("  {DIM}{}{RESET}", r))
                .unwrap_or_default();
            format!("{color}{task}{RESET} {state}{reason_extra}")
        }
        EventKind::RunRetry => {
            let task = p["task_ref"].as_str().unwrap_or("?");
            let attempt = p["attempt"].as_i64().unwrap_or(0);
            let backoff_ms = p["backoff_ms"].as_i64().unwrap_or(0);
            format!("{YELLOW}{task}{RESET} attempt {attempt} after {DIM}{backoff_ms}ms{RESET}")
        }
        EventKind::SchedulerTick => {
            let spawned = p["dispatched"].as_i64().unwrap_or(0);
            let done = p["finalized"].as_i64().unwrap_or(0);
            let retried = p["retried"].as_i64().unwrap_or(0);
            format!("{DIM}spawned={spawned} done={done} retried={retried}{RESET}")
        }
        EventKind::SchedulerError => {
            let msg = p["error"].as_str().unwrap_or("");
            format!("{RED}{}{RESET}", truncate(msg, 70))
        }
        EventKind::AgentStaleWarning => {
            let state = p["current_state"].as_str().unwrap_or("?");
            let last = p["last_update"].as_str().unwrap_or("");
            format!(
                "{YELLOW}stale {state}{RESET} {DIM}last={}{RESET}",
                truncate(last, 25)
            )
        }
    }
}

/// Truncate at a char boundary, not a byte boundary. Prevents panics on
/// multi-byte glyphs (e.g. snippets with ─ box-drawing chars from our own
/// log output that made its way into the DAG as a node).
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Walk backwards from `max` to the nearest char boundary.
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
