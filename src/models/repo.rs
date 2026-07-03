use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Repo {
    pub repo_id: String,
    pub canonical_url: Option<String>,
    pub name: String,
    pub task_prefix: String,
    pub local_paths: sqlx::types::Json<Vec<String>>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct RepoRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> RepoRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    /// Register or update a repo identified by its canonical_url (preferred)
    /// or falling back to a local-path hash. Returns the existing row if one
    /// matches and appends `local_path` to the known paths list.
    pub async fn register(
        &self,
        canonical_url: Option<&str>,
        name: &str,
        task_prefix: &str,
        local_path: Option<&str>,
    ) -> Result<Repo, sqlx::Error> {
        // Try to find by URL first
        if let Some(url) = canonical_url
            && let Some(existing) = self.get_by_url(url).await?
        {
            if let Some(p) = local_path {
                self.append_path(&existing.repo_id, p).await?;
                return self
                    .get(&existing.repo_id)
                    .await
                    .map(|r| r.expect("just updated"));
            }
            return Ok(existing);
        }

        // Else try by prefix (some non-git repos land here)
        if let Some(existing) = self.get_by_prefix(task_prefix).await? {
            if let Some(p) = local_path {
                self.append_path(&existing.repo_id, p).await?;
                return self
                    .get(&existing.repo_id)
                    .await
                    .map(|r| r.expect("just updated"));
            }
            return Ok(existing);
        }

        let paths: Vec<String> = local_path.into_iter().map(String::from).collect();

        sqlx::query_as::<_, Repo>(
            r#"
            INSERT INTO repos (canonical_url, name, task_prefix, local_paths)
            VALUES ($1, $2, $3, $4)
            RETURNING repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
            "#,
        )
        .bind(canonical_url)
        .bind(name)
        .bind(task_prefix)
        .bind(sqlx::types::Json(&paths))
        .fetch_one(self.pool)
        .await
    }

    pub async fn get(&self, repo_id: &str) -> Result<Option<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos WHERE repo_id = $1"#,
        )
        .bind(repo_id)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_url(&self, url: &str) -> Result<Option<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos WHERE canonical_url = $1"#,
        )
        .bind(url)
        .fetch_optional(self.pool)
        .await
    }

    pub async fn get_by_prefix(&self, prefix: &str) -> Result<Option<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos WHERE task_prefix = $1"#,
        )
        .bind(prefix)
        .fetch_optional(self.pool)
        .await
    }

    async fn append_path(&self, repo_id: &str, path: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE repos
               SET local_paths = (SELECT json_group_array(value) FROM
                                  (SELECT value FROM json_each(local_paths)
                                   UNION SELECT $2)),
                   updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now')
               WHERE repo_id = $1"#,
        )
        .bind(repo_id)
        .bind(path)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Repo>, sqlx::Error> {
        sqlx::query_as::<_, Repo>(
            r#"SELECT repo_id, canonical_url, name, task_prefix, local_paths,
                      metadata, created_at, updated_at
               FROM repos ORDER BY name"#,
        )
        .fetch_all(self.pool)
        .await
    }
}

/// Detect the repository for the given directory via `git`.
/// Returns (canonical_url, toplevel_path, basename) if inside a git work tree.
pub fn detect_git_repo(start_dir: &std::path::Path) -> Option<(Option<String>, String, String)> {
    let toplevel = std::process::Command::new("git")
        .args(["-C"])
        .arg(start_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !toplevel.status.success() {
        return None;
    }
    let top = String::from_utf8_lossy(&toplevel.stdout).trim().to_string();
    if top.is_empty() {
        return None;
    }

    let url = std::process::Command::new("git")
        .args(["-C", &top, "config", "--get", "remote.origin.url"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        });

    let basename = std::path::Path::new(&top)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());

    Some((url, top, basename))
}

/// Slugify a repo name into a safe task_prefix: lowercase, alnum + dash only.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "repo".to_string()
    } else {
        out
    }
}
