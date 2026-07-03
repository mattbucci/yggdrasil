//! `ygg plan` — execute tasks with worktrees + CC sessions.
//!
//! Three subcommands that together form the click-to-do surface:
//!
//!   ygg plan create <name>                — shortcut to create an epic
//!   ygg plan add    <epic> <title> [--deps REF,REF] [--kind ...]
//!   ygg plan run    <task-ref> [--dry-run]
//!
//! `plan run` is the single-task synchronous path: ensure worktree →
//! spawn CC session in that worktree → claim task. Multi-task dep-driven
//! orchestration is the supervisor (yggdrasil-44).

use std::process::Command;
use uuid::Uuid;

use crate::models::repo::RepoRepo;
use crate::models::task::{Task, TaskKind, TaskRepo, TaskStatus};
use crate::worktree;

pub async fn create(
    pool: &crate::db::DbPool,
    title: &str,
    description: Option<&str>,
    agent_name: &str,
) -> Result<Task, anyhow::Error> {
    let repo = crate::cli::task_cmd::resolve_cwd_repo(pool).await?;
    let task_repo = TaskRepo::new(pool);
    let agent_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1",
    )
    .bind(agent_name)
    .fetch_optional(pool)
    .await?
    .flatten();

    let created = task_repo
        .create(
            &repo.repo_id,
            agent_id.clone(),
            crate::models::task::TaskCreate {
                title,
                description: description.unwrap_or(""),
                acceptance: None,
                design: None,
                notes: None,
                kind: TaskKind::Epic,
                priority: 1,
                assignee: agent_id,
                labels: &[],
                external_ref: None,
                agent_slug: None,
            },
        )
        .await?;
    println!(
        "Created plan {}-{}  {}",
        repo.task_prefix, created.seq, title
    );
    Ok(created)
}

pub async fn add(
    pool: &crate::db::DbPool,
    epic_ref: &str,
    title: &str,
    description: Option<&str>,
    kind: Option<&str>,
    deps: &[String],
    agent_name: &str,
) -> Result<Task, anyhow::Error> {
    let epic = crate::cli::task_cmd::resolve_task_public(pool, epic_ref).await?;
    let repo = RepoRepo::new(pool)
        .get(&epic.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;
    let agent_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1",
    )
    .bind(agent_name)
    .fetch_optional(pool)
    .await?
    .flatten();
    let task_kind = match kind {
        Some(k) => <TaskKind as std::str::FromStr>::from_str(k).map_err(|e| anyhow::anyhow!(e))?,
        None => TaskKind::Task,
    };

    let task_repo = TaskRepo::new(pool);
    let child = task_repo
        .create(
            &repo.repo_id,
            agent_id,
            crate::models::task::TaskCreate {
                title,
                description: description.unwrap_or(""),
                acceptance: None,
                design: None,
                notes: None,
                kind: task_kind,
                priority: 2,
                assignee: None,
                labels: &[],
                external_ref: None,
                agent_slug: None,
            },
        )
        .await?;

    // Wire the child under the epic and apply declared deps.
    task_repo.add_dep(&epic.task_id, &child.task_id).await?;
    for d in deps {
        let dep = crate::cli::task_cmd::resolve_task_public(pool, d).await?;
        task_repo.add_dep(&child.task_id, &dep.task_id).await?;
    }

    println!(
        "Added {}-{}  under {epic_ref}  {}",
        repo.task_prefix, child.seq, title
    );
    if !deps.is_empty() {
        println!("  deps: {}", deps.join(", "));
    }
    Ok(child)
}

/// Walk from an epic down through the tasks it depends on (its blockers,
/// transitively — nested epics flatten). An epic's "children" in work
/// terms are the sub-tasks blocking its completion, so we follow the
/// blockers_of edge, not children_of.
async fn descendants(pool: &crate::db::DbPool, root: String) -> Result<Vec<String>, anyhow::Error> {
    let edges: Vec<(String, String)> = sqlx::query_as("SELECT task_id, blocker_id FROM task_deps")
        .fetch_all(pool)
        .await?;
    let mut blockers_of: std::collections::HashMap<String, Vec<String>> = Default::default();
    for (t, b) in edges {
        blockers_of.entry(t).or_default().push(b);
    }

    let mut seen = std::collections::HashSet::new();
    let mut frontier = vec![root.clone()];
    let mut out = Vec::new();
    while let Some(id) = frontier.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(blockers) = blockers_of.get(&id) {
            frontier.extend(blockers.iter().cloned());
        }
        if id != root {
            out.push(id);
        }
    }
    Ok(out)
}

