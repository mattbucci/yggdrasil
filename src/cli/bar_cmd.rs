//! `ygg bar` — status-bar line generator. Reads Claude Code's statusline
//! JSON from stdin (`{session_id, transcript_path, workspace: {current_dir},
//! ...}`), parses the session transcript for token totals + context size, and
//! appends this agent's Yggdrasil state (state, locks held, other active
//! agents) joined from the DB.
//!
//! Format (per user feedback — match the liked shell statusline, then add ygg):
//!   ↑<in> ↓<out> cache:<X> │ ctx:<N> (pct%) │ ygg:<agent> <state> · N locks · M agents
//!
//! - ↑/↓/cache are summed across every assistant turn (session totals);
//!   ctx is the last turn's window occupancy (input + cache), like CC's own bar.
//! - The ygg segment is best-effort: if this session has no matching agent
//!   row (e.g. a plain `claude` session that never ran a ygg hook), it's
//!   omitted and the token line still renders.
//!
//! Designed to be fast: a single DB round of small queries, opened per
//! invocation. Refreshes every 3s (Claude Code statusLine default).

use crate::lock::LockManager;
use crate::models::agent::{AgentRepo, AgentState};
use chrono::{Duration, Utc};
use std::io::{BufRead, Read};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";

/// Context-window size used to compute the ctx percentage. The 1M window is
/// the tier this user runs; the liked shell statusline hard-codes the same.
const CTX_LIMIT: i64 = 1_000_000;

/// An agent counts as "active" (toward the M-agents tally) when it is not
/// shut down *and* was touched within this window — drops stale idle rows.
const ACTIVE_WINDOW_MINS: i64 = 15;

pub async fn execute(pool: &crate::db::DbPool) -> Result<(), anyhow::Error> {
    // Read Claude Code's JSON payload from stdin. Non-fatal if absent.
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let j: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);

    // ── Token segment (matches the liked shell statusline) ──────────────────
    let transcript = j.get("transcript_path").and_then(|v| v.as_str());
    let Totals { ti, to, tc, ctx } = transcript
        .map(|p| transcript_totals(std::path::Path::new(p)))
        .unwrap_or_default();

    let pct = if CTX_LIMIT > 0 {
        (ctx * 100 / CTX_LIMIT).clamp(0, 100)
    } else {
        0
    };
    let ctx_color = if pct < 50 {
        GREEN
    } else if pct <= 80 {
        YELLOW
    } else {
        RED
    };

    let mut line = format!(
        "{CYAN}↑{}{RESET} {GREEN}↓{}{RESET} {DIM}cache:{}{RESET} {DIM}│{RESET} \
         {ctx_color}ctx:{} ({pct}%){RESET}",
        fmt_k(ti),
        fmt_k(to),
        fmt_k(tc),
        fmt_k(ctx),
    );

    // ── ygg state segment (best-effort) ─────────────────────────────────────
    if let Some(seg) = ygg_segment(pool, &j).await {
        line.push_str(&format!(" {DIM}│{RESET} {seg}"));
    }

    println!("{line}");
    Ok(())
}

/// This agent's Yggdrasil state: `ygg:<name> <state> · N locks · M agents`.
/// Returns None when the current session has no matching agent row, so the
/// token line renders alone rather than erroring.
async fn ygg_segment(pool: &crate::db::DbPool, j: &serde_json::Value) -> Option<String> {
    let name = current_agent_name(j);
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = repo.get_by_name(&name).await.ok()??;

    let locks = LockManager::new(pool, 0, crate::db::user_id())
        .list_agent_locks(&agent.agent_id)
        .await
        .map(|v| v.len())
        .unwrap_or(0);

    let cutoff = Utc::now() - Duration::minutes(ACTIVE_WINDOW_MINS);
    let others = repo
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| {
            a.agent_id != agent.agent_id
                && a.current_state != AgentState::Shutdown
                && a.updated_at >= cutoff
        })
        .count();

    Some(format!(
        "{DIM}ygg:{RESET}{BOLD}{name}{RESET} {DIM}{}{RESET} {DIM}· {locks} locks · {others} agents{RESET}",
        agent.current_state
    ))
}

/// Resolve this session's agent name the same way the hooks do
/// (`$YGG_AGENT_NAME`, else the working-directory basename). For the
/// statusLine we prefer the cwd reported in the JSON payload, falling back to
/// the process cwd.
fn current_agent_name(j: &serde_json::Value) -> String {
    if let Ok(name) = std::env::var("YGG_AGENT_NAME")
        && !name.is_empty()
    {
        return name;
    }
    let cwd = j
        .pointer("/workspace/current_dir")
        .and_then(|v| v.as_str())
        .or_else(|| j.get("cwd").and_then(|v| v.as_str()))
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    cwd.as_deref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "ygg".to_string())
}

#[derive(Default)]
struct Totals {
    /// Σ input_tokens across assistant turns.
    ti: i64,
    /// Σ output_tokens across assistant turns.
    to: i64,
    /// Σ (cache_creation + cache_read) across assistant turns.
    tc: i64,
    /// Last turn's window occupancy: input + cache_creation + cache_read.
    ctx: i64,
}

/// Stream the transcript JSONL once, summing per-turn usage for the totals and
/// keeping the last turn's occupancy for ctx. Mirrors the liked shell
/// statusline's `jq` reducer. Reads line-by-line to bound memory on large
/// transcripts.
fn transcript_totals(path: &std::path::Path) -> Totals {
    let mut t = Totals::default();
    let Ok(file) = std::fs::File::open(path) else {
        return t;
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("assistant") {
            continue;
        }
        let Some(u) = v.pointer("/message/usage").or_else(|| v.pointer("/usage")) else {
            continue;
        };
        let field = |k: &str| u.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        let inp = field("input_tokens");
        let out = field("output_tokens");
        let cache = field("cache_creation_input_tokens") + field("cache_read_input_tokens");
        t.ti += inp;
        t.to += out;
        t.tc += cache;
        // Last assistant turn wins for the live context occupancy.
        t.ctx = inp + cache;
    }
    t
}

/// Compact token formatting: `340` → `340`, `45230` → `45.2k`, `1_500_000` →
/// `1.5M` (one decimal). Cumulative totals — especially summed cache reads —
/// routinely cross 1M over a long session, so step up to `M` rather than
/// printing an unreadable `1500.0k`.
fn fmt_k(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{}.{}M", n / 1_000_000, (n % 1_000_000) / 100_000)
    } else if n >= 1000 {
        format!("{}.{}k", n / 1000, (n % 1000) / 100)
    } else {
        format!("{n}")
    }
}
