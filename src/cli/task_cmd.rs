use crate::models::agent::AgentRepo;
use crate::models::event::{EventKind, EventRepo};
use crate::models::repo::{RepoRepo, detect_git_repo, slugify};
use crate::models::task::{Task, TaskCreate, TaskKind, TaskRepo, TaskStatus, TaskUpdate};
use std::str::FromStr;
use uuid::Uuid;

/// Resolve the current working directory to a repo row, registering it
/// if this is the first time we've seen it. Falls back to a
/// non-git placeholder keyed on the absolute path.
pub async fn resolve_cwd_repo(
    pool: &sqlx::PgPool,
) -> Result<crate::models::repo::Repo, anyhow::Error> {
    let cwd = std::env::current_dir()?;
    let repo_repo = RepoRepo::new(pool);

    if let Some((url, toplevel, name)) = detect_git_repo(&cwd) {
        let prefix = slugify(&name);
        let repo = repo_repo
            .register(url.as_deref(), &name, &prefix, Some(&toplevel))
            .await?;
        return Ok(repo);
    }

    // Non-git directory: key by absolute path, prefix from basename
    let path_str = cwd.to_string_lossy().to_string();
    let name = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "scratch".to_string());
    let prefix = slugify(&name);
    let repo = repo_repo
        .register(None, &name, &prefix, Some(&path_str))
        .await?;
    Ok(repo)
}

async fn resolve_agent_id(
    pool: &sqlx::PgPool,
    agent_name: &str,
) -> Result<Option<Uuid>, anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    Ok(agent_repo
        .get_by_name(agent_name)
        .await?
        .map(|a| a.agent_id))
}

/// Public wrapper so plan_cmd can share the resolver without duplicating it.
pub async fn resolve_task_public(
    pool: &sqlx::PgPool,
    reference: &str,
) -> Result<Task, anyhow::Error> {
    resolve_task(pool, reference).await
}

/// Parse a task reference: either a UUID, or a "<prefix>-NNN" string.
async fn resolve_task(pool: &sqlx::PgPool, reference: &str) -> Result<Task, anyhow::Error> {
    // Full UUID is always a fast path.
    if let Ok(uuid) = Uuid::parse_str(reference) {
        let t = TaskRepo::new(pool)
            .get(uuid)
            .await?
            .ok_or_else(|| anyhow::anyhow!("task {uuid} not found"))?;
        return Ok(t);
    }

    // Short-UUID shorthand — prefix match on task_id::text. Accepted forms:
    //   baddbb20          (bare hex, ≥6 chars)
    //   ygg-baddbb20      (namespaced)
    // Ambiguous prefixes error out with the candidate count so the user
    // knows to paste more of the UUID.
    let hex_candidate = reference.strip_prefix("ygg-").unwrap_or(reference);
    if hex_candidate.len() >= 6 && hex_candidate.chars().all(|c| c.is_ascii_hexdigit()) {
        let matches: Vec<Uuid> =
            sqlx::query_scalar("SELECT task_id FROM tasks WHERE task_id::text LIKE $1 LIMIT 5")
                .bind(format!("{hex_candidate}%"))
                .fetch_all(pool)
                .await?;
        match matches.len() {
            0 => {} // fall through to prefix-seq resolver
            1 => {
                let t = TaskRepo::new(pool)
                    .get(matches[0])
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("task vanished"))?;
                return Ok(t);
            }
            n => anyhow::bail!(
                "ambiguous short-UUID '{reference}' ({n} matches) — paste more characters"
            ),
        }
    }

    // <prefix>-<seq>
    let (prefix, seq_str) = reference.rsplit_once('-').ok_or_else(|| {
        anyhow::anyhow!("expected UUID, ygg-<shortuuid>, or <prefix>-<seq>, got {reference}")
    })?;
    let seq: i32 = seq_str
        .parse()
        .map_err(|_| anyhow::anyhow!("sequence must be an integer: {seq_str}"))?;

    let repo = RepoRepo::new(pool)
        .get_by_prefix(prefix)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no repo with prefix '{prefix}'"))?;
    let t = TaskRepo::new(pool)
        .get_by_ref(repo.repo_id, seq)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task {prefix}-{seq} not found"))?;
    Ok(t)
}

pub struct CreateOpts<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub priority: Option<i16>,
    pub acceptance: Option<&'a str>,
    pub design: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub labels: &'a [String],
    pub agent_name: &'a str,
    pub external_ref: Option<&'a str>,
    /// Emit JSON to stdout instead of the human confirmation line.
    pub json: bool,
}

