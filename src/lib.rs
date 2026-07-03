pub mod analytics;
pub mod bench;
pub mod blob;
pub mod bus;
pub mod cli;
pub mod config;
pub mod db;
pub mod executor;
pub mod interrupt;
pub mod lock;
pub mod models;
pub mod notify;
pub mod redaction;
pub mod scheduler;
pub mod stats;
pub mod status;
pub mod tmux;
pub mod tui;
pub mod watcher;
pub mod worktree;

use crate::db::DbPool;

/// Shared application state threaded through all subsystems.
pub struct AppState {
    pub pool: DbPool,
    pub agent_id: String,
    pub config: config::AppConfig,
}

/// Unified error type for Ygg.
#[derive(Debug, thiserror::Error)]
pub enum YggError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),

    #[error("executor failed (exit {exit_code}): {stderr}")]
    Executor { exit_code: i32, stderr: String },

    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },

    #[error("lock conflict: {0}")]
    Lock(#[from] lock::LockError),

    #[error("config: {0}")]
    Config(String),

    #[error("tmux: {0}")]
    Tmux(String),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}
