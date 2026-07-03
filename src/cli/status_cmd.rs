use crate::lock::LockManager;
use crate::models::agent::AgentRepo;
use crate::models::worker::WorkerRepo;

/// Handle `ygg status [--agent <name>] [--all-users]`
pub async fn execute(
    pool: &crate::db::DbPool,
    agent_name: Option<&str>,
    all_users: bool,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());

    if let Some(name) = agent_name {
        let agent = agent_repo
            .get_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", name))?;

        println!("Agent: {}", agent.agent_name);
        println!("  ID:       {}", agent.agent_id);
        println!("  State:    {}", agent.current_state);
        println!("  Pressure: {} tokens", agent.context_tokens);
        println!("  Updated:  {}", agent.updated_at);

        let lock_mgr = LockManager::new(pool, 300, crate::db::user_id());
        let locks = lock_mgr.list_agent_locks(&agent.agent_id).await?;
        if locks.is_empty() {
            println!("  Locks:    none");
        } else {
            for lock in &locks {
                println!(
                    "  Lock:     {} (expires {})",
                    lock.resource_key,
                    lock.expires_at.format("%H:%M:%S")
                );
            }
        }
    } else {
        let agents = if all_users {
            agent_repo.list_all_users().await?
        } else {
            agent_repo.list().await?
        };

        if agents.is_empty() {
            println!("No agents registered.");
            return Ok(());
        }

        println!(
            "{:<20} {:<15} {:<12} {:<20}",
            "NAME", "STATE", "PRESSURE", "UPDATED"
        );
        for agent in &agents {
            println!(
                "{:<20} {:<15} {:<12} {:<20}",
                agent.agent_name,
                agent.current_state.to_string(),
                format!("{} tok", agent.context_tokens),
                agent.updated_at.format("%H:%M:%S"),
            );
        }
        println!("\n{} agent(s).", agents.len());

        // Live workers with intent
        let workers = WorkerRepo::new(pool).list_live().await.unwrap_or_default();
        if !workers.is_empty() {
            println!(
                "\n{:<16} {:<14} {:<12} {:<30}",
                "WORKER", "STATE", "DELIVERY", "INTENT"
            );
            for w in &workers {
                let delivery = if w.branch_merged {
                    "merged"
                } else if w.pr_url.is_some() {
                    "pr-open"
                } else if w.branch_pushed {
                    "pushed"
                } else {
                    "local"
                };
                println!(
                    "{:<16} {:<14} {:<12} {:<30}",
                    w.tmux_window,
                    format!("{:?}", w.state).to_lowercase(),
                    delivery,
                    w.intent.as_deref().unwrap_or("—"),
                );
            }
            println!("\n{} worker(s).", workers.len());
        }
    }

    Ok(())
}
