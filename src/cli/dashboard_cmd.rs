use crate::config::AppConfig;
use crate::db::DbPool;

pub async fn execute(pool: &DbPool, config: &AppConfig) -> Result<(), anyhow::Error> {
    crate::tui::app::run(pool, config).await
}