pub async fn create(pool: &sqlx::PgPool, opts: CreateOpts<'_>) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let created_by = resolve_agent_id(pool, opts.agent_name).await?;

    let kind = match opts.kind {
        Some(k) => TaskKind::from_str(k).map_err(|e| anyhow::anyhow!(e))?,
        None => TaskKind::default(),
    };

    let priority = opts.priority.unwrap_or(2);
    if !(0..=4).contains(&priority) {
        anyhow::bail!("priority must be between 0 (critical) and 4 (backlog)");
    }

    let labels: &[String] = opts.labels;

    let task = TaskRepo::new(pool)
        .create(
            repo.repo_id,
            created_by,
            TaskCreate {
                title: opts.title,
                description: opts.description.unwrap_or(""),
                acceptance: opts.acceptance,
                design: opts.design,
                notes: opts.notes,
                kind,
                priority,
                assignee: None,
                labels,
                external_ref: opts.external_ref,
            },
        )
        .await?;

    let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
    let _ = EventRepo::new(pool)
        .emit(
            EventKind::TaskCreated,
            opts.agent_name,
            created_by,
            serde_json::json!({
                "ref": task_ref.clone(),
                "title": task.title,
                "kind": task.kind.to_string(),
                "priority": task.priority,
            }),
        )
        .await;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ref": task_ref,
                "task": task,
            }))?
        );
    } else {
        println!("Created {}  {}", task_ref, task.title);
    }
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    all_repos: bool,
    status: Option<&str>,
    labels_and: &[String],
    labels_any: &[String],
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let statuses: Vec<TaskStatus> = match status {
        None => vec![],
        Some(s) => s
            .split(',')
            .map(|piece| piece.trim())
            .filter(|p| !p.is_empty())
            .map(TaskStatus::from_str)
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow::anyhow!(e))?,
    };
    let tasks = TaskRepo::new(pool)
        .list_multi(
            repo_id,
            if statuses.is_empty() {
                None
            } else {
                Some(&statuses)
            },
        )
        .await?;

    // Label filters. `--label a,b` = AND (task has ALL of a, b); `--label-any
    // a,b` = OR (task has AT LEAST ONE). Applied in-memory since the set is
    // already scoped to repo+status. Both filters can combine.
    let filtered = if labels_and.is_empty() && labels_any.is_empty() {
        tasks
    } else {
        let and_set: std::collections::HashSet<&str> =
            labels_and.iter().map(|s| s.as_str()).collect();
        let any_set: std::collections::HashSet<&str> =
            labels_any.iter().map(|s| s.as_str()).collect();
        let task_repo = TaskRepo::new(pool);
        let mut keep = Vec::with_capacity(tasks.len());
        for t in tasks {
            let task_labels = task_repo.labels(t.task_id).await.unwrap_or_default();
            let and_ok = and_set.iter().all(|l| task_labels.iter().any(|tl| tl == l));
            let any_ok =
                any_set.is_empty() || any_set.iter().any(|l| task_labels.iter().any(|tl| tl == l));
            if and_ok && any_ok {
                keep.push(t);
            }
        }
        keep
    };
    if json {
        emit_tasks_json(pool, &filtered).await
    } else {
        print_task_table(pool, &filtered).await
    }
}

