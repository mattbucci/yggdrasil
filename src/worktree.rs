//! Worktree helper — provisions git worktrees per task so the click-to-do
//! supervisor can spawn CC sessions in isolated working copies.
//!
//! Layout:
//!   root = `$XDG_STATE_HOME/ygg/worktrees` (falls back to
//!          `$HOME/.local/state/ygg/worktrees`)
//!   path = <root>/<repo-prefix>-<seq>
//!   branch = ygg/<repo-prefix>-<seq>
//!
//! Idempotent: `ensure` returns the existing worktree if the branch is
//! already checked out at our path. `teardown` prunes the worktree and
//! optionally deletes the branch per policy.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::models::repo::{Repo, RepoRepo};
use crate::models::task::{Task, TaskRepo};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TeardownPolicy {
    /// Leave the worktree + branch alone. Default for inspection after close.
    Keep,
    /// Prune the worktree but leave the branch for potential PR.
    Archive,
    /// Prune the worktree and delete the local branch.
    Delete,
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub task_ref: String, // "yggdrasil-42"
    pub branch: String,   // "ygg/yggdrasil-42"
    pub path: PathBuf,
    pub base_path: PathBuf, // repo root we worktreed from
}

/// Create (or return existing) worktree for the given task. The caller is
/// responsible for having at least one local_path recorded on the repo.
pub async fn ensure(pool: &crate::db::DbPool, task_id: String) -> Result<Worktree, anyhow::Error> {
    let (task, repo) = resolve(pool, task_id).await?;
    let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
    let branch = format!("ygg/{task_ref}");
    let root = worktree_root()?;
    let path = root.join(&task_ref);

    let base = primary_local_path(&repo).ok_or_else(|| {
        anyhow::anyhow!(
            "repo '{}' has no local_paths recorded — can't create worktree",
            repo.name
        )
    })?;

    // Fast path: worktree already exists at our path (idempotent).
    if path.exists() && is_worktree_of(&base, &path) {
        return Ok(Worktree {
            task_ref,
            branch,
            path,
            base_path: base.clone(),
        });
    }

    std::fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("create worktree root {}: {e}", root.display()))?;

    // Pick a base commit: current HEAD of the source repo. Resolve once so
    // we don't race with the source branch moving between detection and
    // `git worktree add`.
    let base_commit = git_stdout(&base, &["rev-parse", "HEAD"])?;

    // If the branch already exists (leftover from a prior run), just check
    // it out; otherwise create it.
    let branch_exists = git(
        &base,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    let args: Vec<String> = if branch_exists {
        vec![
            "worktree".into(),
            "add".into(),
            path.to_string_lossy().into(),
            branch.clone(),
        ]
    } else {
        vec![
            "worktree".into(),
            "add".into(),
            "-b".into(),
            branch.clone(),
            path.to_string_lossy().into(),
            base_commit,
        ]
    };
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    git(&base, &args_ref)?;

    Ok(Worktree {
        task_ref,
        branch,
        path,
        base_path: base,
    })
}

/// Remove the worktree per policy. `Keep` is a no-op. `Delete` also deletes
/// the local branch; `Archive` leaves the branch. Never force-removes if
/// the worktree has uncommitted changes unless `force = true`.
pub async fn teardown(
    pool: &crate::db::DbPool,
    task_id: String,
    policy: TeardownPolicy,
    force: bool,
) -> Result<(), anyhow::Error> {
    if policy == TeardownPolicy::Keep {
        return Ok(());
    }

    let (task, repo) = resolve(pool, task_id).await?;
    let task_ref = format!("{}-{}", repo.task_prefix, task.seq);
    let branch = format!("ygg/{task_ref}");
    let path = worktree_root()?.join(&task_ref);
    let base = primary_local_path(&repo)
        .ok_or_else(|| anyhow::anyhow!("no local_paths for repo {}", repo.name))?;

    if !path.exists() {
        // Already gone — still try to prune stale registrations + branch.
        let _ = git(&base, &["worktree", "prune"]);
        if policy == TeardownPolicy::Delete {
            let _ = git(&base, &["branch", "-D", &branch]);
        }
        return Ok(());
    }

    let mut args: Vec<&str> = vec!["worktree", "remove"];
    let path_str = path.to_string_lossy().into_owned();
    if force {
        args.push("--force");
    }
    args.push(&path_str);
    git(&base, &args)?;

    if policy == TeardownPolicy::Delete {
        // -D (force) because the branch may have work we already decided to drop.
        let _ = git(&base, &["branch", "-D", &branch]);
    }
    Ok(())
}

