//! `ygg run` — task-run lifecycle. Opens, finalizes, and inspects task_runs
//! rows. The scheduler uses these helpers internally; the CLI exposes them so
//! humans driving claim/close manually keep the same data shape (yggdrasil-96).

use crate::models::event::{EventKind, EventRepo};
use crate::models::repo::RepoRepo;
use crate::models::task_run::{
    RunReason, RunState, TaskRun, TaskRunCreate, TaskRunRepo, check_inline_size,
};

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const GRAY: &str = "\x1b[38;5;245m";

/// Open a run for a task. Inserts a new task_runs row at attempt = (latest+1)
/// or 1, with parent_run_id set when a previous attempt exists. Sets
/// tasks.current_attempt_id and starts the run as `running` (manual claim
/// semantics; the scheduler uses a slightly different state arc via `ready`).
///
/// This is the primary CLI helper backing `ygg task claim` and the spawned
/// agent's SessionStart hook (when YGG_RUN_ID is unset).
pub async fn open_for_task(
    pool: &crate::db::DbPool,
    task_id: String,
    agent_id: Option<String>,
    input: serde_json::Value,
) -> Result<TaskRun, anyhow::Error> {
    let runs = TaskRunRepo::new(pool);
    let prior = runs.latest(&task_id).await?;
    let attempt = prior.as_ref().map(|p| p.attempt + 1).unwrap_or(1);

    check_inline_size(&input, "input").map_err(|e| anyhow::anyhow!("oversize run input: {e}"))?;

    let mut run = runs
        .create(TaskRunCreate {
            task_id: task_id.clone(),
            attempt,
            parent_run_id: prior.as_ref().map(|p| p.run_id.clone()),
            input,
            ..Default::default()
        })
        .await?;

    // Manual claim path goes scheduled → running directly. The scheduler's
    // dispatch path uses scheduled → ready → running. Both end up the same
    // place for a heartbeating agent.
    runs.set_state(&run.run_id, RunState::Running, RunReason::Ok)
        .await?;
    run.state = RunState::Running;

    // Bind to agent + tasks.current_attempt_id in one statement so a half-
    // applied claim is impossible.
    let mut tx = pool.begin().await?;
    sqlx::query(
        r#"UPDATE task_runs
           SET agent_id = $2,
               started_at = COALESCE(started_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
               heartbeat_at = COALESCE(heartbeat_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
               claimed_at = COALESCE(claimed_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
               updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
           WHERE run_id = $1"#,
    )
    .bind(&run.run_id)
    .bind(&agent_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE tasks SET current_attempt_id = $2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE task_id = $1")
        .bind(&task_id)
        .bind(&run.run_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    run.agent_id = agent_id;
    Ok(run)
}

/// Finalize a task's current run with the given state. Idempotent: if the
/// run is already terminal, returns the existing terminal state without
/// overwriting it.
///
/// Used by `ygg task close` to keep the run history honest, and by the Stop
/// hook (yggdrasil-97) once the agent's outcome data is on disk. A `state` of
/// `Cancelled` corresponds to user interruption; `Failed` corresponds to a
/// reason matching the close-reason "fail*" heuristic; `Succeeded` is the
/// happy path.
pub async fn finalize_for_task(
    pool: &crate::db::DbPool,
    task_id: String,
    state: RunState,
    reason: RunReason,
    agent_name: &str,
    agent_id: Option<String>,
) -> Result<Option<TaskRun>, anyhow::Error> {
    if !state.is_terminal() {
        return Err(anyhow::anyhow!(
            "finalize_for_task requires a terminal state; got {state}"
        ));
    }
    let runs = TaskRunRepo::new(pool);
    let Some(latest) = runs.latest(&task_id).await? else {
        return Ok(None);
    };
    if latest.state.is_terminal() {
        return Ok(Some(latest));
    }
    runs.set_state(&latest.run_id, state, reason).await?;

    // Clear current_attempt_id; preserved on the run itself.
    sqlx::query(
        "UPDATE tasks SET current_attempt_id = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE task_id = $1",
    )
    .bind(&task_id)
    .execute(pool)
    .await?;

    // Emit a run_terminal event so logs / dashboard / scheduler can react.
    let repo_id = sqlx::query_scalar::<_, String>("SELECT repo_id FROM tasks WHERE task_id = $1")
        .bind(&task_id)
        .fetch_one(pool)
        .await?;
    let task_ref = sqlx::query_as::<_, (String, i32)>(
        r#"SELECT r.task_prefix, t.seq FROM tasks t
           JOIN repos r USING (repo_id) WHERE t.task_id = $1"#,
    )
    .bind(&task_id)
    .fetch_one(pool)
    .await
    .ok()
    .map(|(prefix, seq)| format!("{prefix}-{seq}"))
    .unwrap_or_else(|| task_id.to_string());

    let _ = EventRepo::new(pool)
        .emit(
            EventKind::RunTerminal,
            agent_name,
            agent_id,
            serde_json::json!({
                "task_ref": task_ref,
                "task_id": task_id,
                "repo_id": repo_id,
                "run_id": latest.run_id,
                "attempt": latest.attempt,
                "state": state.as_str(),
                "reason": reason.as_str(),
            }),
        )
        .await;

    Ok(Some(latest))
}

/// `ygg run claim <task_ref>` — open a run for a task by reference.
/// Mostly used by spawned agents whose SessionStart hook calls into here.
pub async fn claim_cli(
    pool: &crate::db::DbPool,
    reference: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let task = super::task_cmd::resolve_task_public(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let run = open_for_task(pool, task.task_id, agent_id.clone(), serde_json::json!({})).await?;

    let repo = RepoRepo::new(pool).get(&task.repo_id).await?;
    let task_ref = repo
        .as_ref()
        .map(|r| format!("{}-{}", r.task_prefix, task.seq))
        .unwrap_or_else(|| reference.to_string());
    let _ = EventRepo::new(pool)
        .emit(
            EventKind::RunClaimed,
            agent_name,
            agent_id,
            serde_json::json!({
                "task_ref": task_ref,
                "run_id": run.run_id,
                "attempt": run.attempt,
                "agent": agent_name,
            }),
        )
        .await;

    println!("{task_ref}: opened run #{} ({})", run.attempt, run.run_id);
    Ok(())
}

/// `ygg run heartbeat [--run-id <uuid>]` — bump heartbeat_at on a run.
/// Without --run-id, heartbeats whichever run is currently bound to the
/// agent's latest in-progress task.
pub async fn heartbeat_cli(
    pool: &crate::db::DbPool,
    run_id: Option<String>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let runs = TaskRunRepo::new(pool);
    let id = match run_id {
        Some(id) => id,
        None => {
            // yggdrasil-110: refuse to heartbeat a NULL agent_id. `IS NOT
            // DISTINCT FROM NULL` matches every NULL row, so an unresolvable
            // agent_name would heartbeat some *other* agent's run.
            let agent_id = resolve_agent_id(pool, agent_name).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "agent {agent_name} not found; pass --run-id <uuid> to heartbeat explicitly"
                )
            })?;
            sqlx::query_scalar::<_, String>(
                r#"SELECT run_id FROM task_runs
                   WHERE state = 'running' AND agent_id = $1
                   ORDER BY started_at DESC LIMIT 1"#,
            )
            .bind(agent_id)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no running run for agent {agent_name}"))?
        }
    };
    runs.heartbeat(&id).await?;
    Ok(())
}

/// `ygg run finalize <task-ref> --state <state> [--reason <reason>]` — manually
/// finalize a run. Used by the Stop hook with agent_name resolved from env.
pub async fn finalize_cli(
    pool: &crate::db::DbPool,
    reference: &str,
    state: &str,
    reason: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    use std::str::FromStr;
    let state = RunState::from_str(state).map_err(|e| anyhow::anyhow!(e))?;
    let reason = RunReason::from_str(reason).map_err(|e| anyhow::anyhow!(e))?;
    let task = super::task_cmd::resolve_task_public(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let res = finalize_for_task(pool, task.task_id, state, reason, agent_name, agent_id).await?;
    match res {
        Some(_) => println!("{reference}: run finalized → {state} ({reason})"),
        None => println!("{reference}: no run to finalize"),
    }
    Ok(())
}

/// `ygg run show <run-id>` — print one run's detail.
pub async fn show_cli(pool: &crate::db::DbPool, run_id: String) -> Result<(), anyhow::Error> {
    let run = TaskRunRepo::new(pool)
        .get(&run_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no run {run_id}"))?;
    print_run(&run);
    Ok(())
}

/// `ygg run list <task-ref>` — print run history for a task.
pub async fn list_cli(pool: &crate::db::DbPool, reference: &str) -> Result<(), anyhow::Error> {
    let task = super::task_cmd::resolve_task_public(pool, reference).await?;
    let runs = TaskRunRepo::new(pool).list_by_task(&task.task_id).await?;
    if runs.is_empty() {
        println!("{reference}: no runs yet");
        return Ok(());
    }
    println!(
        "{DIM}#{RESET}  {DIM}state{RESET}     {DIM}reason{RESET}              {DIM}started{RESET}              {DIM}duration{RESET}  {DIM}commit{RESET}"
    );
    for run in &runs {
        print_run_row(run);
    }
    Ok(())
}

fn print_run(run: &TaskRun) {
    println!("run_id:           {}", run.run_id);
    println!("task_id:          {}", run.task_id);
    println!("attempt:          {}", run.attempt);
    if let Some(p) = &run.parent_run_id {
        println!("parent_run_id:    {p}");
    }
    println!("state:            {} ({})", run.state, run.reason);
    println!("idempotency_key:  {}", run.idempotency_key);
    println!("scheduled_at:     {}", run.scheduled_at);
    if let Some(t) = run.claimed_at {
        println!("claimed_at:       {t}");
    }
    if let Some(t) = run.started_at {
        println!("started_at:       {t}");
    }
    if let Some(t) = run.heartbeat_at {
        println!("heartbeat_at:     {t}");
    }
    if let Some(t) = run.ended_at {
        println!("ended_at:         {t}");
    }
    if let Some(d) = run.deadline_at {
        println!("deadline_at:      {d}");
    }
    if let Some(a) = &run.agent_id {
        println!("agent_id:         {a}");
    }
    if let Some(w) = &run.worker_id {
        println!("worker_id:        {w}");
    }
    if let Some(s) = &run.session_id {
        println!("session_id:       {s}");
    }
    println!("max_attempts:     {}", run.max_attempts);
    if let Some(sha) = &run.output_commit_sha {
        println!("commit:           {sha}");
    }
    if let Some(b) = &run.output_branch {
        println!("branch:           {b}");
    }
    if let Some(p) = &run.output_pr_url {
        println!("pr_url:           {p}");
    }
    if let Some(b) = &run.output_blob_ref {
        println!("output_blob:      {b}");
    }
    if let Some(f) = &run.fingerprint {
        println!("fingerprint:      {f}");
    }
}

fn print_run_row(run: &TaskRun) {
    let color = match run.state {
        RunState::Succeeded => GREEN,
        RunState::Failed | RunState::Crashed | RunState::Poison => RED,
        RunState::Cancelled => DIM,
        RunState::Running => BLUE,
        RunState::Retrying => YELLOW,
        _ => GRAY,
    };
    let started = run
        .started_at
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "—".to_string());
    let duration = match (run.started_at, run.ended_at) {
        (Some(s), Some(e)) => format!("{}s", (e - s).num_seconds().max(0)),
        (Some(s), None) => format!("{}s+", (chrono::Utc::now() - s).num_seconds().max(0)),
        _ => "—".to_string(),
    };
    let commit = run
        .output_commit_sha
        .as_deref()
        .map(|s| &s[..s.len().min(10)])
        .unwrap_or("—");
    println!(
        "#{:<2} {color}{:<9}{RESET} {:<19} {:<20} {:>8}  {GRAY}{}{RESET}",
        run.attempt,
        run.state.as_str(),
        run.reason.as_str(),
        started,
        duration,
        commit,
    );
}

async fn resolve_agent_id(
    pool: &crate::db::DbPool,
    agent_name: &str,
) -> Result<Option<String>, anyhow::Error> {
    use crate::models::agent::AgentRepo;
    Ok(AgentRepo::new(pool, crate::db::user_id())
        .get_by_name(agent_name)
        .await?
        .map(|a| a.agent_id))
}

/// `ygg run capture-outcome [--agent X]` — Stop-hook handoff (yggdrasil-97).
/// Finds the latest still-running run for the agent and writes outcome fields
/// (ended_at, current branch, latest commit since started_at). Heuristically
/// transitions the run terminal so manual-mode (no scheduler) still produces
/// useful run history; the scheduler treats already-terminal runs as
/// finalized and skips them. Idempotent.
pub async fn capture_outcome_cli(
    pool: &crate::db::DbPool,
    agent_name: &str,
    cwd: Option<std::path::PathBuf>,
) -> Result<(), anyhow::Error> {
    // yggdrasil-110: same NULL-cross-contamination issue as heartbeat. If the
    // agent_name doesn't resolve, silently no-op rather than potentially
    // capturing some other agent's run via `IS NOT DISTINCT FROM NULL`.
    // Stop-hook tolerance: prefer skipping over blocking agent shutdown.
    let Some(agent_id) = resolve_agent_id(pool, agent_name).await? else {
        return Ok(());
    };

    // Find the latest running run for this agent. (Postgres used SKIP
    // LOCKED here; on single-host SQLite the follow-up UPDATEs are
    // idempotent and WAL + busy_timeout handle scheduler concurrency.)
    let run: Option<TaskRun> = sqlx::query_as::<_, TaskRun>(
        r#"SELECT * FROM task_runs
           WHERE state = 'running' AND agent_id = $1
           ORDER BY started_at DESC NULLS LAST, scheduled_at DESC
           LIMIT 1"#,
    )
    .bind(&agent_id)
    .fetch_optional(pool)
    .await?;

    let Some(run) = run else {
        return Ok(());
    };

    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let (commit, branch) = capture_git_state(&cwd);

    // Did the agent produce a commit since started_at? Used as a coarse
    // "succeeded" heuristic when no closer signal is available.
    let produced_commit = match (run.started_at, &commit) {
        (Some(started), Some(sha)) => commit_after(&cwd, sha, started),
        _ => false,
    };

    sqlx::query(
        r#"UPDATE task_runs
           SET ended_at = COALESCE(ended_at, strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
               output_commit_sha = COALESCE(output_commit_sha, $2),
               output_branch = COALESCE(output_branch, $3),
               updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
           WHERE run_id = $1"#,
    )
    .bind(&run.run_id)
    .bind(commit.as_deref())
    .bind(branch.as_deref())
    .execute(pool)
    .await?;

    let runs = TaskRunRepo::new(pool);
    let (state, reason) = if produced_commit {
        (RunState::Succeeded, RunReason::Ok)
    } else {
        (RunState::Failed, RunReason::AgentError)
    };
    runs.set_state(&run.run_id, state, reason).await?;

    // Clear current_attempt_id on the task; preserved on the run row.
    sqlx::query(
        "UPDATE tasks SET current_attempt_id = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE current_attempt_id = $1",
    )
    .bind(&run.run_id)
    .execute(pool)
    .await?;

    let _ = EventRepo::new(pool)
        .emit(
            EventKind::RunTerminal,
            agent_name,
            Some(agent_id),
            serde_json::json!({
                "task_id": run.task_id,
                "run_id": run.run_id,
                "attempt": run.attempt,
                "state": state.as_str(),
                "reason": reason.as_str(),
                "commit_sha": commit,
                "branch": branch,
                "captured_by": "stop_hook",
            }),
        )
        .await;

    Ok(())
}

fn capture_git_state(cwd: &std::path::Path) -> (Option<String>, Option<String>) {
    let commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty());

    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty() && s != "HEAD");

    (commit, branch)
}

/// True if the named commit's author date is at or after `started`. Crude but
/// sufficient: we just need to detect that *some* work happened during the run
/// rather than the agent stopping idle on a clean tree.
fn commit_after(cwd: &std::path::Path, sha: &str, started: chrono::DateTime<chrono::Utc>) -> bool {
    let output = std::process::Command::new("git")
        .args(["show", "-s", "--format=%aI", sha])
        .current_dir(cwd)
        .output();
    let Ok(out) = output else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let date = String::from_utf8(out.stdout).unwrap_or_default();
    let date = date.trim();
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(date) else {
        return false;
    };
    parsed.with_timezone(&chrono::Utc) >= started
}
