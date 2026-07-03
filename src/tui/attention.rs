//! Attention items — things that want a human (yggdrasil-130).
//!
//! Three categories surface in the attention pane / tab badge:
//!
//!   1. agents in `waiting_tool` / hung > N min — the user owes them a
//!      Yes/No on a tool call;
//!   2. runs that are `awaiting_review` (approval gate, ADR 0016 §D8);
//!   3. runs that have been auto-nudged ≥ 2 times — likely stuck in
//!      a loop the scheduler can't break.
//!
//! The pane's value-prop is: instead of scanning four other panes for
//! "anything I need to do", this tab is the punch list. The numeric
//! badge on the tab title = sum of the three counts.

use crate::db::DbPool;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionKind {
    /// Agent's current_state is a tool-wait that's older than the
    /// threshold — the human probably needs to read a prompt.
    AgentWaitingTool { agent: String, idle_secs: i64 },
    /// Run is parked behind an approval gate (approve_plan or
    /// approve_completion).
    RunAwaitingReview { task_ref: String, run_id: String },
    /// Run has been nudged ≥ 2 times — likely a recover-loop the
    /// scheduler can't break alone.
    RunRepeatedlyNudged { task_ref: String, nudges: i32 },
}

#[derive(Debug, Clone)]
pub struct AttentionItem {
    pub kind: AttentionKind,
    pub since: DateTime<Utc>,
}

/// Idle threshold (secs) past which a `waiting_tool` agent counts as
/// "human attention required". 600 s (10 min) matches the engagement
/// research's "hung > 10 min" heuristic.
pub const WAITING_TOOL_THRESHOLD_SECS: i64 = 600;

/// Threshold after which a run with N nudges counts as repeatedly-stuck.
pub const NUDGE_THRESHOLD: i32 = 2;

/// Single roundtrip that pulls all three categories. Cheap enough for
/// the existing 500ms refresh tick; the queries are bounded (each
/// LIMIT 50) so a runaway dataset can't pin the TUI.
pub async fn fetch_attention_items(pool: &DbPool) -> Result<Vec<AttentionItem>, sqlx::Error> {
    let mut out: Vec<AttentionItem> = Vec::new();

    // (1) Tool-waiting agents, idle longer than the threshold.
    let waiting: Vec<(String, DateTime<Utc>, i64)> = sqlx::query_as(
        r#"SELECT agent_name, updated_at,
                  CAST((julianday('now') - julianday(updated_at)) * 86400 AS INTEGER) AS idle_secs
             FROM agents
            WHERE archived_at IS NULL
              AND current_state = 'waiting_tool'
              AND updated_at < strftime('%Y-%m-%dT%H:%M:%f+00:00','now','-' || $1 || ' seconds')
            ORDER BY updated_at ASC
            LIMIT 50"#,
    )
    .bind(WAITING_TOOL_THRESHOLD_SECS)
    .fetch_all(pool)
    .await?;
    for (agent, since, idle) in waiting {
        out.push(AttentionItem {
            since,
            kind: AttentionKind::AgentWaitingTool {
                agent,
                idle_secs: idle,
            },
        });
    }

    // (2) Runs awaiting review — approve_plan / approve_completion
    // gates encoded as task.approval_level + tasks.approved_at NULL.
    let awaiting: Vec<(String, String, i32, DateTime<Utc>)> = sqlx::query_as(
        r#"SELECT tr.run_id, r.task_prefix, t.seq, COALESCE(tr.scheduled_at, t.updated_at)
             FROM task_runs tr
             JOIN tasks t ON t.task_id = tr.task_id
             JOIN repos r ON r.repo_id = t.repo_id
            WHERE t.approval_level IN ('approve_plan', 'approve_completion')
              AND t.approved_at IS NULL
              AND tr.state IN ('ready', 'running')
            ORDER BY tr.scheduled_at ASC
            LIMIT 50"#,
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for (run_id, prefix, seq, since) in awaiting {
        out.push(AttentionItem {
            since,
            kind: AttentionKind::RunAwaitingReview {
                task_ref: format!("{prefix}-{seq}"),
                run_id,
            },
        });
    }

    // (3) Runs repeatedly nudged. nudges_count column may not yet
    // exist; tolerate via best-effort query.
    let nudged: Vec<(String, i32, i32, DateTime<Utc>)> = sqlx::query_as(
        r#"SELECT r.task_prefix, t.seq, COALESCE(tr.attempt, 1), tr.scheduled_at
             FROM task_runs tr
             JOIN tasks t ON t.task_id = tr.task_id
             JOIN repos r ON r.repo_id = t.repo_id
            WHERE tr.state = 'running'
              AND tr.attempt >= $1
            ORDER BY tr.scheduled_at ASC
            LIMIT 50"#,
    )
    .bind(NUDGE_THRESHOLD)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for (prefix, seq, attempt, since) in nudged {
        out.push(AttentionItem {
            since,
            kind: AttentionKind::RunRepeatedlyNudged {
                task_ref: format!("{prefix}-{seq}"),
                nudges: attempt,
            },
        });
    }

    Ok(out)
}

/// Map an `AttentionItem` to a one-line summary. Kept separate from
/// the renderer so the logs / status-strip / desktop-notify channels
/// can share the format.
pub fn item_label(item: &AttentionItem) -> String {
    match &item.kind {
        AttentionKind::AgentWaitingTool { agent, idle_secs } => {
            format!("agent {agent} waiting on tool · {idle_secs}s idle")
        }
        AttentionKind::RunAwaitingReview { task_ref, .. } => {
            format!("{task_ref}: awaiting review")
        }
        AttentionKind::RunRepeatedlyNudged { task_ref, nudges } => {
            format!("{task_ref}: nudged {nudges}× — likely stuck")
        }
    }
}

/// Total attention count for the badge on the tab title. Front-end
/// caches this between refreshes so the badge updates as items
/// resolve without re-querying every paint.
pub fn count(items: &[AttentionItem]) -> usize {
    items.len()
}