pub async fn ready(pool: &sqlx::PgPool, json: bool) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let tasks = TaskRepo::new(pool).ready(repo.repo_id).await?;
    if json {
        return emit_tasks_json(pool, &tasks).await;
    }
    if tasks.is_empty() {
        println!("No ready tasks in {}.", repo.name);
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn stale(
    pool: &sqlx::PgPool,
    days: i32,
    all_repos: bool,
    status: Option<&str>,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let mut tasks = TaskRepo::new(pool).stale(repo_id, days).await?;
    if let Some(s) = status {
        let want = TaskStatus::from_str(s).map_err(|e| anyhow::anyhow!(e))?;
        tasks.retain(|t| t.status == want);
    }
    if json {
        return emit_tasks_json(pool, &tasks).await;
    }
    if tasks.is_empty() {
        println!("No stale tasks (> {days}d untouched).");
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn blocked(pool: &sqlx::PgPool, json: bool) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let tasks = TaskRepo::new(pool).blocked(repo.repo_id).await?;
    if json {
        return emit_tasks_json(pool, &tasks).await;
    }
    if tasks.is_empty() {
        println!("No blocked tasks in {}.", repo.name);
        return Ok(());
    }
    print_task_table(pool, &tasks).await
}

pub async fn show(pool: &sqlx::PgPool, reference: &str, json: bool) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let repo = RepoRepo::new(pool)
        .get(t.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo vanished"))?;
    let labels = TaskRepo::new(pool).labels(t.task_id).await?;
    let deps = TaskRepo::new(pool).deps(t.task_id).await?;

    if json {
        let links = TaskRepo::new(pool).links(t.task_id).await?;
        let deps_json: Vec<_> = deps
            .iter()
            .map(|d| {
                serde_json::json!({
                    "ref": format!("{}-{}", repo.task_prefix, d.seq),
                    "task_id": d.task_id,
                    "title": d.title,
                    "status": d.status.to_string(),
                })
            })
            .collect();
        let links_json: Vec<_> = links
            .iter()
            .map(|(k, id)| {
                serde_json::json!({
                    "kind": k,
                    "target_id": id,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ref": format!("{}-{}", repo.task_prefix, t.seq),
                "repo": repo.name,
                "task": t,
                "labels": labels,
                "deps": deps_json,
                "links": links_json,
            }))?
        );
        return Ok(());
    }

    let short_uuid = &t.task_id.to_string()[..8];
    println!(
        "{}-{}  [{}]  P{}  rel={}  {}  (ygg-{short_uuid})",
        repo.task_prefix, t.seq, t.status, t.priority, t.relevance, t.kind
    );
    println!();
    println!("  {}", t.title);
    if !t.description.is_empty() {
        println!();
        for line in t.description.lines() {
            println!("  {line}");
        }
    }
    if let Some(a) = &t.acceptance {
        println!();
        println!("  Acceptance:");
        for line in a.lines() {
            println!("    {line}");
        }
    }
    if let Some(d) = &t.design {
        println!();
        println!("  Design:");
        for line in d.lines() {
            println!("    {line}");
        }
    }
    if let Some(n) = &t.notes {
        println!();
        println!("  Notes:");
        for line in n.lines() {
            println!("    {line}");
        }
    }
    if !labels.is_empty() {
        println!();
        println!("  Labels: {}", labels.join(", "));
    }
    if let Some(r) = t.external_ref.as_deref().filter(|s| !s.is_empty()) {
        println!();
        println!("  External: {r}");
    }
    if !deps.is_empty() {
        println!();
        println!("  Depends on:");
        for d in &deps {
            let indicator = if matches!(d.status, TaskStatus::Closed) {
                "✓"
            } else {
                "·"
            };
            println!(
                "    {indicator} {}-{} [{}] {}",
                repo.task_prefix, d.seq, d.status, d.title
            );
        }
    }
    let links = TaskRepo::new(pool).links(t.task_id).await?;
    if !links.is_empty() {
        println!();
        println!("  Links:");
        for (kind, target_id) in &links {
            // Best-effort target title lookup.
            let row: Option<(i32, Uuid, String)> = sqlx::query_as(
                "SELECT t.seq, t.repo_id, t.title FROM tasks t WHERE t.task_id = $1",
            )
            .bind(target_id)
            .fetch_optional(pool)
            .await?;
            if let Some((seq, repo_id, title)) = row {
                let prefix = RepoRepo::new(pool)
                    .get(repo_id)
                    .await?
                    .map(|r| r.task_prefix)
                    .unwrap_or_else(|| "?".into());
                println!("    [{kind}]  {prefix}-{seq}  {title}");
            } else {
                println!("    [{kind}]  {target_id}");
            }
        }
    }
    if t.human_flag {
        println!();
        println!("  [flagged for human decision]");
    }

    // Run history (ADR 0016). Prints when at least one task_runs row exists.
    let runs = crate::models::task_run::TaskRunRepo::new(pool)
        .list_by_task(t.task_id)
        .await?;
    if !runs.is_empty() {
        println!();
        println!("  Runs:");
        for r in &runs {
            let started = r
                .started_at
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "—".into());
            let duration = match (r.started_at, r.ended_at) {
                (Some(s), Some(e)) => format!("{}s", (e - s).num_seconds().max(0)),
                (Some(s), None) => format!("{}s+", (chrono::Utc::now() - s).num_seconds().max(0)),
                _ => "—".into(),
            };
            let commit = r
                .output_commit_sha
                .as_deref()
                .map(|s| &s[..s.len().min(10)])
                .unwrap_or("");
            println!(
                "    #{}  {:<9}  {:<19}  {:>8}  {}  {}",
                r.attempt,
                r.state.as_str(),
                r.reason.as_str(),
                started,
                duration,
                commit,
            );
        }
    }
    Ok(())
}

pub async fn update(
    pool: &sqlx::PgPool,
    reference: &str,
    title: Option<&str>,
    description: Option<&str>,
    priority: Option<i16>,
    kind: Option<&str>,
    acceptance: Option<&str>,
    design: Option<&str>,
    notes: Option<&str>,
    external_ref: Option<Option<&str>>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;

    let kind = kind
        .map(|k| TaskKind::from_str(k).map_err(|e| anyhow::anyhow!(e)))
        .transpose()?;

    TaskRepo::new(pool)
        .update(
            t.task_id,
            agent_id,
            TaskUpdate {
                title,
                description,
                acceptance,
                design,
                notes,
                kind,
                priority,
                assignee: None,
                human_flag: None,
                external_ref,
            },
        )
        .await?;
    println!("Updated {}", reference);
    Ok(())
}

pub async fn set_status(
    pool: &sqlx::PgPool,
    reference: &str,
    status: &str,
    reason: Option<&str>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let status = TaskStatus::from_str(status).map_err(|e| anyhow::anyhow!(e))?;

    TaskRepo::new(pool)
        .set_status(t.task_id, status.clone(), agent_id, reason)
        .await?;

    let repo = RepoRepo::new(pool).get(t.repo_id).await?;
    let _ = EventRepo::new(pool).emit(
        EventKind::TaskStatusChanged,
        agent_name,
        agent_id,
        serde_json::json!({
            "ref": format!("{}-{}", repo.as_ref().map(|r| r.task_prefix.as_str()).unwrap_or("?"), t.seq),
            "to": status.to_string(),
            "reason": reason,
            "title": t.title,
        }),
    ).await;

    println!("{reference} → {status}");
    Ok(())
}

pub async fn claim(
    pool: &sqlx::PgPool,
    reference: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    TaskRepo::new(pool)
        .update(
            t.task_id,
            agent_id,
            TaskUpdate {
                assignee: Some(agent_id),
                ..Default::default()
            },
        )
        .await?;
    TaskRepo::new(pool)
        .set_status(t.task_id, TaskStatus::InProgress, agent_id, None)
        .await?;

    // ADR 0016 manual-mode parity: open a task_runs row so the run history is
    // visible in `ygg task show` and matches what the scheduler would have
    // written for an auto-dispatched claim.
    let run =
        super::run_cmd::open_for_task(pool, t.task_id, agent_id, serde_json::json!({})).await?;
    let _ = crate::models::event::EventRepo::new(pool)
        .emit(
            crate::models::event::EventKind::RunClaimed,
            agent_name,
            agent_id,
            serde_json::json!({
                "task_ref": reference,
                "run_id": run.run_id,
                "attempt": run.attempt,
                "agent": agent_name,
            }),
        )
        .await;
    println!(
        "{reference} claimed by {agent_name} (run #{} {})",
        run.attempt, run.run_id
    );

    // yggdrasil-82: surface scoped learnings whose file_glob matches any
    // file path mentioned in the task's text fields, plus repo-wide and
    // global learnings. Best-effort — silent on Postgres hiccups.
    let mut text = t.title.clone();
    if !t.description.is_empty() {
        text.push('\n');
        text.push_str(&t.description);
    }
    if let Some(a) = &t.acceptance {
        text.push('\n');
        text.push_str(a);
    }
    if let Some(d) = &t.design {
        text.push('\n');
        text.push_str(d);
    }
    if let Some(n) = &t.notes {
        text.push('\n');
        text.push_str(n);
    }
    let files = crate::cli::learning_cmd::extract_file_mentions(&text);
    let kind_str = t.kind.to_string();
    if let Ok(lines) = crate::cli::learning_cmd::surface_for_files(
        pool,
        Some(t.repo_id),
        &files,
        Some(agent_name),
        Some(&kind_str),
    )
    .await
    {
        for line in lines {
            println!("{line}");
        }
    }

    Ok(())
}

pub async fn close(
    pool: &sqlx::PgPool,
    reference: &str,
    reason: Option<&str>,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let agent_id = resolve_agent_id(pool, agent_name).await?;

    // Finalize the current run BEFORE flipping task status so the event
    // ordering is run_terminal → task_status_changed.
    let reason_str = reason.unwrap_or("");
    let (run_state, run_reason) = classify_close_reason(reason_str);
    let _ = super::run_cmd::finalize_for_task(
        pool, t.task_id, run_state, run_reason, agent_name, agent_id,
    )
    .await?;

    set_status(pool, reference, "closed", reason, agent_name).await
}

/// Heuristic mapping from free-text close reason → run terminal state. Manual
/// closes that mention "fail"/"crash"/"cancel" preserve that nuance in the
/// run history; everything else is treated as a successful manual close.
fn classify_close_reason(
    reason: &str,
) -> (
    crate::models::task_run::RunState,
    crate::models::task_run::RunReason,
) {
    use crate::models::task_run::{RunReason, RunState};
    let r = reason.to_ascii_lowercase();
    if r.contains("crash") {
        (RunState::Crashed, RunReason::TmuxGone)
    } else if r.contains("cancel") || r.contains("abort") {
        (RunState::Cancelled, RunReason::UserCancelled)
    } else if r.contains("fail") || r.contains("error") {
        (RunState::Failed, RunReason::AgentError)
    } else {
        (RunState::Succeeded, RunReason::Ok)
    }
}

/// Reassign a task filed in the wrong repo to the right one, by destination
/// task_prefix. The task keeps its UUID; its human ref is renumbered into the
/// target repo (`yggdrasil-42` → `canairy-7`).
pub async fn move_to(
    pool: &sqlx::PgPool,
    reference: &str,
    target_prefix: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let target = RepoRepo::new(pool)
        .get_by_prefix(target_prefix)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no repo with prefix '{target_prefix}' — `ygg task list --all` shows known prefixes"
            )
        })?;
    if target.repo_id == t.repo_id {
        anyhow::bail!("{reference} is already in '{target_prefix}'");
    }
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let new_seq = TaskRepo::new(pool)
        .move_to_repo(t.task_id, target.repo_id, agent_id)
        .await?;
    println!("{reference} → {}-{new_seq}", target.task_prefix);
    Ok(())
}

