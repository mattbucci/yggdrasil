//! `ygg learn` — scoped rule capture with deterministic match.
//!
//! See ADR 0015 and model/learning.rs. This is the orchestration-layer
//! replacement for fuzzy-similarity retrieval of durable rules. Scope
//! tuples are `(repo_id, file_glob, rule_id)`; lookups are SQL predicates,
//! not cosine thresholds.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::learning::LearningRepo;

pub fn parse_scope_tags(scopes: &[String]) -> Result<serde_json::Value, anyhow::Error> {
    let mut map = serde_json::Map::new();
    for s in scopes {
        if s == "global" {
            continue;
        }
        if let Some((raw_key, raw_val)) = s.split_once('=') {
            let key = raw_key.trim();
            let val = raw_val.trim();
            if val.is_empty() {
                anyhow::bail!("invalid --scope '{s}': value cannot be empty");
            }
            match key {
                "agent" | "kind" => {
                    map.insert(key.to_string(), serde_json::Value::String(val.to_string()));
                }
                _ => {
                    anyhow::bail!(
                        "invalid --scope key '{key}', expected agent=<name> or kind=<task-kind>"
                    );
                }
            }
        } else {
            anyhow::bail!(
                "invalid --scope '{s}', expected global, agent=<name>, or kind=<task-kind>"
            );
        }
    }
    Ok(serde_json::Value::Object(map))
}

/// `ygg learn create` / `propose`. `status`/`source` carry the ADR 0017
/// lifecycle: `("active","manual")` is today's immediately-firing create,
/// `("pending","manual")` is `create --pending`, `("pending","proposed")`
/// is `propose`.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    pool: &crate::db::DbPool,
    text: &str,
    global: bool,
    file_glob: Option<&str>,
    rule_id: Option<&str>,
    context: Option<&str>,
    agent_name: &str,
    scope_tags: &serde_json::Value,
    status: &str,
    source: &str,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if global {
        None
    } else {
        Some(resolve_cwd_repo(pool).await?.repo_id)
    };
    let created_by: Option<String> =
        sqlx::query_scalar("SELECT agent_id FROM agents WHERE agent_name = $1")
            .bind(agent_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let learning = LearningRepo::new(pool)
        .create(
            repo_id.clone(),
            file_glob,
            rule_id,
            text,
            context,
            created_by,
            scope_tags,
            status,
            source,
        )
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&learning)?);
        return Ok(());
    }
    let scope = format_scope_label(repo_id, file_glob, rule_id, scope_tags);
    // `proposed` and `--pending` learnings land in the gate, not the live
    // corpus — say so, and point at the triage verb.
    if status == "pending" {
        let verb = if source == "proposed" {
            "Proposed"
        } else {
            "Pending"
        };
        println!(
            "{} [{}] {}\n  id {} — approve with `ygg learn approve {}`",
            verb,
            scope,
            short(text, 100),
            learning.learning_id,
            learning.learning_id,
        );
    } else {
        println!("Learned [{}] {}", scope, short(text, 100));
    }
    Ok(())
}

/// `ygg learn pending` — the triage queue (status='pending'), newest first.
pub async fn pending(
    pool: &crate::db::DbPool,
    all_repos: bool,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id)
    };
    let rows = LearningRepo::new(pool).list_pending(repo_id).await?;

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
        println!("No pending learnings.");
        return Ok(());
    }
    for r in &rows {
        let scope = format_scope_label(
            r.repo_id.clone(),
            r.file_glob.as_deref(),
            r.rule_id.as_deref(),
            &r.scope_tags,
        );
        println!(
            "  · {} [{} · {}] {}",
            r.learning_id,
            scope,
            r.source,
            short(&r.text, 100)
        );
    }
    println!("\napprove: `ygg learn approve <id>` · reject: `ygg learn reject <id>`");
    Ok(())
}

/// `ygg learn approve <id>` — promote a pending learning to active.
pub async fn approve(
    pool: &crate::db::DbPool,
    learning_id: String,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let approver: Option<String> =
        sqlx::query_scalar("SELECT agent_id FROM agents WHERE agent_name = $1")
            .bind(agent_name)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    match LearningRepo::new(pool)
        .approve(learning_id.clone(), approver)
        .await?
    {
        Some(l) => {
            let scope = format_scope_label(
                l.repo_id,
                l.file_glob.as_deref(),
                l.rule_id.as_deref(),
                &l.scope_tags,
            );
            println!(
                "approved {learning_id} → active [{scope}] {}",
                short(&l.text, 80)
            );
        }
        None => {
            anyhow::bail!(
                "no pending learning with id {learning_id} (already active, rejected, or unknown)"
            );
        }
    }
    Ok(())
}

/// `ygg learn reject <id>` — drop a pending proposal.
pub async fn reject(
    pool: &crate::db::DbPool,
    learning_id: String,
    reason: Option<&str>,
) -> Result<(), anyhow::Error> {
    if LearningRepo::new(pool).reject(learning_id.clone()).await? {
        match reason {
            Some(r) => println!("rejected {learning_id} — {r}"),
            None => println!("rejected {learning_id}"),
        }
    } else {
        anyhow::bail!(
            "no pending learning with id {learning_id} (active rows are removed with `ygg learn delete`)"
        );
    }
    Ok(())
}

