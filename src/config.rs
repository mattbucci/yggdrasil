use std::env;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// SQLite database location: either a `sqlite://` URL or a plain path.
    /// Resolved from `YGG_DB_PATH` > `DATABASE_URL` > the XDG default
    /// (`$XDG_DATA_HOME/ygg/ygg.db`, fallback `~/.local/share/ygg/ygg.db`).
    /// Never required in the environment.
    pub database_url: String,
    pub context_limit_tokens: usize,
    pub context_hard_cap_tokens: usize,
    pub lock_ttl_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub watcher_interval_secs: u64,
    pub rtk_binary_path: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, crate::YggError> {
        // Only load from ~/.config/ygg/.env — never from local .env
        // Local .env files cause stale config issues
        if let Ok(home) = env::var("HOME") {
            let config_env = std::path::Path::new(&home).join(".config/ygg/.env");
            if config_env.exists() {
                dotenvy::from_path(&config_env).ok();
            }
        }

        Ok(Self {
            database_url: crate::db::resolve_database_url(),
            context_limit_tokens: env::var("CONTEXT_LIMIT_TOKENS")
                .unwrap_or_else(|_| "250000".into())
                .parse()
                .unwrap_or(250_000),
            context_hard_cap_tokens: env::var("CONTEXT_HARD_CAP_TOKENS")
                .unwrap_or_else(|_| "300000".into())
                .parse()
                .unwrap_or(300_000),
            lock_ttl_secs: env::var("LOCK_TTL_SECS")
                .unwrap_or_else(|_| "300".into())
                .parse()
                .unwrap_or(300),
            heartbeat_interval_secs: env::var("HEARTBEAT_INTERVAL_SECS")
                .unwrap_or_else(|_| "60".into())
                .parse()
                .unwrap_or(60),
            watcher_interval_secs: env::var("WATCHER_INTERVAL_SECS")
                .unwrap_or_else(|_| "30".into())
                .parse()
                .unwrap_or(30),
            rtk_binary_path: env::var("RTK_BINARY_PATH").unwrap_or_else(|_| "rtk".into()),
        })
    }
}