pub async fn add_dep(
    pool: &sqlx::PgPool,
    task_ref: &str,
    blocker_ref: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, task_ref).await?;
    let b = resolve_task(pool, blocker_ref).await?;
    TaskRepo::new(pool).add_dep(t.task_id, b.task_id).await?;
    println!("{task_ref} now depends on {blocker_ref}");
    Ok(())
}

pub async fn remove_dep(
    pool: &sqlx::PgPool,
    task_ref: &str,
    blocker_ref: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, task_ref).await?;
    let b = resolve_task(pool, blocker_ref).await?;
    TaskRepo::new(pool).remove_dep(t.task_id, b.task_id).await?;
    println!("dependency removed: {task_ref} ← {blocker_ref}");
    Ok(())
}

pub async fn label_add(
    pool: &sqlx::PgPool,
    reference: &str,
    label: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    TaskRepo::new(pool).add_label(t.task_id, label).await?;
    println!("{reference} + {label}");
    Ok(())
}

pub async fn label_remove(
    pool: &sqlx::PgPool,
    reference: &str,
    label: &str,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let n = TaskRepo::new(pool).remove_label(t.task_id, label).await?;
    if n == 0 {
        println!("{reference}: no such label '{label}'");
    } else {
        println!("{reference} − {label}");
    }
    Ok(())
}

