//! `ygg remember "..."` — persist a durable note scoped to the current repo
//! (or --global). Re-added after ADR 0015: this is a plain note store, NOT the
//! removed embedding/similarity corpus. Notes surface deterministically in
//! `ygg prime` (SessionStart) and `ygg remember --list`.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::memory::MemoryRepo;
use crate::models::repo::RepoRepo;
use uuid::Uuid;

async fn resolve_agent_id(pool: &sqlx::PgPool, agent_name: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT agent_id FROM agents WHERE agent_name = $1")
        .bind(agent_name)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

pub async fn remember(
    pool: &sqlx::PgPool,
    text: &str,
    global: bool,
    agent_name: &str,
    json: bool,
) -> Result<(), anyhow::Error> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("nothing to remember — pass a note, or use --list to read them");
    }

    // Best-effort repo detection; a note without a repo is still valid (global).
    let repo_id = if global {
        None
    } else {
        match resolve_cwd_repo(pool).await {
            Ok(r) => Some(r.repo_id),
            Err(_) => None,
        }
    };
    let created_by = resolve_agent_id(pool, agent_name).await;

    let memory = MemoryRepo::new(pool)
        .create(repo_id, text, created_by)
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&memory)?);
        return Ok(());
    }

    match repo_id {
        Some(rid) => {
            let scope = RepoRepo::new(pool)
                .get(rid)
                .await?
                .map(|r| format!("{} ({})", r.name, r.task_prefix))
                .unwrap_or_else(|| "this repo".to_string());
            println!("Remembered · scoped to {scope}");
        }
        None => println!("Remembered · global (visible from every repo)"),
    }
    Ok(())
}

pub async fn list(
    pool: &sqlx::PgPool,
    all: bool,
    limit: i64,
    json: bool,
) -> Result<(), anyhow::Error> {
    // Scope the same way `remember` writes: current repo (+ global) unless --all.
    let repo_id = if all {
        None
    } else {
        resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id)
    };
    let rows = MemoryRepo::new(pool).list(repo_id, all, limit).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "count": rows.len(),
                "results": rows,
            }))?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("No notes. Write one with `ygg remember \"...\"`.");
        return Ok(());
    }
    for m in &rows {
        let scope = if m.repo_id.is_none() {
            "global"
        } else {
            "repo"
        };
        let when = m.created_at.format("%Y-%m-%d");
        println!("  · [{scope} · {when}] {}", short(&m.text, 120));
    }
    Ok(())
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