/// Supervisor: walks the epic, spawns CC sessions for ready tasks up to
/// the parallelism cap, polls every `poll_secs` for status changes,
/// exits when no open tasks remain in the epic. Each spawn goes through
/// the same `run` path, so every task gets a worktree + priming prompt.
pub async fn supervise(
    pool: &crate::db::DbPool,
    epic_ref: &str,
    agent_name: &str,
    parallelism: usize,
    dry_run: bool,
    poll_secs: u64,
) -> Result<(), anyhow::Error> {
    let epic = crate::cli::task_cmd::resolve_task_public(pool, epic_ref).await?;
    let repo = RepoRepo::new(pool)
        .get(&epic.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;

    let all_descendants = descendants(pool, epic.task_id.clone()).await?;
    if all_descendants.is_empty() {
        println!("Epic {} has no children — nothing to supervise.", epic_ref);
        return Ok(());
    }
    println!(
        "Supervising {epic_ref} ({} child task(s), parallelism={parallelism}) — Ctrl-C to stop",
        all_descendants.len()
    );

    loop {
        // Snapshot all descendant tasks each tick — cheap, avoids stale
        // state if someone closes a task out of band.
        let tasks: Vec<Task> = sqlx::query_as::<_, Task>(
            r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                      kind, status, priority, created_by, assignee, human_flag,
                      created_at, updated_at, closed_at, close_reason, relevance
               FROM tasks WHERE task_id IN (SELECT value FROM json_each($1))"#,
        )
        .bind(serde_json::to_string(&all_descendants).unwrap_or_default())
        .fetch_all(pool)
        .await?;

        let open_count = tasks
            .iter()
            .filter(|t| t.status != TaskStatus::Closed)
            .count();
        let running_count = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::InProgress)
            .count();
        let closed_count = tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Closed && !is_failed(t))
            .count();
        let failed_count = tasks.iter().filter(|t| is_failed(t)).count();

        if open_count == 0 {
            if failed_count > 0 {
                println!(
                    "Epic {epic_ref} finished with {failed_count} failure(s): {} closed / {} total.",
                    closed_count,
                    all_descendants.len()
                );
            } else {
                println!(
                    "All tasks closed ({}/{}) — epic {epic_ref} done.",
                    closed_count,
                    all_descendants.len()
                );
            }
            return Ok(());
        }

        if is_paused(&epic.task_id) {
            println!(
                "[{}] paused — use `ygg plan resume {epic_ref}` to continue",
                timestamp()
            );
            if dry_run {
                return Ok(());
            } // don't infinite-loop in dry-run
            tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
            continue;
        }

        // Find ready tasks: status=Open AND all blockers successfully
        // Closed (a failed blocker does NOT unblock downstream — downstream
        // waits for the human to retry or intervene).
        let id_to_status: std::collections::HashMap<String, (TaskStatus, bool)> = tasks
            .iter()
            .map(|t| (t.task_id.clone(), (t.status.clone(), is_failed(t))))
            .collect();
        let task_repo = TaskRepo::new(pool);
        let capacity = parallelism.saturating_sub(running_count);
        if capacity == 0 {
            println!(
                "[{}] {} running / {} open — at parallelism cap, waiting…",
                timestamp(),
                running_count,
                open_count
            );
        } else {
            let mut spawned = 0usize;
            for t in &tasks {
                if spawned >= capacity {
                    break;
                }
                if t.status != TaskStatus::Open {
                    continue;
                }
                let deps = task_repo.deps(&t.task_id).await?;
                let blockers_clear = deps.iter().all(|d| {
                    match id_to_status.get(&d.task_id) {
                        Some((TaskStatus::Closed, false)) => true,
                        Some((TaskStatus::Closed, true)) => false, // failed blocker
                        Some(_) => false,                          // still open/running/blocked
                        None => true,                              // outside the epic
                    }
                });
                if !blockers_clear {
                    continue;
                }

                let task_ref = format!("{}-{}", repo.task_prefix, t.seq);
                println!(
                    "[{}] spawning {task_ref}  {}",
                    timestamp(),
                    &t.title[..t.title.len().min(60)]
                );
                if !dry_run && let Err(e) = run(pool, &task_ref, agent_name, false).await {
                    eprintln!("  ! spawn failed: {e}");
                }
                spawned += 1;
            }
            if spawned == 0 {
                println!(
                    "[{}] {} open / {} running — no new ready tasks",
                    timestamp(),
                    open_count,
                    running_count
                );
            }
        }

        if dry_run {
            println!("(dry-run: not actually spawning)");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
    }
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// File-based pause flag. No schema change needed, file absent = running.
/// Path: $XDG_STATE_HOME/ygg/plans/<task_id>.paused (or ~/.local/state).
fn pause_flag_path(task_id: &str) -> Result<std::path::PathBuf, anyhow::Error> {
    let base = if let Ok(x) = std::env::var("XDG_STATE_HOME") {
        if !x.is_empty() {
            std::path::PathBuf::from(x)
        } else {
            std::path::PathBuf::from(std::env::var("HOME")?).join(".local/state")
        }
    } else {
        std::path::PathBuf::from(std::env::var("HOME")?).join(".local/state")
    };
    Ok(base.join("ygg/plans").join(format!("{task_id}.paused")))
}

pub async fn pause(pool: &crate::db::DbPool, epic_ref: &str) -> Result<(), anyhow::Error> {
    let epic = crate::cli::task_cmd::resolve_task_public(pool, epic_ref).await?;
    let path = pause_flag_path(&epic.task_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, format!("paused at {}", chrono::Utc::now()))?;
    println!("paused {epic_ref} — supervisor will stop spawning new tasks");
    println!("resume: ygg plan resume {epic_ref}");
    Ok(())
}

pub async fn resume(pool: &crate::db::DbPool, epic_ref: &str) -> Result<(), anyhow::Error> {
    let epic = crate::cli::task_cmd::resolve_task_public(pool, epic_ref).await?;
    let path = pause_flag_path(&epic.task_id)?;
    let _ = std::fs::remove_file(&path);
    println!("resumed {epic_ref}");
    Ok(())
}

fn is_paused(task_id: &str) -> bool {
    pause_flag_path(task_id)
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Abort: tear down in-progress descendants. Removes worktrees (policy:
/// archive, so the branch remains for inspection), sets status back to
/// open, releases locks held by the agent associated with the supervise.
pub async fn abort(
    pool: &crate::db::DbPool,
    epic_ref: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let epic = crate::cli::task_cmd::resolve_task_public(pool, epic_ref).await?;
    let ids = descendants(pool, epic.task_id.clone()).await?;
    let tasks: Vec<Task> = sqlx::query_as::<_, Task>(
        r#"SELECT task_id, repo_id, seq, title, description, acceptance, design, notes,
                  kind, status, priority, created_by, assignee, human_flag,
                  created_at, updated_at, closed_at, close_reason, relevance
           FROM tasks WHERE task_id IN (SELECT value FROM json_each($1)) AND status = 'in_progress'"#,
    )
    .bind(serde_json::to_string(&ids).unwrap_or_default())
    .fetch_all(pool)
    .await?;

    println!(
        "aborting {} in-progress task(s) under {epic_ref}",
        tasks.len()
    );
    for t in &tasks {
        let repo = RepoRepo::new(pool)
            .get(&t.repo_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;
        let r = format!("{}-{}", repo.task_prefix, t.seq);
        println!("  · {r}  dropping worktree (policy=archive) + reverting to open");
        let _ = crate::worktree::teardown(
            pool,
            t.task_id.clone(),
            crate::worktree::TeardownPolicy::Archive,
            true,
        )
        .await;
        let _ = TaskRepo::new(pool)
            .set_status(&t.task_id, TaskStatus::Open, None, None)
            .await;
    }

    // Mark paused so supervisor doesn't immediately re-spawn on next run.
    let _ = std::fs::create_dir_all(pause_flag_path(&epic.task_id)?.parent().unwrap());
    let _ = std::fs::write(
        pause_flag_path(&epic.task_id)?,
        format!("aborted at {}", chrono::Utc::now()),
    );

    println!("releasing locks held by agent '{agent_name}'…");
    let lock_mgr = crate::lock::LockManager::new(pool, 300, crate::db::user_id());
    let locks = lock_mgr.list_all().await.unwrap_or_default();
    let agent_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1",
    )
    .bind(agent_name)
    .fetch_optional(pool)
    .await?
    .flatten();
    if let Some(aid) = agent_id {
        for l in locks.iter().filter(|l| l.agent_id == aid) {
            let _ = lock_mgr.release(&l.resource_key, &aid).await;
        }
    }
    println!("done. To resume after investigating: ygg plan resume {epic_ref}");
    Ok(())
}

/// Did a task fail? close_reason contains "fail" is the cheap heuristic.
fn is_failed(task: &Task) -> bool {
    task.status == TaskStatus::Closed
        && task
            .close_reason
            .as_deref()
            .map(|r| r.to_lowercase().contains("fail"))
            .unwrap_or(false)
}

/// Run the single-task path. When called from the TUI, every println!
/// would bleed into the ratatui alternate-screen frame and corrupt the
/// display. Route status through the `reporter` closure instead — the
/// CLI entry passes a println-backed one; the TUI passes a capture-
/// into-flash one. Returns (ok, last-line-of-status-for-flash).
pub async fn run(
    pool: &crate::db::DbPool,
    task_ref: &str,
    agent_name: &str,
    dry_run: bool,
) -> Result<(), anyhow::Error> {
    run_with_reporter(pool, task_ref, agent_name, dry_run, &|line: &str| {
        println!("{line}");
    })
    .await
    .map(|_| ())
}

/// Core logic — all status goes through the reporter closure. Returns
/// the final headline string (for the TUI flash).
pub async fn run_with_reporter(
    pool: &crate::db::DbPool,
    task_ref: &str,
    agent_name: &str,
    dry_run: bool,
    reporter: &dyn Fn(&str),
) -> Result<String, anyhow::Error> {
    let task = crate::cli::task_cmd::resolve_task_public(pool, task_ref).await?;
    let repo = RepoRepo::new(pool)
        .get(&task.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;

    // Refuse to run a task whose blockers aren't all closed — that's the
    // supervisor's job; single-shot run should be a leaf operation.
    let deps = TaskRepo::new(pool).deps(&task.task_id).await?;
    let unresolved: Vec<_> = deps
        .iter()
        .filter(|d| d.status != TaskStatus::Closed)
        .collect();
    if !unresolved.is_empty() {
        reporter(&format!(
            "{}-{}  has {} open blocker(s) — refusing to run.",
            repo.task_prefix,
            task.seq,
            unresolved.len()
        ));
        for d in &unresolved {
            reporter(&format!(
                "  · {}-{}  [{}]  {}",
                repo.task_prefix, d.seq, d.status, d.title
            ));
        }
        reporter("Close the blockers first, or use `ygg plan supervise`.");
        return Ok(format!("blocked by {} open dep(s)", unresolved.len()));
    }

    if dry_run {
        reporter("DRY RUN:");
        reporter(&format!(
            "  1. ensure worktree for {}-{}",
            repo.task_prefix, task.seq
        ));
        reporter(&format!(
            "  2. claim task (assignee = {agent_name}, status → in_progress)"
        ));
        reporter("  3. tmux new-session in the worktree");
        reporter("  4. launch `claude` with a priming prompt");
        return Ok("dry-run printed".to_string());
    }

    // 1. Worktree
    let wt = worktree::ensure(pool, task.task_id.clone()).await?;
    reporter(&format!("worktree: {}", wt.path.display()));

    // 2. Claim the task
    let agent_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT agent_id FROM agents WHERE agent_name = $1",
    )
    .bind(agent_name)
    .fetch_optional(pool)
    .await?
    .flatten();
    if let Some(aid) = agent_id {
        let _ = TaskRepo::new(pool)
            .set_status(&task.task_id, TaskStatus::InProgress, Some(aid), None)
            .await;
    }

    // 2b. Pre-trust the worktree path in ~/.claude.json so Claude Code
    // doesn't show its workspace trust dialog. `--dangerously-skip-
    // permissions` bypasses tool prompts but NOT the trust dialog — the
    // trust state is persisted per-path in ~/.claude.json.projects.
    if let Err(e) = pre_trust_claude_path(&wt.path) {
        reporter(&format!("warn: couldn't pre-trust worktree: {e}"));
    }

    // 3. tmux session + 4. launch claude. Session name encodes the agent +
    // persona + task ref + short uniq suffix so `tmux ls` reads like a
    // flat worker list and a second spawn of the same task doesn't collide:
    //   ygg-reviewer·yggdrasil-43·a3c1
    //   ygg-route53·kb-chunking-7·9f0e
    let persona = std::env::var("YGG_AGENT_PERSONA")
        .ok()
        .filter(|s| !s.is_empty());
    let agent_label = match persona.as_deref() {
        Some(p) => format!("{agent_name}:{p}"),
        None => agent_name.to_string(),
    };
    let uniq: String = Uuid::new_v4().to_string().chars().take(4).collect();
    let session = sanitize_tmux_name(&format!(
        "ygg-{agent_label}·{}-{}·{uniq}",
        repo.task_prefix, task.seq
    ));
    spawn_tmux(&session, &wt.path, &task, &repo, agent_name)?;

    // 5. Register the worker row so observer + reconciliation + Workers
    // panel have something to track. Session association is best-effort:
    // there's no cc_session_id yet (claude hasn't launched), so the
    // hook-driven session resolution will fill session_id later when
    // claude fires its SessionStart.
    use crate::models::worker::WorkerRepo;
    match WorkerRepo::new(pool)
        .spawn(
            task.task_id,
            None,
            &session,
            "work",
            &wt.path.to_string_lossy(),
        )
        .await
    {
        Ok(w) => reporter(&format!("worker registered: {}", w.worker_id)),
        Err(e) => reporter(&format!("warn: couldn't register worker row: {e}")),
    }

    reporter(&format!(
        "launched {} in tmux session '{session}'",
        task_ref_display(&repo.task_prefix, task.seq)
    ));
    reporter(&format!("  attach: tmux attach -t {session}"));
    Ok(format!(
        "launched {} (session: {session})",
        task_ref_display(&repo.task_prefix, task.seq)
    ))
}

/// Pre-populate ~/.claude.json with `projects[<path>].hasTrustDialogAccepted
/// = true` so a freshly-created worktree doesn't prompt. Reads the whole
/// file, mutates the one key, writes back. Fast enough (file is ~200KB);
/// atomic via rename.
fn pre_trust_claude_path(path: &std::path::Path) -> Result<(), anyhow::Error> {
    use std::io::Write;
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let claude_json = std::path::PathBuf::from(&home).join(".claude.json");
    if !claude_json.exists() {
        // Fresh install — Claude will create it on first run. Don't pre-
        // create because we'd have to invent every other key too.
        return Ok(());
    }
    let raw = std::fs::read_to_string(&claude_json)?;
    let mut root: serde_json::Value = serde_json::from_str(&raw)?;
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path_key = abs.to_string_lossy().to_string();

    let projects = root
        .get_mut("projects")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("~/.claude.json has no 'projects' object"))?;

    let entry = projects.entry(path_key.clone()).or_insert_with(|| {
        serde_json::json!({
            "allowedTools": [],
            "mcpContextUris": [],
            "mcpServers": {},
            "enabledMcpjsonServers": [],
            "disabledMcpjsonServers": [],
            "hasTrustDialogAccepted": true,
            "projectOnboardingSeenCount": 1,
            "hasClaudeMdExternalIncludesApproved": false,
            "hasClaudeMdExternalIncludesWarningShown": false,
        })
    });
    if let Some(obj) = entry.as_object_mut() {
        obj.insert(
            "hasTrustDialogAccepted".into(),
            serde_json::Value::Bool(true),
        );
    }

    // Write via tempfile + rename for atomicity. Ownership/perms are
    // preserved by rename on the same filesystem.
    let tmp = claude_json.with_extension("json.ygg-tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string(&root)?.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &claude_json)?;
    Ok(())
}

/// Single-quote a shell argument. Agent names are slugs, but guard against
/// the occasional `:` (persona) or odd char slipping through.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// tmux window names can't contain colons (they split target specs) or
/// whitespace. We keep "·" as a visual separator but replace colons from
/// the persona with "-".
fn sanitize_tmux_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' => '-',
            ' ' | '\t' | '\n' => '_',
            _ => c,
        })
        .collect()
}