pub async fn list(
    pool: &crate::db::DbPool,
    file_path: Option<&str>,
    rule_id: Option<&str>,
    all_repos: bool,
    json: bool,
) -> Result<(), anyhow::Error> {
    let repo_id = if all_repos {
        None
    } else {
        resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id)
    };
    let rows = LearningRepo::new(pool)
        .list_matching(repo_id, file_path, rule_id, None, None)
        .await?;

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
        println!("No learnings match.");
        return Ok(());
    }
    for r in &rows {
        let scope = format_scope_label(
            r.repo_id.clone(),
            r.file_glob.as_deref(),
            r.rule_id.as_deref(),
            &r.scope_tags,
        );
        let applied = if r.applied_count > 0 {
            let last = r
                .last_applied_at
                .map(|t| format!(", last {}", t.format("%Y-%m-%d")))
                .unwrap_or_default();
            format!(" [×{}{}]", r.applied_count, last)
        } else {
            String::new()
        };
        println!("  · [{}]{} {}", scope, applied, short(&r.text, 100));
    }
    Ok(())
}

pub async fn delete(pool: &crate::db::DbPool, learning_id: String) -> Result<(), anyhow::Error> {
    LearningRepo::new(pool).delete(learning_id.clone()).await?;
    println!("deleted {learning_id}");
    Ok(())
}

/// Surface scoped learnings whose `file_glob` matches any of the given paths
/// (yggdrasil-82). Returns formatted lines suitable for direct stdout / hook
/// injection. Increments each surfaced learning's `applied_count` so the
/// usage telemetry reflects real hits, not just creation.
pub async fn surface_for_files(
    pool: &crate::db::DbPool,
    repo_id: Option<String>,
    files: &[String],
    agent_name: Option<&str>,
    task_kind: Option<&str>,
) -> Result<Vec<String>, anyhow::Error> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let repo = LearningRepo::new(pool);

    for f in files {
        let rows = repo
            .list_matching(repo_id.clone(), Some(f), None, agent_name, task_kind)
            .await?;
        for l in rows {
            if !seen.insert(l.learning_id.clone()) {
                continue;
            }
            out.push(format_learning_line(&l));
            let _ = repo.increment_applied(l.learning_id.clone()).await;
        }
    }

    if files.is_empty() {
        let rows = repo
            .list_matching(repo_id, None, None, agent_name, task_kind)
            .await?;
        for l in rows {
            if l.file_glob.is_some() || l.rule_id.is_some() {
                continue;
            }
            if !seen.insert(l.learning_id.clone()) {
                continue;
            }
            out.push(format_learning_line(&l));
            let _ = repo.increment_applied(l.learning_id.clone()).await;
        }
    }

    Ok(out)
}

/// Surface learnings matching a single file path an agent is about to edit
/// (yggdrasil-180). Resolves the repo from cwd, queries the deterministic
/// `list_matching` predicate, and dedups per session so the same learning is
/// not re-injected on every edit. Returns formatted lines for newly-surfaced
/// learnings and bumps each one's `applied_count` / `last_applied_at`.
///
/// Only file-scoped learnings (`file_glob IS NOT NULL`) fire here — repo-wide
/// and global no-glob learnings are general rules already surfaced at session
/// start (prime) and task claim, so re-injecting them on every edit is noise.
/// Best-effort — any DB or filesystem hiccup yields an empty Vec, never an
/// error to the hook caller.
pub async fn surface_for_edit(
    pool: &crate::db::DbPool,
    file_path: &str,
    agent_name: Option<&str>,
    session_id: &str,
) -> Vec<String> {
    let repo_id = resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id);
    let repo = LearningRepo::new(pool);
    let rows = match repo
        .list_matching(repo_id, Some(file_path), None, agent_name, None)
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    if rows.is_empty() {
        return Vec::new();
    }

    // Per-session dedup via a best-effort flat file under /tmp/ygg.
    // Sanitize session_id before embedding in a path: only [a-zA-Z0-9_-] allowed,
    // everything else becomes '_', preventing path-traversal via crafted session IDs.
    let seen_path = (!session_id.is_empty()).then(|| {
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        std::path::PathBuf::from(format!("/tmp/ygg/learnings-{safe}.seen"))
    });
    let mut seen: std::collections::HashSet<String> = seen_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut fresh = Vec::new();
    for l in &rows {
        // Edit-time injection is for file-scoped rules only.
        if l.file_glob.is_none() {
            continue;
        }
        let id = l.learning_id.to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        out.push(format_learning_line(l));
        fresh.push(id);
        let _ = repo.increment_applied(l.learning_id.clone()).await;
    }

    if let Some(p) = seen_path
        && !fresh.is_empty()
    {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
        {
            for id in &fresh {
                let _ = writeln!(f, "{id}");
            }
        }
    }

    out
}

