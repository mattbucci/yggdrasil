use crate::config::AppConfig;
use crate::db::DbPool;
use crate::watcher::Watcher;

pub async fn execute(pool: &DbPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    let watcher = Watcher::new(pool.clone(), config.clone());
    watcher.run().await
}
