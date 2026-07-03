use std::path::PathBuf;

use crate::config::AppConfig;
use crate::models::agent::AgentRepo;
use crate::tmux::TmuxManager;

/// Spawn a new Claude Code agent in a tmux window.
///
/// Each agent gets its own git worktree so concurrent agents never collide
/// on the shared working directory.
pub async fn execute(
    pool: &crate::db::DbPool,
    _config: &AppConfig,
    task: &str,
    name: Option<&str>,
) -> Result<(), anyhow::Error> {
    let agent_name = match name {
        Some(n) => n.to_string(),
        None => slugify(task),
    };

    if agent_name.is_empty() {
        anyhow::bail!(
            "agent name cannot be empty — provide --name or a task with alphanumeric characters"
        );
    }

    println!("Spawning agent '{agent_name}' for task: {task}");

    if !TmuxManager::is_available().await {
        anyhow::bail!("tmux is not available. Install tmux first.");
    }

    // Register agent in DB
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = agent_repo.register(&agent_name).await?;
    println!("  Agent ID: {}", agent.agent_id);

    // Create a git worktree so this agent has an isolated working copy.
    let worktree_path = create_worktree(&agent_name).await?;
    println!("  Worktree: {}", worktree_path.display());

    // Ensure tmux session + create task window. The helper returns pane
    // *ids* (tmux's stable @N references) rather than positional .0/.1
    // — the latter break under `pane-base-index 1` in .tmux.conf.
    TmuxManager::ensure_session().await?;
    let (left_pane, right_pane) = TmuxManager::create_task_window(&agent_name).await?;

    // cd into the worktree, then start Claude Code with the task as prompt.
    // YGG_SPAWN_PERMISSION_MODE overrides the default (bypassPermissions).
    // Accepted values: bypassPermissions, dontAsk, acceptEdits, default, plan.
    let cd_cmd = format!("cd '{}'", shell_escape(&worktree_path.to_string_lossy()));
    TmuxManager::send_keys(&left_pane, &cd_cmd).await?;
    let perm_mode =
        std::env::var("YGG_SPAWN_PERMISSION_MODE").unwrap_or_else(|_| "bypassPermissions".into());
    const ALLOWED_PERM_MODES: &[&str] = &[
        "bypassPermissions",
        "dontAsk",
        "acceptEdits",
        "default",
        "plan",
    ];
    if !ALLOWED_PERM_MODES.contains(&perm_mode.as_str()) {
        anyhow::bail!(
            "invalid YGG_SPAWN_PERMISSION_MODE {:?} — allowed: {}",
            perm_mode,
            ALLOWED_PERM_MODES.join(", ")
        );
    }
    let claude_cmd = build_claude_cmd(&perm_mode, &agent_name, task);
    TmuxManager::send_keys(&left_pane, &claude_cmd).await?;

    // Start the ygg observer in the sidebar (right) pane. It tails the
    // Claude Code JSONL transcript and ingests turns into the DAG.
    let ygg_bin = std::env::current_exe()
        .unwrap_or_else(|_| "ygg".into())
        .to_string_lossy()
        .to_string();
    let observe_cmd = format!(
        "'{}' observe --agent '{}'",
        shell_escape(&ygg_bin),
        shell_escape(&agent_name),
    );
    TmuxManager::send_keys(&right_pane, &observe_cmd).await?;

    println!("  Agent '{agent_name}' spawned in tmux");
    println!("  ├─ pane 0: Claude Code (agent)");
    println!("  ├─ pane 1: ygg observe (DAG ingest)");
    println!("  └─ worktree: {}", worktree_path.display());
    println!();
    println!("  tmux attach -t ygg");

    Ok(())
}

/// Create a git worktree for the agent under `.ygg/worktrees/<name>`.
/// Returns the absolute path to the new worktree.
async fn create_worktree(agent_name: &str) -> Result<PathBuf, anyhow::Error> {
    let branch_name = format!("ygg/{agent_name}");
    let worktree_dir = PathBuf::from(".ygg/worktrees").join(agent_name);

    if worktree_dir.exists() {
        // Reuse existing worktree (agent respawn / retry).
        return Ok(std::fs::canonicalize(&worktree_dir)?);
    }

    std::fs::create_dir_all(".ygg/worktrees")?;

    let output = tokio::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch_name,
            &worktree_dir.to_string_lossy(),
            "HEAD",
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {stderr}");
    }

    Ok(std::fs::canonicalize(&worktree_dir)?)
}

