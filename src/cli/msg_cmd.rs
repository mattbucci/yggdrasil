//! Agent-to-agent messaging: inbox pattern on the `events` table.
//!
//! send  : write a `message` event with recipient_agent_id; optionally
//!         tmux send-keys the body into the recipient's pane for instant
//!         delivery (`--push`).
//! inbox : list messages newer than the caller's `agents.message_cursor`.
//!         Default output is human-readable; the `prompt-submit` hook
//!         calls this and pipes the block directly into the worker's
//!         next user turn.
//! mark-read : bump the cursor to the latest message's timestamp. The
//!         hook calls this after injecting.

use crate::config::AppConfig;
use crate::db::DbPool;
use crate::models::agent::AgentRepo;
use crate::tmux::TmuxManager;
use chrono::{DateTime, Utc};
use std::process::Command;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Message {
    pub id: String,
    pub from_agent_id: Option<String>,
    pub from_agent_name: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

pub async fn send(
    pool: &DbPool,
    from_agent_name: &str,
    to_agent_name: &str,
    body: &str,
    push: bool,
) -> Result<(), anyhow::Error> {
    send_inner(pool, from_agent_name, to_agent_name, body, push, false).await
}

pub async fn send_inner(
    pool: &DbPool,
    from_agent_name: &str,
    to_agent_name: &str,
    body: &str,
    push: bool,
    no_spawn: bool,
) -> Result<(), anyhow::Error> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let from = repo.get_by_name(from_agent_name).await?;
    let to = repo.get_by_name(to_agent_name).await?;

    // Record the message event regardless of spawn outcome.
    let (to_id, recipient_exists) = match &to {
        Some(a) => (Some(a.agent_id.clone()), true),
        None => (None, false),
    };
    let from_id = from.as_ref().map(|a| a.agent_id.clone());
    let payload = serde_json::json!({ "body": body });

    if let Some(tid) = to_id {
        sqlx::query(
            "INSERT INTO events (event_kind, agent_id, agent_name, recipient_agent_id, payload)
             VALUES ('message', $1, $2, $3, $4)",
        )
        .bind(&from_id)
        .bind(from_agent_name)
        .bind(tid)
        .bind(&payload)
        .execute(pool)
        .await?;
    }

    // Detect whether the recipient has a live tmux window. Propagate tmux
    // errors so transient failures don't trigger a spurious spawn — except
    // under --no-spawn, where a missing/broken tmux simply means "inactive"
    // (the message row is already recorded above).
    let has_window = if recipient_exists {
        match TmuxManager::has_agent_window(to_agent_name).await {
            Ok(v) => v,
            Err(_) if no_spawn => false,
            Err(e) => return Err(e.into()),
        }
    } else {
        false
    };

    if has_window {
        println!("sent to {to_agent_name}");
        if push {
            let _ = push_via_tmux(to_agent_name, body);
        }
    } else if no_spawn {
        if !recipient_exists {
            anyhow::bail!("recipient agent '{to_agent_name}' not found (--no-spawn)");
        }
        println!("sent to {to_agent_name} (inactive, --no-spawn)");
    } else {
        // Auto-spawn: start a fresh worker with the message as its task prompt.
        let config = AppConfig::from_env()?;
        let task_prompt = format!("[message from {from_agent_name}] {body}");
        println!("spawning '{to_agent_name}' (inactive) …");
        let spawn_result =
            super::spawn::execute(pool, &config, &task_prompt, Some(to_agent_name)).await;

        // Record the message event even if spawn partially failed (the agent
        // row may already exist from spawn::execute's register call).
        if !recipient_exists && let Some(new_agent) = repo.get_by_name(to_agent_name).await? {
            sqlx::query(
                "INSERT INTO events (event_kind, agent_id, agent_name, recipient_agent_id, payload)
                     VALUES ('message', $1, $2, $3, $4)",
            )
            .bind(from_id)
            .bind(from_agent_name)
            .bind(new_agent.agent_id)
            .bind(&payload)
            .execute(pool)
            .await?;
        }
        spawn_result?;
    }

    Ok(())
}

/// List messages that arrived after the agent's message_cursor. Does NOT
/// advance the cursor — callers (the hook) invoke `mark_read` after
/// successful injection. Pass `all=true` to see every message regardless
/// of cursor position.
pub async fn inbox(
    pool: &DbPool,
    agent_name: &str,
    all: bool,
) -> Result<Vec<Message>, anyhow::Error> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{agent_name}' not found"))?;

    // Read cursor inline — AgentRepo doesn't expose it and we shouldn't
    // add a single-shot getter for something this narrow.
    let cursor: Option<DateTime<Utc>> = if all {
        None
    } else {
        sqlx::query_scalar("SELECT message_cursor FROM agents WHERE agent_id = $1")
            .bind(&agent.agent_id)
            .fetch_optional(pool)
            .await?
            .flatten()
    };

    let rows: Vec<Message> = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            String,
            serde_json::Value,
            DateTime<Utc>,
        ),
    >(
        r#"SELECT id, agent_id, agent_name, payload, created_at
             FROM events
            WHERE event_kind = 'message'
              AND recipient_agent_id = $1
              AND ($2 IS NULL OR created_at > $2)
            ORDER BY created_at ASC, rowid ASC
            LIMIT 200"#,
    )
    .bind(&agent.agent_id)
    .bind(cursor)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(id, aid, aname, payload, ts)| Message {
        id,
        from_agent_id: aid,
        from_agent_name: aname,
        body: payload
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        created_at: ts,
    })
    .collect();

    Ok(rows)
}