pub async fn label_list(
    pool: &sqlx::PgPool,
    reference: &str,
    json: bool,
) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let labels = TaskRepo::new(pool).labels(t.task_id).await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ref": reference,
                "labels": labels,
            }))?
        );
    } else if labels.is_empty() {
        println!("{reference}: no labels");
    } else {
        println!("{reference}: {}", labels.join(", "));
    }
    Ok(())
}

pub async fn label_list_all(
    pool: &sqlx::PgPool,
    all_repos: bool,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let pairs = TaskRepo::new(pool).all_labels(repo_id).await?;
    if json {
        let rows: Vec<_> = pairs
            .iter()
            .map(|(l, n)| {
                serde_json::json!({
                    "label": l, "count": n,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "count": pairs.len(),
                "results": rows,
            }))?
        );
        return Ok(());
    }
    if pairs.is_empty() {
        println!("No labels.");
        return Ok(());
    }
    println!("{:<4}  {}", "N", "LABEL");
    for (label, count) in &pairs {
        println!("{count:<4}  {label}");
    }
    Ok(())
}

pub async fn bump(pool: &sqlx::PgPool, reference: &str, delta: i32) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let new_val = TaskRepo::new(pool).bump_relevance(t.task_id, delta).await?;
    let sign = if delta >= 0 { "+" } else { "" };
    println!("{reference} relevance {sign}{delta} → {new_val}");
    Ok(())
}

pub async fn link(
    pool: &sqlx::PgPool,
    from_ref: &str,
    to_ref: &str,
    kind: &str,
) -> Result<(), anyhow::Error> {
    // Accept both the "see-also" and "see_also" spelling.
    let normalized = kind.replace('-', "_");
    let allowed = ["see_also", "superseded_by", "duplicate_of", "related"];
    if !allowed.contains(&normalized.as_str()) {
        anyhow::bail!("unknown link kind '{kind}' — try: {}", allowed.join(", "));
    }
    let a = resolve_task(pool, from_ref).await?;
    let b = resolve_task(pool, to_ref).await?;
    TaskRepo::new(pool)
        .add_link(a.task_id, b.task_id, &normalized)
        .await?;
    println!("{from_ref}  --[{normalized}]-->  {to_ref}");
    Ok(())
}

