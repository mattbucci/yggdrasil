//! `ygg handoff` — checkpoint the current session so a fresh one (after
//! `/clear`) resumes without re-explaining. `save` stores the note (replacing
//! any prior one for this repo+agent), `show` prints it, `clear` drops it. The
//! saved note auto-surfaces at the top of `ygg prime` on the next SessionStart.

use crate::cli::task_cmd::resolve_cwd_repo;
use crate::models::agent::AgentRepo;
use crate::models::handoff::HandoffRepo;
use uuid::Uuid;

/// Resolve the agent_id the same way `ygg prime` does — `register_with_persona`
/// keyed by (name, $YGG_AGENT_PERSONA) — so a handoff saved here is fetched
/// under the identical agent on the next prime. A raw name lookup is ambiguous
/// when the same name exists under multiple personas.
async fn resolve_agent_id(pool: &sqlx::PgPool, agent_name: &str) -> Result<Uuid, anyhow::Error> {
    let persona = std::env::var("YGG_AGENT_PERSONA")
        .ok()
        .filter(|s| !s.is_empty());
    // Propagate, don't swallow: a DB failure here must error rather than fall
    // back to agent_id=NULL, which would silently target the wrong (repo, NULL)
    // handoff scope. `register_with_persona` always yields the agent on success.
    let agent = AgentRepo::new(pool, crate::db::user_id())
        .register_with_persona(agent_name, persona.as_deref())
        .await?;
    Ok(agent.agent_id)
}

pub async fn save(
    pool: &sqlx::PgPool,
    text: &str,
    agent_name: &str,
    json: bool,
) -> Result<(), anyhow::Error> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("nothing to hand off — pass the note as an argument or on stdin");
    }
    // A handoff without a detected repo is still useful (keyed to the agent).
    let repo_id = resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id);
    let agent_id = resolve_agent_id(pool, agent_name).await?;

    let handoff = HandoffRepo::new(pool)
        .save(repo_id, Some(agent_id), text)
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&handoff)?);
        return Ok(());
    }
    let scope = if repo_id.is_some() {
        "this repo"
    } else {
        "global"
    };
    println!(
        "Handoff saved ({} chars, {scope}). It surfaces at the top of the next `ygg prime`.",
        text.chars().count(),
    );
    Ok(())
}

pub async fn show(pool: &sqlx::PgPool, agent_name: &str, json: bool) -> Result<(), anyhow::Error> {
    let repo_id = resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id);
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let handoff = HandoffRepo::new(pool)
        .latest(repo_id, Some(agent_id))
        .await?;

    match handoff {
        None => {
            if json {
                println!("null");
            } else {
                println!("No handoff. Write one with `ygg handoff save \"...\"`.");
            }
        }
        Some(h) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&h)?);
            } else {
                println!("{}", h.text);
            }
        }
    }
    Ok(())
}

pub async fn clear(pool: &sqlx::PgPool, agent_name: &str) -> Result<(), anyhow::Error> {
    let repo_id = resolve_cwd_repo(pool).await.ok().map(|r| r.repo_id);
    let agent_id = resolve_agent_id(pool, agent_name).await?;
    let cleared = HandoffRepo::new(pool)
        .clear(repo_id, Some(agent_id))
        .await?;
    if cleared {
        println!("Handoff cleared.");
    } else {
        println!("No handoff to clear.");
    }
    Ok(())
}
