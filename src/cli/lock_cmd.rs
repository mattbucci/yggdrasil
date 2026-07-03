use crate::config::AppConfig;
use crate::lock::LockManager;
use crate::models::agent::AgentRepo;

/// Handle `ygg lock acquire <resource>`
pub async fn acquire(
    pool: &crate::db::DbPool,
    config: &AppConfig,
    resource: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs, crate::db::user_id());
    match lock_mgr.acquire(resource, &agent.agent_id).await {
        Ok(lock) => {
            println!(
                "Lock acquired: {} (expires {})",
                lock.resource_key, lock.expires_at
            );
        }
        Err(e) => {
            println!("Failed: {e}");
        }
    }
    Ok(())
}

/// Handle `ygg lock release <resource>`
pub async fn release(
    pool: &crate::db::DbPool,
    config: &AppConfig,
    resource: &str,
    agent_name: &str,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    let agent = agent_repo
        .get_by_name(agent_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found", agent_name))?;

    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs, crate::db::user_id());
    lock_mgr.release(resource, &agent.agent_id).await?;
    println!("Lock released: {resource}");
    Ok(())
}

/// Handle `ygg lock list` (and `--stale`)
pub async fn list(
    pool: &crate::db::DbPool,
    config: &AppConfig,
    stale_only: bool,
    stale_secs: i64,
) -> Result<(), anyhow::Error> {
    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs, crate::db::user_id());
    let mut locks = lock_mgr.list_all().await?;

    if stale_only {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(stale_secs);
        locks.retain(|l| l.acquired_at < cutoff);
    }

    if locks.is_empty() {
        if stale_only {
            println!("No stale locks (held > {stale_secs}s).");
        } else {
            println!("No active locks.");
        }
        return Ok(());
    }

    // Resolve agent names inline so the output is scriptable without
    // round-tripping through UUIDs.
    let agent_repo = AgentRepo::new(pool, crate::db::user_id());
    let now = chrono::Utc::now();
    println!(
        "{:<30} {:<20} {:<10} {:<10}",
        "RESOURCE", "AGENT", "HELD_FOR", "TTL"
    );
    for lock in &locks {
        let agent = agent_repo
            .get(&lock.agent_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.agent_name)
            .unwrap_or_else(|| format!("{}…", &lock.agent_id.to_string()[..8]));
        let held = (now - lock.acquired_at).num_seconds().max(0);
        let ttl = (lock.expires_at - now).num_seconds().max(0);
        println!(
            "{:<30} {:<20} {:<10} {:<10}",
            lock.resource_key,
            agent,
            format!("{held}s"),
            format!("{ttl}s"),
        );
    }
    println!(
        "\n{} lock(s){}.",
        locks.len(),
        if stale_only {
            format!(" held > {stale_secs}s")
        } else {
            String::new()
        }
    );
    Ok(())
}
