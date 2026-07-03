use crate::config::AppConfig;
use crate::db::DbPool;

pub async fn execute_take_over(
    pool: &DbPool,
    config: &AppConfig,
    agent: &str,
) -> Result<(), anyhow::Error> {
    let snapshot = crate::interrupt::take_over(pool, config, agent).await?;
    println!(
        "Took over agent '{}' (was: {})",
        agent, snapshot.prior_state
    );
    println!("  Pressure: {} tokens", snapshot.context_tokens);
    println!("\nType in the agent's tmux pane. When done:");
    println!("  ygg interrupt hand-back {}", agent);
    Ok(())
}

pub async fn execute_hand_back(
    pool: &DbPool,
    _config: &AppConfig,
    agent: &str,
) -> Result<(), anyhow::Error> {
    crate::interrupt::hand_back(pool, agent).await?;
    println!("Handed back control to agent '{}'", agent);
    Ok(())
}
