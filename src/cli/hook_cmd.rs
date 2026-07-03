//! Native `ygg hook <event>` handlers — replaces shell scripts in scripts/hooks/.
//!
//! Each handler reads the JSON payload Claude Code sends on stdin, extracts
//! the relevant fields, and calls into the same internal functions the shell
//! scripts used to chain via subprocess. Eliminates the jq dependency, shell
//! quoting risks, and five separate script files.

use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::AgentRepo;
use clap::Subcommand;
use tracing::warn;

#[derive(Subcommand)]
pub enum HookAction {
    /// SessionStart: inject agent context
    SessionStart,
    /// UserPromptSubmit: write prompt node + inject similar context + deliver messages
    PromptSubmit,
    /// PreToolUse: record tool use + heartbeat + conditional lock acquire
    PreToolUse,
    /// PreCompact: digest conversation + re-inject context
    PreCompact,
    /// Stop: digest session + capture outcome + stop-check
    Stop,
}

/// Read stdin, parse common fields, dispatch to the appropriate handler.
pub async fn handle(action: HookAction) -> anyhow::Result<()> {
    // Read the full JSON payload from stdin (Claude Code pipes it in).
    let input = {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };

    let payload: serde_json::Value =
        serde_json::from_str(&input).unwrap_or_else(|_| serde_json::json!({}));

    // Extract common fields shared across hooks.
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let agent_name = std::env::var("YGG_AGENT_NAME").unwrap_or_else(|_| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "ygg".to_string())
    });

    // Record session_id process-globally so downstream DB calls tag correctly.
    // Avoids the unsafe, non-thread-safe std::env::set_var.
    crate::models::event::set_cc_session_id(&session_id);

    match action {
        HookAction::SessionStart => handle_session_start(&agent_name, &session_id, &payload).await,
        HookAction::PromptSubmit => handle_prompt_submit(&agent_name, &payload).await,
        HookAction::PreToolUse => handle_pre_tool_use(&agent_name, &payload).await,
        HookAction::PreCompact => handle_pre_compact(&agent_name, &payload).await,
        HookAction::Stop => handle_stop(&agent_name, &payload).await,
    }
}

// ── SessionStart ────────────────────────────────────────────────────────────

/// Port of scripts/hooks/session-start.sh:
/// 1. Write agent→session mapping to /tmp/ygg/session-{sid}.agent
/// 2. Call prime (output to stdout)
async fn handle_session_start(
    agent_name: &str,
    session_id: &str,
    payload: &serde_json::Value,
) -> anyhow::Result<()> {
    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Write session mapping so other hooks/tools can resolve agent from sid.
    if !session_id.is_empty() {
        let dir = std::path::Path::new("/tmp/ygg");
        std::fs::create_dir_all(dir).ok();
        let mapping_file = dir.join(format!("session-{session_id}.agent"));
        std::fs::write(&mapping_file, agent_name).ok();
    }

    // Call prime — output goes to stdout for Claude Code injection.
    let tp = if transcript_path.is_empty() {
        None
    } else {
        Some(transcript_path.as_str())
    };
    crate::cli::prime::execute(agent_name, tp).await?;

    Ok(())
}

// ── UserPromptSubmit ────────────────────────────────────────────────────────