fn task_ref_display(prefix: &str, seq: i32) -> String {
    format!("{prefix}-{seq}")
}

fn spawn_tmux(
    session: &str,
    cwd: &std::path::Path,
    task: &Task,
    repo: &crate::models::repo::Repo,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    // Each worker gets its own tmux SESSION (not a window under `yggdrasil`).
    // Rationale: a session-per-worker matches the "each spawn is an
    // independent agent" mental model — user detaches from the TUI and
    // attaches to a specific worker cleanly (tmux switch-client -t <sess>),
    // ctrl-b d detaches that worker alone, and `tmux ls` reads like a
    // flat worker list. The dashboard's own `yggdrasil` session stays
    // separate; this worker has no window in it.
    const WINDOW: &str = "work";
    let status = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            session,
            "-n",
            WINDOW,
            "-c",
            &cwd.to_string_lossy(),
        ])
        .status()
        .map_err(|e| anyhow::anyhow!("tmux new-session: {e}"))?;
    if !status.success() {
        anyhow::bail!("tmux new-session failed for '{session}'");
    }

    // Write the priming prompt to a temp file, then pipe it via stdin.
    // Heredocs through `tmux send-keys` were fragile — the \n handling
    // varies between shells and terminal emulators, and the workspace
    // trust dialog would still appear because the flag wasn't making it
    // through cleanly. File + redirect is unambiguous.
    let prime = prime_prompt(task, repo);
    let prime_path =
        std::env::temp_dir().join(format!("ygg-prime-{}-{}.txt", repo.task_prefix, task.seq));
    std::fs::write(&prime_path, &prime).map_err(|e| anyhow::anyhow!("write prime file: {e}"))?;

    let target = format!("{session}:{WINDOW}");
    let flags = std::env::var("YGG_CLAUDE_FLAGS")
        .unwrap_or_else(|_| "--dangerously-skip-permissions".to_string());
    // Cleanup the temp file once claude has consumed it. `; rm -f` keeps
    // the session alive while ensuring the file gets removed.
    // Tag the spawn so the Stop-hook enforcer (`ygg stop-check`) knows
    // this is a managed worker and can block the Claude session from
    // ending while the task or branch are still pending.
    let cmd = format!(
        "YGG_SPAWNED=1 YGG_AGENT_NAME={agent} claude {flags} < {path:?} ; rm -f {path:?}",
        agent = shell_single_quote(agent_name),
        path = prime_path.to_string_lossy(),
    );
    Command::new("tmux")
        .args(["send-keys", "-t", &target, &cmd, "Enter"])
        .status()
        .map_err(|e| anyhow::anyhow!("tmux send-keys: {e}"))?;

    Ok(())
}

fn prime_prompt(task: &Task, repo: &crate::models::repo::Repo) -> String {
    let mut p = format!(
        "You have been spawned to work on a single ygg task.\n\n\
         **Task: {}-{}**  [{}]  P{}  kind={:?}\n\n\
         **Title:** {}\n\n",
        repo.task_prefix, task.seq, task.status, task.priority, task.kind, task.title,
    );
    if !task.description.is_empty() {
        p.push_str(&format!("**Description:**\n{}\n\n", task.description));
    }
    if let Some(a) = &task.acceptance
        && !a.is_empty()
    {
        p.push_str(&format!("**Acceptance:**\n{a}\n\n"));
    }
    if let Some(d) = &task.design
        && !d.is_empty()
    {
        p.push_str(&format!("**Design:**\n{d}\n\n"));
    }
    p.push_str(
        "Work in this git worktree. When complete, close the task with:\n  \
         `ygg task close ",
    );
    p.push_str(&format!(
        "{}-{} --reason \"...\"`\n",
        repo.task_prefix, task.seq
    ));
    p
}