/// Build the shell line that launches Claude Code in the agent's pane.
///
/// Two hardening measures for long-running tool calls (e.g. `cargo test`
/// running past Claude Code's default 2-minute Bash timeout):
///
///   - `BASH_DEFAULT_TIMEOUT_MS` / `BASH_MAX_TIMEOUT_MS` raise the Bash
///     tool timeout (30 min default, 2 h ceiling) so slow commands don't
///     get SIGTERM'd out from under the agent.
///   - `setsid` puts `claude` in its own session so a SIGTERM aimed at a
///     timed-out child's process group can't propagate up and kill the
///     agent. macOS ships `setsid` only as a libc call (no CLI), so we
///     resolve it at runtime via `command -v` and fall back to a bare
///     invocation when it's absent (`${SETSID:+...}` expands to nothing).
///     `-w` keeps `claude` in the foreground so the pane stays attached
///     and the watcher's exit detection still works; `-c` hands it the
///     pane's controlling terminal for the interactive TUI.
///
/// `YGG_AGENT_NAME` is exported so the lifecycle hooks resolve this agent by
/// its registered name. Without it the hooks fall back to the cwd basename —
/// the worktree directory — which re-mints stale identities unrelated to the
/// current task. `YGG_SPAWNED` marks this as a spawned worker for the
/// Stop-check + learnings nudge. The env prefix applies to claude and every
/// hook subprocess it launches.
fn build_claude_cmd(perm_mode: &str, agent_name: &str, task: &str) -> String {
    format!(
        "SETSID=\"$(command -v setsid)\"; \
         YGG_AGENT_NAME='{name}' YGG_SPAWNED=1 \
         BASH_DEFAULT_TIMEOUT_MS=1800000 BASH_MAX_TIMEOUT_MS=7200000 \
         ${{SETSID:+\"$SETSID\" -w -c}} \
         claude --dangerously-skip-permissions --permission-mode '{pm}' --name '{name}' '{tk}'",
        name = shell_escape(agent_name),
        pm = shell_escape(perm_mode),
        tk = shell_escape(task),
    )
}

fn slugify(task: &str) -> String {
    task.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

fn shell_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_cmd_sets_bash_timeout_env() {
        let cmd = build_claude_cmd("bypassPermissions", "agent-x", "do the thing");
        // Both timeout knobs are passed as an env-assignment prefix so
        // claude (and the Bash tool it spawns) inherit them.
        assert!(cmd.contains("BASH_DEFAULT_TIMEOUT_MS=1800000"));
        assert!(cmd.contains("BASH_MAX_TIMEOUT_MS=7200000"));
    }

    #[test]
    fn claude_cmd_wraps_in_setsid_when_available() {
        let cmd = build_claude_cmd("bypassPermissions", "agent-x", "do the thing");
        // Resolves setsid at runtime and applies it only when present,
        // so the line still runs on macOS (no setsid CLI).
        assert!(cmd.contains("command -v setsid"));
        assert!(cmd.contains("${SETSID:+\"$SETSID\" -w -c}"));
    }

    #[test]
    fn claude_cmd_passes_perm_mode_and_task() {
        let cmd = build_claude_cmd("acceptEdits", "my-agent", "fix the bug");
        assert!(cmd.contains("--permission-mode 'acceptEdits'"));
        assert!(cmd.contains("--name 'my-agent'"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(cmd.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn claude_cmd_escapes_single_quotes_in_task() {
        let cmd = build_claude_cmd("bypassPermissions", "agent-x", "it's broken");
        // Single quote is escaped so the surrounding '...' stays balanced.
        assert!(cmd.contains("'it'\\''s broken'"));
    }

    #[test]
    fn claude_cmd_quotes_perm_mode() {
        // perm_mode must be single-quoted even though validation restricts it to a
        // safe fixed set — defence-in-depth against future changes.
        let cmd = build_claude_cmd("dontAsk", "agent-x", "do the thing");
        assert!(cmd.contains("--permission-mode 'dontAsk'"));
    }

    #[test]
    fn claude_cmd_exports_agent_identity_env() {
        // Hooks resolve agent identity from YGG_AGENT_NAME; YGG_SPAWNED marks
        // the worker. Both must be in the env prefix applied to claude.
        let cmd = build_claude_cmd("bypassPermissions", "agent-x", "do the thing");
        assert!(cmd.contains("YGG_AGENT_NAME='agent-x'"));
        assert!(cmd.contains("YGG_SPAWNED=1"));
    }

    #[test]
    fn claude_cmd_escapes_single_quotes_in_agent_name() {
        // agent_name containing a single quote must not break the shell command.
        let cmd = build_claude_cmd("bypassPermissions", "agent's-name", "do the thing");
        assert!(cmd.contains("--name 'agent'\\''s-name'"));
    }
}
