use crate::config::AppConfig;
use crate::models::agent::{AgentRepo, AgentState};
use crate::tmux::TmuxManager;

/// Snapshot of agent state at the moment of interruption.
pub struct InterruptSnapshot {
    pub agent_id: String,
    pub prior_state: AgentState,
    pub context_tokens: i32,
}

/// Take over an agent's session.
/// 1. Transition to HumanOverride
/// 2. Focus the tmux window
pub async fn take_over(
    pool: &crate::db::DbPool,
    _config: &AppConfig,
    agent_name: &str,
) -> Result<InterruptSnapshot, anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());

    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    let snapshot = InterruptSnapshot {
        agent_id: agent.agent_id.clone(),
        prior_state: agent.current_state.clone(),
        context_tokens: agent.context_tokens,
    };

    // Transition to HumanOverride
    agent_repo
        .transition(
            &agent.agent_id,
            agent.current_state.clone(),
            AgentState::HumanOverride,
        )
        .await?;

    // Focus the tmux window
    TmuxManager::select_window(agent_name).await.ok();

    tracing::info!(agent = agent_name, "human override initiated");

    Ok(snapshot)
}

/// Hand back control to the agent: transition back to Idle.
pub async fn hand_back(pool: &crate::db::DbPool, agent_name: &str) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());

    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    if agent.current_state != AgentState::HumanOverride {
        anyhow::bail!(
            "agent '{}' is in state '{}', not human_override",
            agent_name,
            agent.current_state
        );
    }

    // Transition back to idle
    agent_repo
        .transition(&agent.agent_id, AgentState::HumanOverride, AgentState::Idle)
        .await?;

    tracing::info!(agent = agent_name, "control handed back");

    Ok(())
}