/// Walk task_deps and report any dependency cycles. Adds-ons to add_dep's
/// already-present cycle guard catch any pre-existing rows (e.g. data
/// imported before the guard landed, or rows inserted via raw SQL).
pub async fn lint(pool: &sqlx::PgPool, all_repos: bool, json: bool) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let cycles = TaskRepo::new(pool).find_cycles(repo_id).await?;

    // Resolve every task_id we'll print into a "<prefix>-<seq>" string in one
    // batch so a 50-cycle report doesn't issue 50N queries.
    let mut all_ids: Vec<Uuid> = cycles.iter().flatten().copied().collect();
    all_ids.sort();
    all_ids.dedup();
    let refs: std::collections::HashMap<Uuid, String> = if all_ids.is_empty() {
        Default::default()
    } else {
        let rows: Vec<(Uuid, String, i32)> = sqlx::query_as(
            r#"SELECT t.task_id, r.task_prefix, t.seq
               FROM tasks t JOIN repos r ON r.repo_id = t.repo_id
               WHERE t.task_id = ANY($1)"#,
        )
        .bind(&all_ids)
        .fetch_all(pool)
        .await?;
        rows.into_iter()
            .map(|(id, prefix, seq)| (id, format!("{prefix}-{seq}")))
            .collect()
    };

    if json {
        let payload: Vec<Vec<&str>> = cycles
            .iter()
            .map(|c| {
                c.iter()
                    .map(|id| refs.get(id).map(String::as_str).unwrap_or("?"))
                    .collect()
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if cycles.is_empty() {
        println!("no cycles");
        return Ok(());
    }

    println!("found {} cycle(s) in task_deps:", cycles.len());
    for cycle in &cycles {
        let parts: Vec<&str> = cycle
            .iter()
            .map(|id| refs.get(id).map(String::as_str).unwrap_or("?"))
            .collect();
        // Append the start vertex so the loop is visually closed.
        let first = parts.first().copied().unwrap_or("?");
        println!("  cycle: {} → {first}", parts.join(" → "));
    }
    // Non-zero exit so scripts can chain `ygg task lint && ...`.
    std::process::exit(1);
}

/// Soft-delete a task: stamp deleted_at = now() so it stops appearing in
/// list/ready/blocked/dupes. Idempotent: re-deleting a trashed task
/// touches deleted_at (resets the purge clock).
pub async fn delete(pool: &sqlx::PgPool, reference: &str) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let existed = TaskRepo::new(pool).soft_delete(t.task_id).await?;
    if existed {
        println!("{reference} → trashed (restore with `ygg task restore {reference}`)");
    } else {
        anyhow::bail!("{reference}: not found");
    }
    Ok(())
}

/// Pull a task back from the trash.
pub async fn restore(pool: &sqlx::PgPool, reference: &str) -> Result<(), anyhow::Error> {
    let t = resolve_task(pool, reference).await?;
    let changed = TaskRepo::new(pool).restore(t.task_id).await?;
    if changed {
        println!("{reference} → restored");
    } else {
        println!("{reference}: not in trash");
    }
    Ok(())
}

/// List trashed tasks (scope: current repo or --all). The `ygg task list`
/// view stays clean; trash is its own pane.
pub async fn trash(pool: &sqlx::PgPool, all_repos: bool, json: bool) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let rows = TaskRepo::new(pool).list_trashed(repo_id).await?;

    let prefixes: std::collections::HashMap<Uuid, String> = if rows.is_empty() {
        Default::default()
    } else {
        let ids: Vec<Uuid> = rows.iter().map(|t| t.repo_id).collect();
        let pairs: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT repo_id, task_prefix FROM repos WHERE repo_id = ANY($1)")
                .bind(&ids)
                .fetch_all(pool)
                .await?;
        pairs.into_iter().collect()
    };

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|t| {
                serde_json::json!({
                    "ref": format!(
                        "{}-{}",
                        prefixes.get(&t.repo_id).map(String::as_str).unwrap_or("?"),
                        t.seq
                    ),
                    "title": t.title,
                    "status": t.status.to_string(),
                    "kind": t.kind.to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("trash is empty");
        return Ok(());
    }
    println!("{} trashed task(s):", rows.len());
    for t in &rows {
        let prefix = prefixes.get(&t.repo_id).map(String::as_str).unwrap_or("?");
        println!("  {prefix}-{}  [{}]  {}", t.seq, t.status, t.title);
    }
    Ok(())
}

/// Hard-delete every trashed task whose deleted_at is older than `days`.
/// Intended for cron / launchd; safe to run interactively. Default 30 days.
pub async fn purge(pool: &sqlx::PgPool, days: i32, all_repos: bool) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let n = TaskRepo::new(pool).purge_older_than(days, repo_id).await?;
    println!("purged {n} trashed task(s) older than {days}d");
    Ok(())
}

pub async fn stats(pool: &sqlx::PgPool, all_repos: bool, json: bool) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let s = TaskRepo::new(pool).stats(repo_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
    } else {
        println!(
            "open: {}    in_progress: {}    blocked: {}    closed: {}",
            s.open, s.in_progress, s.blocked, s.closed
        );
    }
    Ok(())
}