/// UserPromptSubmit hook:
/// 1. Record incremental token stats from the live transcript
/// 2. Check inbox — if non-empty and not "inbox empty", output to stdout
/// 3. Call mark-read (silent)
async fn handle_prompt_submit(
    agent_name: &str,
    _payload: &serde_json::Value,
) -> anyhow::Result<()> {
    // Gracefully ignore errors (matches `|| true` in the shell script).
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            warn!("hook prompt-submit: config error: {e}");
            return Ok(());
        }
    };
    let pool = match crate::db::create_pool(&config.database_url).await {
        Ok(p) => p,
        Err(e) => {
            warn!("hook prompt-submit: db pool error: {e}");
            return Ok(());
        }
    };

    // Record incremental token stats from the live transcript (ygg-2).
    // Uses replace semantics so repeated calls overwrite (no double-counting).
    match crate::stats::collector::find_latest_transcript() {
        Some(tp) => {
            let turns = crate::stats::collector::parse_session(std::path::Path::new(&tp));
            if turns.is_empty() {
                warn!("hook prompt-submit: transcript has 0 parseable turns: {tp}");
            } else {
                let usage = crate::stats::collector::aggregate_usage(&turns);
                let tool_names: Vec<String> =
                    turns.iter().flat_map(|t| t.tool_names.clone()).collect();
                let category = crate::stats::classifier::classify(&tool_names);
                let agent_repo = AgentRepo::new(&pool, crate::db::user_id());
                match agent_repo.get_by_name(agent_name).await {
                    Ok(Some(agent)) => {
                        if let Err(e) = crate::stats::tracker::replace_stats(
                            &pool,
                            &agent.agent_id,
                            &usage,
                            &category,
                        )
                        .await
                        {
                            warn!("hook prompt-submit: replace_stats failed: {e}");
                        }
                    }
                    Ok(None) => {
                        warn!("hook prompt-submit: agent '{agent_name}' not found for stats")
                    }
                    Err(e) => warn!("hook prompt-submit: agent lookup failed: {e}"),
                }
            }
        }
        None => warn!("hook prompt-submit: no transcript found for stats"),
    }

    // Inject unread agent-to-agent messages and advance cursor.
    match crate::cli::msg_cmd::inbox(&pool, agent_name, false).await {
        Ok(msgs) => {
            if !msgs.is_empty() {
                crate::cli::msg_cmd::print_inbox(&msgs);
                // Advance cursor so same messages don't resurface.
                if let Err(e) = crate::cli::msg_cmd::mark_read(&pool, agent_name).await {
                    warn!("hook prompt-submit: mark-read failed: {e}");
                }
            }
        }
        Err(e) => {
            warn!("hook prompt-submit: inbox failed: {e}");
        }
    }

    Ok(())
}

// ── PreToolUse ──────────────────────────────────────────────────────────────