/// Best-effort file-path extractor for free-text task fields.
pub fn extract_file_mentions(text: &str) -> Vec<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static FILE_PATTERN: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?:[a-zA-Z0-9_./*\-]+/)?(?:[a-zA-Z0-9_*\-]+(?:\.[a-zA-Z0-9_*\-]+)*)\.[a-zA-Z]{2,5}",
        )
        .expect("file-mention regex")
    });
    let mut found: Vec<String> = FILE_PATTERN
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect();
    found.sort();
    found.dedup();
    found
}

fn scope_tag_fragments(scope_tags: &serde_json::Value) -> Vec<String> {
    let mut frags = Vec::new();
    if let Some(obj) = scope_tags.as_object() {
        if let Some(a) = obj.get("agent").and_then(|v| v.as_str()) {
            frags.push(format!("agent={a}"));
        }
        if let Some(k) = obj.get("kind").and_then(|v| v.as_str()) {
            frags.push(format!("kind={k}"));
        }
    }
    frags
}

fn format_scope_label(
    repo_id: Option<String>,
    file_glob: Option<&str>,
    rule_id: Option<&str>,
    scope_tags: &serde_json::Value,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (repo_id, file_glob, rule_id) {
        (None, _, _) => parts.push("global".to_string()),
        (Some(_), Some(g), Some(r)) => {
            parts.push(g.to_string());
            parts.push(r.to_string());
        }
        (Some(_), Some(g), None) => parts.push(g.to_string()),
        (Some(_), None, Some(r)) => parts.push(format!("rule={r}")),
        (Some(_), None, None) => parts.push("repo".to_string()),
    }
    parts.extend(scope_tag_fragments(scope_tags));
    parts.join(" · ")
}

fn format_learning_line(l: &crate::models::learning::Learning) -> String {
    let scope = format_scope_label(
        l.repo_id.clone(),
        l.file_glob.as_deref(),
        l.rule_id.as_deref(),
        &l.scope_tags,
    );
    format!("[ygg learning · {scope}] {}", short(&l.text, 200))
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn extracts_common_paths() {
        let t = "Edit src/main.rs and update Cargo.toml; also touch docs/orchestration.md.";
        let found = extract_file_mentions(t);
        assert!(found.contains(&"src/main.rs".to_string()));
        assert!(found.contains(&"Cargo.toml".to_string()));
        assert!(found.contains(&"docs/orchestration.md".to_string()));
    }

    #[test]
    fn rejects_version_strings() {
        let t = "Bump serde to 1.0.220 and tokio to 1.40.";
        let found = extract_file_mentions(t);
        assert!(found.iter().all(|f| !f.starts_with("1.")));
    }

    #[test]
    fn handles_globs() {
        let t = "Run `cargo check`; touches migrations/*.sql and terraform/*.tf";
        let found = extract_file_mentions(t);
        assert!(found.iter().any(|f| f.contains("*.sql")));
        assert!(found.iter().any(|f| f.contains("*.tf")));
    }

    #[test]
    fn handles_multi_dot_basenames() {
        let t = "Update vite.config.ts and tsconfig.build.json before merging.";
        let found = extract_file_mentions(t);
        assert!(
            found.iter().any(|f| f == "vite.config.ts"),
            "should preserve the full multi-dot basename, got {found:?}"
        );
        assert!(
            found.iter().any(|f| f == "tsconfig.build.json"),
            "should preserve the full multi-dot basename, got {found:?}"
        );
    }

    #[test]
    fn parse_scope_tags_global() {
        let tags = parse_scope_tags(&["global".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({}));
    }

    #[test]
    fn parse_scope_tags_agent() {
        let tags = parse_scope_tags(&["agent=linter".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({"agent": "linter"}));
    }

    #[test]
    fn parse_scope_tags_kind() {
        let tags = parse_scope_tags(&["kind=bug".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({"kind": "bug"}));
    }

    #[test]
    fn parse_scope_tags_combined() {
        let tags = parse_scope_tags(&["agent=ci".to_string(), "kind=feature".to_string()]).unwrap();
        assert_eq!(tags, serde_json::json!({"agent": "ci", "kind": "feature"}));
    }

    #[test]
    fn parse_scope_tags_unknown_key_errors() {
        let err = parse_scope_tags(&["agnt=foo".to_string()]);
        assert!(err.is_err(), "unknown key should fail");
    }

    #[test]
    fn parse_scope_tags_no_equals_errors() {
        let err = parse_scope_tags(&["bogus".to_string()]);
        assert!(err.is_err(), "token without '=' should fail");
    }

    #[test]
    fn parse_scope_tags_empty_value_errors() {
        let err = parse_scope_tags(&["agent=".to_string()]);
        assert!(err.is_err(), "empty value should fail");
    }

    #[test]
    fn scope_label_with_tags() {
        let label = format_scope_label(
            Some(Uuid::nil().to_string()),
            Some("src/*.rs"),
            None,
            &serde_json::json!({"agent": "foo", "kind": "bug"}),
        );
        assert_eq!(label, "src/*.rs · agent=foo · kind=bug");
    }
}
