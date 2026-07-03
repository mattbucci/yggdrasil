use crate::models::agent::AgentRepo;

/// Detect and recover orphaned agents stuck in active states after a crash.
pub async fn execute(
    pool: &crate::db::DbPool,
    stale_secs: Option<u64>,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    let threshold = stale_secs.unwrap_or(300) as i64; // default 5 min

    let orphaned = agent_repo.find_orphaned(threshold).await?;

    if orphaned.is_empty() {
        println!("No orphaned agents found.");
        return Ok(());
    }

    println!("Found {} orphaned agent(s):\n", orphaned.len());
    for agent in &orphaned {
        println!("  {} ({})", agent.agent_name, agent.current_state);
        println!("    last update: {}", agent.updated_at);
    }

    println!();
    use std::io::{self, BufRead, Write};
    println!("Reset all to idle? [Y/n]");
    print!("> ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    let a = s.trim().to_lowercase();

    if a.is_empty() || a == "y" || a == "yes" {
        for agent in &orphaned {
            agent_repo.reset_to_idle(&agent.agent_id).await?;
            println!("  {} → idle", agent.agent_name);
        }
        println!("\nRecovery complete. Agents can be resumed with `ygg spawn`.");
    } else {
        println!("Skipped.");
    }

    Ok(())
}