/// Advance the cursor to `now`. Called by the hook after the block has
/// been injected so the same messages don't resurface next turn.
pub async fn mark_read(pool: &DbPool, agent_name: &str) -> Result<(), anyhow::Error> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{agent_name}' not found"))?;
    sqlx::query("UPDATE agents SET message_cursor = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE agent_id = $1")
        .bind(agent.agent_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Pretty-print inbox results. The hook consumes stdout directly so the
/// formatting doubles as the prompt-injection block.
pub fn print_inbox(msgs: &[Message]) {
    if msgs.is_empty() {
        println!("inbox empty");
        return;
    }
    for m in msgs {
        let ts = m.created_at.format("%Y-%m-%d %H:%M");
        let from = if m.from_agent_name.is_empty() {
            "—"
        } else {
            &m.from_agent_name
        };
        println!("[ygg msg | from {from} | {ts}] {}", m.body);
    }
}

/// Send a broadcast message (no specific recipient). Any agent can claim it.
pub async fn broadcast(
    pool: &DbPool,
    from_agent_name: &str,
    body: &str,
) -> Result<String, anyhow::Error> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let from = repo.get_by_name(from_agent_name).await?;
    let from_id = from.as_ref().map(|a| a.agent_id.clone());
    let payload = serde_json::json!({ "body": body });

    let id: String = sqlx::query_scalar(
        "INSERT INTO events (event_kind, agent_id, agent_name, recipient_agent_id, payload)
         VALUES ('message', $1, $2, NULL, $3)
         RETURNING id",
    )
    .bind(from_id)
    .bind(from_agent_name)
    .bind(&payload)
    .fetch_one(pool)
    .await?;

    println!("broadcast sent (id: {id})");
    Ok(id)
}

/// Claim an unclaimed broadcast message for a specific agent.
pub async fn claim_broadcast(
    pool: &DbPool,
    event_id: String,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{agent_name}' not found"))?;

    let rows = sqlx::query(
        "UPDATE events SET recipient_agent_id = $1
          WHERE id = $2
            AND event_kind = 'message'
            AND recipient_agent_id IS NULL",
    )
    .bind(agent.agent_id)
    .bind(&event_id)
    .execute(pool)
    .await?
    .rows_affected();

    if rows == 0 {
        anyhow::bail!("message {event_id} not found or already claimed");
    }
    println!("claimed by {agent_name}");
    Ok(())
}

/// Fetch all recent messages (directed + broadcast) for the TUI chat panel.
pub async fn all_messages(
    pool: &DbPool,
    hours: i64,
    limit: i64,
) -> Result<Vec<ChatMessage>, anyhow::Error> {
    let rows: Vec<(
        String,
        Option<String>,
        String,
        Option<String>,
        serde_json::Value,
        DateTime<Utc>,
    )> = sqlx::query_as(
        r#"SELECT e.id, e.agent_id, e.agent_name,
                      a.agent_name AS to_name,
                      e.payload, e.created_at
                 FROM events e
                 LEFT JOIN agents a ON a.agent_id = e.recipient_agent_id
                WHERE e.event_kind = 'message'
                  AND e.created_at > strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || ($1 * 60) || ' minutes')
                ORDER BY e.created_at DESC
                LIMIT $2"#,
    )
    .bind(hours)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(id, _from_id, from_name, to_name, payload, ts)| ChatMessage {
                id,
                from_name,
                to_name,
                body: payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: ts,
            },
        )
        .collect())
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: String,
    pub from_name: String,
    pub to_name: Option<String>,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

fn push_via_tmux(to_agent: &str, body: &str) -> Result<(), anyhow::Error> {
    // Find a tmux session whose name starts with `ygg-<to_agent>·` — the
    // naming convention plan_cmd::sanitize_tmux_name uses. First match wins.
    let out = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("tmux list-sessions failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let prefix = format!("ygg-{to_agent}");
    let target = text
        .lines()
        .find(|l| l.starts_with(&prefix))
        .ok_or_else(|| anyhow::anyhow!("no tmux session for agent '{to_agent}'"))?;
    // Deliver as a single line of input — works for both input-mode and
    // pane-at-prompt. Caller chose --push knowing it might interrupt.
    Command::new("tmux")
        .args(["send-keys", "-t", target, body, "Enter"])
        .status()?;
    Ok(())
}
