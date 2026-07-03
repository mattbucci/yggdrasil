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
    /// Base URL of the hermes-gateway router, e.g.
    /// `http://hermes-gateway.ph.ca:8642`. None disables all hermes features.
    pub hermes_gateway_url: Option<String>,
    /// Bearer token for the OpenAI-compatible + task API (`/v1/*`).
    pub hermes_gateway_token: Option<String>,
    /// Separate bearer token for the ops dashboard API (`/dashboard/api/*`).
    pub hermes_dashboard_token: Option<String>,
    /// SSE URL of the mnemosyne MCP agent-memory service, e.g.
    /// `http://hermes-gateway.ph.ca:8077/sse`. None disables `ygg memory`.
    pub mnemosyne_mcp_url: Option<String>,
    /// Bearer token for the mnemosyne MCP service (loopback may not require it).
    pub mnemosyne_mcp_token: Option<String>,
    /// Default memory bank (partition). Falls back to `"default"` per call.
    pub mnemosyne_mcp_bank: Option<String>,
    /// Optional host-shared directory readable by both ygg and the mnemosyne
    /// service. When set it enables server-side `mnemosyne_export`/`import`
    /// sync; otherwise sync falls back to recall-based enumeration.
    pub mnemosyne_export_dir: Option<String>,
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
            hermes_gateway_url: non_empty_env("HERMES_GATEWAY_URL"),
            hermes_gateway_token: non_empty_env("HERMES_GATEWAY_TOKEN"),
            hermes_dashboard_token: non_empty_env("HERMES_DASHBOARD_TOKEN"),
            mnemosyne_mcp_url: non_empty_env("MNEMOSYNE_MCP_URL"),
            mnemosyne_mcp_token: non_empty_env("MNEMOSYNE_MCP_TOKEN"),
            mnemosyne_mcp_bank: non_empty_env("MNEMOSYNE_MCP_BANK"),
            mnemosyne_export_dir: non_empty_env("MNEMOSYNE_EXPORT_DIR"),
        })
    }
}

/// Read an environment variable, treating unset *and* empty as absent. Used
/// for optional configuration where an empty string should not be mistaken
/// for a real value.
fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}
