//! `ygg rollup` — per-repo activity summary over a time window.
//!
//! Aggregates tasks created/closed. Output is Markdown so users can pipe
//! it to a file or into a weekly review doc. JSON output is there for
//! tooling; plain-text for quick terminal reads.

use crate::db::DbPool;
use chrono::{DateTime, Duration, Utc};

use crate::models::repo::{Repo, RepoRepo};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

pub async fn execute(
    pool: &DbPool,
    days: i64,
    repo_filter: Option<&str>,
    format: Format,
) -> Result<(), anyhow::Error> {
    let since = Utc::now() - Duration::days(days);
    let repos = RepoRepo::new(pool).list().await.unwrap_or_default();
    let repos: Vec<Repo> = match repo_filter {
        Some(prefix) => repos
            .into_iter()
            .filter(|r| r.task_prefix == prefix)
            .collect(),
        None => repos,
    };

    if repos.is_empty() {
        if let Some(p) = repo_filter {
            anyhow::bail!("no repo with prefix '{p}'");
        }
        println!("(no repos registered)");
        return Ok(());
    }

    // Collect per-repo rollups.
    let mut rollups: Vec<RepoRollup> = Vec::new();
    for repo in &repos {
        rollups.push(build_repo_rollup(pool, repo, since).await?);
    }

    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(
                &rollups.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
            )?;
            println!("{json}");
        }
        Format::Markdown => print_markdown(days, &rollups),
        Format::Text => print_text(days, &rollups),
    }
    Ok(())
}

struct RepoRollup {
    repo: Repo,
    since: DateTime<Utc>,
    tasks_created: Vec<TaskRow>,
    tasks_closed: Vec<TaskRow>,
}

struct TaskRow {
    seq: i32,
    kind: String,
    priority: i16,
    title: String,
}

async fn build_repo_rollup(
    pool: &DbPool,
    repo: &Repo,
    since: DateTime<Utc>,
) -> Result<RepoRollup, anyhow::Error> {
    let created: Vec<(i32, String, i16, String)> = sqlx::query_as(
        r#"SELECT seq, kind, priority, title FROM tasks
           WHERE repo_id = $1 AND created_at >= $2
           ORDER BY priority ASC, seq ASC"#,
    )
    .bind(repo.repo_id.clone())
    .bind(since)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let tasks_created: Vec<TaskRow> = created
        .into_iter()
        .map(|(seq, kind, priority, title)| TaskRow {
            seq,
            kind,
            priority,
            title,
        })
        .collect();

    let closed: Vec<(i32, String, i16, String)> = sqlx::query_as(
        r#"SELECT seq, kind, priority, title FROM tasks
           WHERE repo_id = $1 AND closed_at >= $2 AND status = 'closed'
           ORDER BY closed_at DESC"#,
    )
    .bind(repo.repo_id.clone())
    .bind(since)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let tasks_closed: Vec<TaskRow> = closed
        .into_iter()
        .map(|(seq, kind, priority, title)| TaskRow {
            seq,
            kind,
            priority,
            title,
        })
        .collect();

    Ok(RepoRollup {
        repo: repo.clone(),
        since,
        tasks_created,
        tasks_closed,
    })
}

fn print_markdown(days: i64, rollups: &[RepoRollup]) {
    let period = match days {
        1 => "day".to_string(),
        7 => "week".to_string(),
        n => format!("{n} days"),
    };
    println!("# Yggdrasil rollup — last {period}");
    println!();
    for r in rollups {
        println!("## {} ({})", r.repo.name, r.repo.task_prefix);
        println!();
        println!("Since: {}", r.since.format("%Y-%m-%d %H:%M UTC"));
        println!();

        if !r.tasks_closed.is_empty() {
            println!("### Closed ({})", r.tasks_closed.len());
            for t in &r.tasks_closed {
                println!(
                    "- `{}-{}` P{} [{}] {}",
                    r.repo.task_prefix, t.seq, t.priority, t.kind, t.title
                );
            }
            println!();
        }

        if !r.tasks_created.is_empty() {
            println!("### Created ({})", r.tasks_created.len());
            for t in &r.tasks_created {
                println!(
                    "- `{}-{}` P{} [{}] {}",
                    r.repo.task_prefix, t.seq, t.priority, t.kind, t.title
                );
            }
            println!();
        }
    }
}

fn print_text(days: i64, rollups: &[RepoRollup]) {
    println!("Yggdrasil rollup — last {days} day(s)");
    for r in rollups {
        println!();
        println!("  {} ({})", r.repo.name, r.repo.task_prefix);
        println!(
            "    closed {} · created {}",
            r.tasks_closed.len(),
            r.tasks_created.len(),
        );
        if !r.tasks_closed.is_empty() {
            println!("    closed:");
            for t in r.tasks_closed.iter().take(5) {
                println!(
                    "      {}-{}  {}",
                    r.repo.task_prefix,
                    t.seq,
                    truncate(&t.title, 60)
                );
            }
        }
    }
}

impl RepoRollup {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "repo": { "name": self.repo.name, "prefix": self.repo.task_prefix },
            "since": self.since,
            "tasks_created": self.tasks_created.iter().map(|t| serde_json::json!({
                "ref": format!("{}-{}", self.repo.task_prefix, t.seq),
                "priority": t.priority, "kind": t.kind, "title": t.title,
            })).collect::<Vec<_>>(),
            "tasks_closed": self.tasks_closed.iter().map(|t| serde_json::json!({
                "ref": format!("{}-{}", self.repo.task_prefix, t.seq),
                "priority": t.priority, "kind": t.kind, "title": t.title,
            })).collect::<Vec<_>>(),
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}
