//! Hook-driven agent state updates.
//!
//! Called from PreToolUse to record what the agent is doing right now so the
//! dashboard doesn't forever read "idle". Unconditional overwrite: hooks can't
//! guess the current state.

use crate::models::agent::{AgentRepo, AgentState};
use crate::models::session::{SessionRepo, resolve_current_session};
use tracing::{debug, warn};

pub async fn set_tool(
    pool: &crate::db::DbPool,
    agent_name: &str,
    tool: &str,
) -> Result<(), anyhow::Error> {
    let repo = AgentRepo::new(pool, crate::db::user_id());
    let persona = std::env::var("YGG_AGENT_PERSONA")
        .ok()
        .filter(|s| !s.is_empty());
    // Auto-register on first tool-touch so a hook-driven persona row exists
    // even when the user never ran `ygg prime` explicitly for that persona.
    let agent = match repo
        .get_by_name_persona(agent_name, persona.as_deref())
        .await?
    {
        Some(a) => a,
        None => {
            debug!("agent set-tool: minting new row for ({agent_name}, {persona:?})");
            repo.register_with_persona(agent_name, persona.as_deref())
                .await?
        }
    };

    // Per-session state is the source of truth; agents.current_state is kept
    // in sync as a convenience for single-session display.
    let session_id = resolve_current_session(pool, &agent.agent_id, None).await;
    if let Some(sid) = session_id
        && let Err(e) = SessionRepo::new(pool)
            .force_state(&sid, AgentState::WaitingTool, Some(tool))
            .await
    {
        warn!("agent set-tool: session force_state failed: {e}");
    }
    if let Err(e) = repo
        .force_state(&agent.agent_id, AgentState::WaitingTool, Some(tool))
        .await
    {
        warn!("agent set-tool: force_state failed: {e}");
    }
    Ok(())
}