pub fn worktree_root() -> Result<PathBuf, anyhow::Error> {
    if let Ok(x) = std::env::var("XDG_STATE_HOME")
        && !x.is_empty()
    {
        return Ok(PathBuf::from(x).join("ygg/worktrees"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("HOME not set — cannot locate worktree root"))?;
    Ok(PathBuf::from(home).join(".local/state/ygg/worktrees"))
}

/// Lookup the task + its repo in one shot.
async fn resolve(pool: &crate::db::DbPool, task_id: String) -> Result<(Task, Repo), anyhow::Error> {
    let task = TaskRepo::new(pool)
        .get(&task_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
    let repo = RepoRepo::new(pool)
        .get(&task.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo {} not found", task.repo_id))?;
    Ok((task, repo))
}

/// Pick the first local_path that's an actual git working tree. Repos
/// register multiple candidate paths over time; some may be stale.
fn primary_local_path(repo: &Repo) -> Option<PathBuf> {
    for p in repo.local_paths.iter() {
        let path = PathBuf::from(p);
        if path.join(".git").exists() || is_git_dir(&path) {
            return Some(path);
        }
    }
    // Fall back to the first path; may fail later with a clearer git error.
    repo.local_paths.first().map(PathBuf::from)
}

fn is_git_dir(p: &Path) -> bool {
    // Accept both working-tree repos (`.git` dir) and bare repos.
    p.join("HEAD").exists() && p.join("refs").exists()
}

/// Ask the source repo whether `path` is already one of its worktrees. Used
/// for idempotence — reuse rather than re-add.
fn is_worktree_of(base: &Path, path: &Path) -> bool {
    let Ok(list) = git_stdout(base, &["worktree", "list", "--porcelain"]) else {
        return false;
    };
    let needle = path.to_string_lossy().to_string();
    list.lines()
        .any(|l| l.trim_start_matches("worktree ") == needle)
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), anyhow::Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String, anyhow::Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pre-recovery checkpoint (yggdrasil-115). Snapshot every change in `worktree`
/// (tracked + untracked, respecting .gitignore) into a commit on the
/// `refs/ygg/recovery/<run_id>` ref WITHOUT touching the working branch, HEAD,
/// or the real index — so a retry can recover the work-in-progress before the
/// watcher tears the worktree down. Returns the checkpoint commit SHA.
///
/// Uses a throwaway `GIT_INDEX_FILE` so `git add -A` stages the full working
/// tree without disturbing the agent's staged state; `commit-tree -p HEAD`
/// parents it on the current commit and `update-ref` parks it on the recovery
/// ref, which lives in the shared git dir and survives `git worktree remove`.
pub fn checkpoint(worktree: &Path, run_id: String) -> Option<String> {
    let index = std::env::temp_dir().join(format!("ygg-recovery-{run_id}.index"));
    let _ = std::fs::remove_file(&index);
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    run(&["add", "-A"])?;
    let tree = run(&["write-tree"])?;
    let msg = format!("ygg: pre-recovery checkpoint {run_id}");
    let head = run(&["rev-parse", "HEAD"]).filter(|h| !h.is_empty());
    let mut commit_args = vec!["commit-tree", tree.as_str()];
    if let Some(h) = head.as_deref() {
        commit_args.push("-p");
        commit_args.push(h);
    }
    commit_args.push("-m");
    commit_args.push(msg.as_str());
    let commit = run(&commit_args)?;

    let ref_name = format!("refs/ygg/recovery/{run_id}");
    run(&["update-ref", &ref_name, &commit])?;
    let _ = std::fs::remove_file(&index);
    Some(commit)
}

pub fn parse_policy(s: &str) -> Result<TeardownPolicy, anyhow::Error> {
    match s {
        "keep" => Ok(TeardownPolicy::Keep),
        "archive" => Ok(TeardownPolicy::Archive),
        "delete" => Ok(TeardownPolicy::Delete),
        other => anyhow::bail!("unknown teardown policy '{other}' — try keep/archive/delete"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn policy_parse() {
        assert_eq!(parse_policy("keep").unwrap(), TeardownPolicy::Keep);
        assert_eq!(parse_policy("archive").unwrap(), TeardownPolicy::Archive);
        assert_eq!(parse_policy("delete").unwrap(), TeardownPolicy::Delete);
        assert!(parse_policy("nope").is_err());
    }

    #[test]
    fn root_env_override() {
        // Can't rely on std::env in parallel tests on 2024 edition; use
        // a pure-function path instead: HOME fallback.
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            let r = worktree_root().unwrap();
            assert!(r.ends_with("ygg/worktrees"));
        }
    }

    // Run git in `dir`, asserting success, returning trimmed stdout.
    fn g(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    // Integration: simulate a reap mid-run. The agent left uncommitted
    // (modified + untracked) work in its worktree; checkpoint() must snapshot
    // it onto refs/ygg/recovery/<run_id> without disturbing HEAD or the index,
    // so the retry can recover it (yggdrasil-115).
    #[test]
    fn checkpoint_captures_wip_without_polluting_branch() {
        let dir = std::env::temp_dir().join(format!("ygg-ckpt-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        g(&dir, &["init", "-q"]);
        g(&dir, &["config", "user.email", "test@ygg.local"]);
        g(&dir, &["config", "user.name", "ygg-test"]);
        std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
        g(&dir, &["add", "-A"]);
        g(&dir, &["commit", "-qm", "base"]);
        let base_head = g(&dir, &["rev-parse", "HEAD"]);

        // Mid-run state: a.txt modified, b.txt brand new and untracked.
        std::fs::write(dir.join("a.txt"), "v2\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();

        let run_id = Uuid::new_v4().to_string();
        let sha = checkpoint(&dir, run_id.clone()).expect("checkpoint sha");

        // Ref parks the checkpoint; commit captures the WIP tree.
        assert_eq!(
            g(&dir, &["rev-parse", &format!("refs/ygg/recovery/{run_id}")]),
            sha
        );
        assert_eq!(g(&dir, &["show", &format!("{sha}:a.txt")]), "v2");
        assert_eq!(g(&dir, &["show", &format!("{sha}:b.txt")]), "new");

        // Working branch untouched: HEAD unchanged, nothing staged in the real
        // index (we used a throwaway GIT_INDEX_FILE).
        assert_eq!(g(&dir, &["rev-parse", "HEAD"]), base_head);
        assert_eq!(g(&dir, &["diff", "--cached", "--name-only"]), "");

        std::fs::remove_dir_all(&dir).ok();
    }
}