/// Parse a markdown file into a task tree.
///
/// Header level → kind: H1 = Epic, H2 = Feature, H3/H4 = Task.
/// Body text between a header and the next header of any level becomes that
/// task's description. Parent→child dep edges are inserted so
/// `ygg task ready` surfaces the leaves and the parent rolls up on close.
pub async fn create_from_markdown(
    pool: &sqlx::PgPool,
    path: &std::path::Path,
    agent_name: &str,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo = resolve_cwd_repo(pool).await?;
    let created_by = resolve_agent_id(pool, agent_name).await?;
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;

    let parsed = parse_markdown_tasks(&source);
    if parsed.is_empty() {
        anyhow::bail!("no H1–H4 headers found in {}", path.display());
    }

    // Walk parsed list, tracking the most recent task_id at each level.
    // parent for level N = most recent task at level < N (typically N-1).
    let mut stack: [Option<Uuid>; 5] = [None; 5]; // index 1..=4 used
    let mut created: Vec<(String, Task)> = Vec::with_capacity(parsed.len());
    let task_repo = TaskRepo::new(pool);
    let event_repo = EventRepo::new(pool);

    for p in &parsed {
        let kind = match p.level {
            1 => TaskKind::Epic,
            2 => TaskKind::Feature,
            _ => TaskKind::Task,
        };
        // Priority defaults to 2 (medium) for bulk imports — users override
        // per-task later.
        let task = task_repo
            .create(
                repo.repo_id,
                created_by,
                TaskCreate {
                    title: &p.title,
                    description: &p.body,
                    kind,
                    priority: 2,
                    ..Default::default()
                },
            )
            .await?;

        // Link to parent: parent-task depends on child-task so the parent
        // stays blocked until all children close (rollup semantics).
        let lvl = p.level as usize;
        for parent_lvl in (1..lvl).rev() {
            if let Some(parent_id) = stack[parent_lvl] {
                task_repo.add_dep(parent_id, task.task_id).await.ok();
                break;
            }
        }

        stack[lvl] = Some(task.task_id);
        // Clear any deeper levels so orphaned stacks don't misparent.
        for deeper in (lvl + 1)..=4 {
            stack[deeper] = None;
        }

        let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
        let _ = event_repo
            .emit(
                EventKind::TaskCreated,
                agent_name,
                created_by,
                serde_json::json!({
                    "ref": task_ref,
                    "title": task.title,
                    "kind": task.kind.to_string(),
                    "source": "markdown",
                }),
            )
            .await;
        created.push((task_ref, task));
    }

    if json {
        let out: Vec<_> = created
            .iter()
            .map(|(r, t)| {
                serde_json::json!({
                    "ref": r,
                    "task": t,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "count": created.len(),
                "source": path.display().to_string(),
                "results": out,
            }))?
        );
    } else {
        println!("Parsed {} task(s) from {}", created.len(), path.display());
        for (r, t) in &created {
            let indent = "  ".repeat(match t.kind {
                TaskKind::Epic => 0,
                TaskKind::Feature => 1,
                _ => 2,
            });
            println!("  {indent}{r}  {}", t.title);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct ParsedHeader<'a> {
    level: u8,
    title: String,
    body: String,
    _phantom: std::marker::PhantomData<&'a ()>,
}

/// Minimal markdown-headers parser: `#{1,4}\s+Title`, body is everything
/// between a header and the next header. No pulldown-cmark dep — we don't
/// care about inline markdown, just the section structure.
fn parse_markdown_tasks(source: &str) -> Vec<ParsedHeader<'_>> {
    let mut out: Vec<ParsedHeader> = Vec::new();
    let mut current: Option<ParsedHeader> = None;
    let mut body_buf = String::new();

    for line in source.lines() {
        let trimmed = line.trim_start();
        let hash_count = trimmed.chars().take_while(|c| *c == '#').count();
        let is_header =
            hash_count >= 1 && hash_count <= 4 && trimmed.chars().nth(hash_count) == Some(' ');

        if is_header {
            if let Some(mut prev) = current.take() {
                prev.body = body_buf.trim().to_string();
                body_buf.clear();
                out.push(prev);
            }
            let title = trimmed[hash_count + 1..].trim().to_string();
            current = Some(ParsedHeader {
                level: hash_count as u8,
                title,
                body: String::new(),
                _phantom: std::marker::PhantomData,
            });
        } else if current.is_some() {
            body_buf.push_str(line);
            body_buf.push('\n');
        }
    }
    if let Some(mut prev) = current.take() {
        prev.body = body_buf.trim().to_string();
        out.push(prev);
    }
    out
}

pub async fn dupes(
    pool: &sqlx::PgPool,
    all_repos: bool,
    min_similarity: f64,
    limit: i64,
    json: bool,
) -> Result<(), anyhow::Error> {
    if !(0.0..=1.0).contains(&min_similarity) {
        anyhow::bail!("min_similarity must be between 0.0 and 1.0");
    }
    let repo_id = if all_repos {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let pairs = TaskRepo::new(pool)
        .find_dupes(repo_id, min_similarity, limit)
        .await?;

    if json {
        let repo_repo = RepoRepo::new(pool);
        let mut prefixes: std::collections::HashMap<Uuid, String> =
            std::collections::HashMap::new();
        for (a, b, _) in &pairs {
            for id in [a.repo_id, b.repo_id] {
                if !prefixes.contains_key(&id) {
                    if let Some(r) = repo_repo.get(id).await? {
                        prefixes.insert(id, r.task_prefix);
                    }
                }
            }
        }
        let out: Vec<_> = pairs
            .iter()
            .map(|(a, b, sim)| {
                let pa = prefixes.get(&a.repo_id).cloned().unwrap_or_default();
                let pb = prefixes.get(&b.repo_id).cloned().unwrap_or_default();
                serde_json::json!({
                    "similarity": sim,
                    "a": { "ref": format!("{pa}-{}", a.seq), "title": a.title },
                    "b": { "ref": format!("{pb}-{}", b.seq), "title": b.title },
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "count": pairs.len(),
                "min_similarity": min_similarity,
                "results": out,
            }))?
        );
        return Ok(());
    }

    if pairs.is_empty() {
        println!(
            "No probable duplicates above {:.0}% similarity.",
            min_similarity * 100.0
        );
        return Ok(());
    }
    let repo_repo = RepoRepo::new(pool);
    let mut prefixes: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for (a, b, _) in &pairs {
        for id in [a.repo_id, b.repo_id] {
            if !prefixes.contains_key(&id) {
                if let Some(r) = repo_repo.get(id).await? {
                    prefixes.insert(id, r.task_prefix);
                }
            }
        }
    }
    println!("{} probable duplicate pair(s):", pairs.len());
    for (a, b, sim) in &pairs {
        let pa = prefixes.get(&a.repo_id).cloned().unwrap_or_default();
        let pb = prefixes.get(&b.repo_id).cloned().unwrap_or_default();
        println!("  {:>3.0}%  {pa}-{}  ↔  {pb}-{}", sim * 100.0, a.seq, b.seq);
        println!("         {}", truncate_cli(&a.title, 70));
        println!("         {}", truncate_cli(&b.title, 70));
    }
    Ok(())
}

fn truncate_cli(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// JSON emit path shared by list/ready/blocked. Wraps each task with its
/// repo-qualified ref so downstream agents don't have to look up prefixes.
async fn emit_tasks_json(pool: &sqlx::PgPool, tasks: &[Task]) -> Result<(), anyhow::Error> {
    let repo_repo = RepoRepo::new(pool);
    let mut prefixes: std::collections::HashMap<Uuid, String> = std::collections::HashMap::new();
    for t in tasks {
        if !prefixes.contains_key(&t.repo_id) {
            if let Some(r) = repo_repo.get(t.repo_id).await? {
                prefixes.insert(t.repo_id, r.task_prefix);
            }
        }
    }
    let out: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            let prefix = prefixes.get(&t.repo_id).cloned().unwrap_or_default();
            serde_json::json!({
                "ref": format!("{prefix}-{}", t.seq),
                "task": t,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "count": tasks.len(),
            "results": out,
        }))?
    );
    Ok(())
}

async fn print_task_table(pool: &sqlx::PgPool, tasks: &[Task]) -> Result<(), anyhow::Error> {
    if tasks.is_empty() {
        println!("No tasks.");
        return Ok(());
    }

    // Build a repo_id -> prefix lookup for display
    let repo_repo = RepoRepo::new(pool);
    let mut prefixes = std::collections::HashMap::new();
    for t in tasks {
        if !prefixes.contains_key(&t.repo_id) {
            if let Some(r) = repo_repo.get(t.repo_id).await? {
                prefixes.insert(t.repo_id, r.task_prefix);
            }
        }
    }

    println!(
        "{:<16} {:<12} {:<3} {:<8} {}",
        "ID", "STATUS", "P", "KIND", "TITLE"
    );
    for t in tasks {
        let id = format!(
            "{}-{}",
            prefixes.get(&t.repo_id).cloned().unwrap_or_default(),
            t.seq
        );
        let title = if t.title.len() > 60 {
            format!("{}…", &t.title[..59])
        } else {
            t.title.clone()
        };
        println!(
            "{:<16} {:<12} P{:<2} {:<8} {}",
            id,
            t.status.to_string(),
            t.priority,
            t.kind.to_string(),
            title
        );
    }
    println!("\n{} task(s).", tasks.len());
    Ok(())
}
