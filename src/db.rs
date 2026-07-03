use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// The application database pool. Yggdrasil runs on a single-host embedded
/// SQLite database (WAL mode); this alias is the only DB handle type the rest
/// of the codebase should name.
pub type DbPool = SqlitePool;
/// The sqlx database driver in use.
pub type Db = sqlx::Sqlite;
/// A transaction on the application database.
pub type DbTx<'c> = sqlx::Transaction<'c, Db>;

const DEFAULT_MAX_CONNECTIONS: u32 = 32;
const BUSY_TIMEOUT_MS: u64 = 5000;

/// SQL expression producing the current UTC instant in the exact canonical
/// format sqlx's chrono integration writes `DateTime<Utc>` binds in
/// (RFC 3339, `T` separator, millisecond precision, `+00:00` suffix).
/// Using one canonical text format keeps lexicographic comparisons between
/// column values and SQL-side "now" correct.
pub const SQL_NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00','now')";

static USER_ID: OnceLock<String> = OnceLock::new();

/// Return the cached user identity. Resolved once per process.
pub fn user_id() -> &'static str {
    USER_ID.get_or_init(resolve_user)
}

/// Resolve the current user identity.
/// Priority: YGG_USER env → whoami output → "default".
pub fn resolve_user() -> String {
    if let Ok(u) = std::env::var("YGG_USER")
        && !u.is_empty()
    {
        return u;
    }
    std::process::Command::new("whoami")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Default on-disk location for the database:
/// `$XDG_DATA_HOME/ygg/ygg.db`, falling back to `~/.local/share/ygg/ygg.db`.
pub fn default_db_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join("ygg").join("ygg.db")
}

/// Resolve the database "URL" (a `sqlite://` URL or plain path) from the
/// environment. Precedence: `YGG_DB_PATH` (plain path) > `DATABASE_URL`
/// (sqlite:// URL, honored for compatibility) > default XDG data path.
pub fn resolve_database_url() -> String {
    if let Ok(p) = std::env::var("YGG_DB_PATH")
        && !p.is_empty()
    {
        return p;
    }
    if let Ok(u) = std::env::var("DATABASE_URL")
        && !u.is_empty()
    {
        return u;
    }
    default_db_path().display().to_string()
}

fn connect_options(database_url: &str) -> Result<SqliteConnectOptions, sqlx::Error> {
    let opts = if database_url.starts_with("sqlite:") {
        SqliteConnectOptions::from_str(database_url)?
    } else {
        SqliteConnectOptions::new().filename(database_url)
    };
    Ok(opts
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .foreign_keys(true))
}

/// Open (creating if necessary) the SQLite database at `database_url` —
/// either a `sqlite://` URL or a plain filesystem path. The parent directory
/// is created automatically.
pub async fn create_pool(database_url: &str) -> Result<DbPool, sqlx::Error> {
    let opts = connect_options(database_url)?;
    // Auto-create the parent directory (e.g. ~/.local/share/ygg) so first
    // run works with zero setup. `:memory:` has no parent to create.
    if let Some(parent) = opts.get_filename().parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(sqlx::Error::Io)?;
    }
    let max_connections: u32 = std::env::var("YGG_DB_POOL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_CONNECTIONS);
    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(opts)
        .await
}

pub async fn run_migrations(pool: &DbPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// Return descriptions of migrations that are compiled into the binary but
/// not yet applied to the database. Returns an empty vec when fully up to date.
/// Gracefully handles the case where `_sqlx_migrations` doesn't exist yet
/// (fresh DB) by treating all migrations as pending.
pub async fn pending_migrations(pool: &DbPool) -> Result<Vec<String>, anyhow::Error> {
    let migrator = sqlx::migrate!("./migrations");

    let applied: HashSet<i64> = match sqlx::query_scalar::<_, i64>(
        "SELECT version FROM _sqlx_migrations WHERE success = 1",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows.into_iter().collect(),
        // "no such table" — fresh DB, no migrations applied yet.
        Err(sqlx::Error::Database(ref e)) if e.message().contains("no such table") => {
            HashSet::new()
        }
        Err(e) => return Err(e.into()),
    };

    let pending: Vec<String> = migrator
        .migrations
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .map(|m| m.description.to_string())
        .collect();
    Ok(pending)
}
