//! Stop-hook enforcement: emit a `decision: block` JSON to Claude Code's
//! Stop hook when a spawned worker is about to exit with unfinished work
//! (claimed task still in_progress, uncommitted changes, or unpushed
//! commits on the worktree branch). Silent (exit 0, no output) on the
//! primary session and when everything is tied up.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::agent::AgentRepo;
use std::path::Path;
use std::process::Command;

pub async fn execute(pool: &crate::db::DbPool, agent_name: &str) -> Result<(), anyhow::Error> {
    // Hard kill-switch for users who don't want the check.
    if std::env::var("YGG_STOP_CHECK")
        .map(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        .unwrap_or(false)
    {
        return Ok(());
    }

    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().to_string();

    // Spawn-context detection. We only enforce on spawned workers — the
    // primary interactive session must never be blocked from stopping.
    let spawn_env = std::env::var("YGG_SPAWNED")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let worker: Option<(String, String)> = sqlx::query_as(
        r#"SELECT worker_id, task_id
             FROM workers
            WHERE worktree_path = $1 AND ended_at IS NULL
            ORDER BY started_at DESC
            LIMIT 1"#,
    )
    .bind(&cwd_str)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    if !spawn_env && worker.is_none() {
        return Ok(());
    }

    let mut reasons: Vec<String> = Vec::new();

    // Any tasks this agent still holds in_progress in the current repo.
    if let Ok(repo) = resolve_cwd_repo(pool).await
        && let Ok(Some(agent)) = AgentRepo::new(pool, crate::db::user_id())
            .get_by_name(agent_name)
            .await
    {
        let open: Vec<(i32, String)> = sqlx::query_as(
            r#"SELECT seq, title FROM tasks
                    WHERE repo_id = $1 AND assignee = $2 AND status = 'in_progress'
                    ORDER BY seq"#,
        )
        .bind(repo.repo_id)
        .bind(agent.agent_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        for (seq, title) in open {
            reasons.push(format!(
                    "task {p}-{seq} still in_progress ({title}) — close with `ygg task close {p}-{seq} --reason \"...\"`",
                    p = repo.task_prefix,
                ));
        }
    }

    check_git_state(&cwd, &mut reasons);

    if reasons.is_empty() {
        return Ok(());
    }

    let body = format!(
        "Stop refused — this spawned worker has unfinished work:\n\n• {}\n\n\
         Finish, then let the session end naturally. If this is wrong \
         (e.g. human abort), set `YGG_STOP_CHECK=0` and retry.",
        reasons.join("\n• ")
    );
    let payload = serde_json::json!({
        "decision": "block",
        "reason": body,
    });
    println!("{}", payload);
    Ok(())
}

fn check_git_state(cwd: &Path, reasons: &mut Vec<String>) {
    // Is cwd even inside a git tree? If not, skip silently.
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    let is_git = matches!(&inside, Ok(o) if o.status.success()
        && String::from_utf8_lossy(&o.stdout).trim() == "true");
    if !is_git {
        return;
    }

    // Uncommitted changes (tracked + untracked).
    if let Ok(out) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        && out.status.success()
        && !out.stdout.iter().all(|b| b.is_ascii_whitespace())
    {
        reasons
            .push("uncommitted changes in worktree — `git add -A && git commit -m \"...\"`".into());
    }

    // Unpushed commits. Prefer the upstream comparison; fall back to base.
    if let Ok(out) = Command::new("git")
        .args(["rev-list", "--count", "@{u}..HEAD"])
        .current_dir(cwd)
        .output()
        && out.status.success()
    {
        let n: u64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        if n > 0 {
            reasons.push(format!(
                "{n} unpushed commit(s) on current branch — `git push`"
            ));
        }
        return;
    }

    // No upstream set. Compare against likely base branches; if the branch
    // has unique commits, the worker forgot to publish it.
    for base in ["origin/HEAD", "origin/main", "origin/master"] {
        if let Ok(out) = Command::new("git")
            .args(["rev-list", "--count", &format!("{base}..HEAD")])
            .current_dir(cwd)
            .output()
            && out.status.success()
        {
            let n: u64 = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            if n > 0 {
                reasons.push(format!(
                    "{n} commit(s) on unpublished branch — `git push -u origin HEAD`"
                ));
            }
            return;
        }
    }
}