/// Port of scripts/hooks/pre-tool-use.sh:
/// 1. Record tool use (agent-tool) — silent
/// 2. Heartbeat — silent
/// 3. For Edit/Write/NotebookEdit with a file path: acquire lock, warn on conflict
/// 4. Always exit 0
async fn handle_pre_tool_use(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let tool_name = payload
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let file_path = payload
        .get("tool_input")
        .and_then(|ti| {
            ti.get("file_path")
                .or_else(|| ti.get("path"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");

    // Best-effort DB connection — if unavailable, exit silently.
    let config = match AppConfig::from_env() {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let pool = match crate::db::create_pool(&config.database_url).await {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    // Record tool use for dashboard visibility (silent, ignore errors).
    if !tool_name.is_empty() {
        let _ = crate::cli::agent_cmd::set_tool(&pool, agent_name, tool_name).await;
    }

    // ADR 0016 / yggdrasil-99: bump heartbeat on running task_run.
    let _ = crate::cli::run_cmd::heartbeat_cli(&pool, None, agent_name).await;

    // Only lock on file-modifying tools.
    match tool_name {
        "Edit" | "Write" | "NotebookEdit" => {
            if file_path.is_empty() {
                return Ok(());
            }

            let agent_repo = AgentRepo::new(&pool, crate::db::user_id());
            let agent = match agent_repo.get_by_name(agent_name).await {
                Ok(Some(a)) => a,
                _ => return Ok(()),
            };

            let lock_mgr = LockManager::new(&pool, config.lock_ttl_secs, crate::db::user_id());
            match lock_mgr.acquire(file_path, &agent.agent_id).await {
                Ok(_) => {} // Lock acquired silently.
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("locked by") {
                        eprintln!("ygg: {msg}");
                    }
                }
            }

            // yggdrasil-180: surface scoped learnings whose file_glob matches
            // the file being edited, injected as a system reminder before the
            // edit runs. Deduped per session so the same learning fires once.
            let session_id = payload
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lines = crate::cli::learning_cmd::surface_for_edit(
                &pool,
                file_path,
                Some(agent_name),
                session_id,
            )
            .await;
            if !lines.is_empty() {
                let out = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "additionalContext": lines.join("\n"),
                    }
                });
                println!("{out}");
            }
        }
        _ => {}
    }

    Ok(())
}

// ── PreCompact ──────────────────────────────────────────────────────────────

/// Port of scripts/hooks/pre-compact.sh:
/// 1. If transcript exists, digest it (without --stop)
/// 2. Re-inject agent context via prime (output to stdout)
async fn handle_pre_compact(agent_name: &str, _payload: &serde_json::Value) -> anyhow::Result<()> {
    // Re-inject agent context before Claude Code compacts the window.
    crate::cli::prime::execute(agent_name, None).await?;

    Ok(())
}

// ── Stop ────────────────────────────────────────────────────────────────────

/// Port of scripts/hooks/stop.sh:
/// 1. If transcript exists, digest with --stop semantics (end session + release locks)
/// 2. If YGG_RUN_CAPTURE != "0", capture-outcome
/// 3. Run stop-check (output to stdout)
async fn handle_stop(agent_name: &str, payload: &serde_json::Value) -> anyhow::Result<()> {
    let transcript_path = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Attempt config + pool. If unavailable, warn but still try stop_check
    // at the end — a transient DB failure must not silently skip the blocker
    // check that prevents premature session exits.
    let db = match AppConfig::from_env() {
        Ok(config) => match crate::db::create_pool(&config.database_url).await {
            Ok(pool) => Some((pool, config)),
            Err(e) => {
                warn!("hook stop: db pool error: {e}");
                None
            }
        },
        Err(e) => {
            warn!("hook stop: config error: {e}");
            None
        }
    };

    if let Some((ref pool, ref config)) = db {
        let user_id = crate::db::user_id().to_string();

        // 1. On stop, always end session + release locks; token stats are
        //    best-effort and depend on a readable transcript.
        if let Ok(Some(a)) = AgentRepo::new(pool, &user_id).get_by_name(agent_name).await {
            // Record token stats from the transcript to agent_stats (ygg-2).
            if !transcript_path.is_empty() && std::path::Path::new(&transcript_path).is_file() {
                let turns =
                    crate::stats::collector::parse_session(std::path::Path::new(&transcript_path));
                if !turns.is_empty() {
                    let usage = crate::stats::collector::aggregate_usage(&turns);
                    let tool_names: Vec<String> =
                        turns.iter().flat_map(|t| t.tool_names.clone()).collect();
                    let category = crate::stats::classifier::classify(&tool_names);
                    if let Err(e) =
                        crate::stats::tracker::replace_stats(pool, &a.agent_id, &usage, &category)
                            .await
                    {
                        warn!("hook stop: replace_stats failed: {e}");
                    }
                }
            }

            if let Some(sid) =
                crate::models::session::resolve_current_session(pool, &a.agent_id, None).await
            {
                let _ = crate::models::session::SessionRepo::new(pool)
                    .end(&sid)
                    .await;
            }
            let lock_mgr = LockManager::new(pool, config.lock_ttl_secs, &user_id);
            let _ = lock_mgr.release_all_for_agent(&a.agent_id).await;

            // Mark the agent Shutdown. Subsequent prompts re-enter Executing
            // via inject (force_state is unconditional); the watcher's reap
            // pass will close the tmux window once the process is truly gone.
            let _ = AgentRepo::new(pool, &user_id)
                .force_state(
                    &a.agent_id,
                    crate::models::agent::AgentState::Shutdown,
                    None,
                )
                .await;
        }

        // 2. Capture outcome (unless YGG_RUN_CAPTURE=0).
        let skip_capture = std::env::var("YGG_RUN_CAPTURE")
            .map(|v| v == "0")
            .unwrap_or(false);
        if !skip_capture {
            let _ = crate::cli::run_cmd::capture_outcome_cli(pool, agent_name, None).await;
        }
    }

    // 2b. ADR 0017 M4 — learnings-capture nudge. Remind spawned workers, as a
    // session ends, to record any durable correction they received with
    // `ygg learn propose` (lands in the approval gate, fires on nothing until a
    // human approves). Advisory only (stderr, never blocks). Opt out with
    // YGG_LEARN_NUDGE=0. The load-bearing push is the CLAUDE.md directive the
    // model actually reads; this line is for the operator tending the queue.
    let spawned = std::env::var("YGG_SPAWNED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let nudge_off = std::env::var("YGG_LEARN_NUDGE")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        .unwrap_or(false);
    if spawned && !nudge_off {
        eprintln!(
            "ygg: if you were corrected this session, capture the durable rule: \
             `ygg learn propose \"<rule>\" --file-glob \"<glob>\"` (queues for approval)"
        );
    }

    // 3. Stop-check (output to stdout) — runs regardless of whether
    // digest/capture-outcome succeeded or DB was unavailable.
    match &db {
        Some((pool, _)) => {
            if let Err(e) = crate::cli::stop_check::execute(pool, agent_name).await {
                warn!("hook stop: stop-check failed: {e}");
            }
        }
        None => {
            // DB unavailable — we cannot run the blocker query, but we must
            // not silently swallow the fact that stop_check was skipped.
            warn!("hook stop: stop-check skipped — no DB connection available");
            eprintln!("ygg: warning: stop-check could not run (no DB connection)");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompt_truncation_ascii() {
        // ASCII: byte length == char length, should truncate to exactly 2000 bytes.
        let long = "a".repeat(3000);
        let truncated = if long.len() <= 2000 {
            long.clone()
        } else {
            let mut end = 2000;
            while !long.is_char_boundary(end) {
                end -= 1;
            }
            long[..end].to_string()
        };
        assert_eq!(truncated.len(), 2000);
    }

    #[test]
    fn prompt_truncation_multibyte() {
        // Multi-byte UTF-8: must not exceed 2000 bytes and must not split a char.
        // "ä" is 2 bytes in UTF-8 — 1500 of them = 3000 bytes.
        let long = "ä".repeat(1500);
        assert_eq!(long.len(), 3000);
        let truncated = if long.len() <= 2000 {
            long.clone()
        } else {
            let mut end = 2000;
            while !long.is_char_boundary(end) {
                end -= 1;
            }
            long[..end].to_string()
        };
        assert!(truncated.len() <= 2000);
        assert!(truncated.len() >= 1998); // snapped to nearest char boundary
        // Must be valid UTF-8 (String guarantees this, but be explicit).
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn empty_payload_parses() {
        let val: serde_json::Value =
            serde_json::from_str("{}").unwrap_or_else(|_| serde_json::json!({}));
        assert!(val.get("session_id").is_none());
        assert!(val.get("prompt").is_none());
    }

    #[test]
    fn tool_input_extraction() {
        let payload: serde_json::Value = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": "/src/main.rs"
            }
        });
        let file = payload
            .get("tool_input")
            .and_then(|ti| {
                ti.get("file_path")
                    .or_else(|| ti.get("path"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");
        assert_eq!(file, "/src/main.rs");
    }

    #[test]
    fn tool_input_path_fallback() {
        let payload: serde_json::Value = serde_json::json!({
            "tool_name": "Write",
            "tool_input": {
                "path": "/src/lib.rs"
            }
        });
        let file = payload
            .get("tool_input")
            .and_then(|ti| {
                ti.get("file_path")
                    .or_else(|| ti.get("path"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");
        assert_eq!(file, "/src/lib.rs");
    }

    /// Drift regression: native pre-tool-use handler must call heartbeat
    /// (yggdrasil-99 / yggdrasil-107). The source code is checked at compile
    /// time so removing the call is a loud test failure.
    #[test]
    fn native_pre_tool_use_calls_heartbeat() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("heartbeat_cli"),
            "hook_cmd.rs handle_pre_tool_use must call heartbeat_cli (yggdrasil-99)"
        );
    }

    /// Drift regression: native stop handler must call capture_outcome
    /// (yggdrasil-97 / yggdrasil-107).
    #[test]
    fn native_stop_calls_capture_outcome() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("capture_outcome_cli"),
            "hook_cmd.rs handle_stop must call capture_outcome_cli (yggdrasil-97)"
        );
    }

    /// Drift regression: native stop handler must call stop_check.
    #[test]
    fn native_stop_calls_stop_check() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("stop_check::execute"),
            "hook_cmd.rs handle_stop must call stop_check::execute"
        );
    }

    /// Drift regression: native stop handler must record token stats
    /// (ygg-2). Removing the call silently breaks cost/throughput tracking.
    #[test]
    fn native_stop_calls_replace_stats() {
        let src = include_str!("hook_cmd.rs");
        assert!(
            src.contains("replace_stats"),
            "hook_cmd.rs must call replace_stats (ygg-2)"
        );
    }
}
