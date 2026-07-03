//! Bench drivers — vanilla-single, vanilla-tmux, ygg. The drivers shell out
//! to the configured `claude_bin` and capture stdout/stderr, exit status,
//! and produced commits. Tests inject a fake binary that simulates Claude
//! by parsing the prompt and writing the requested files.

use crate::bench::Baseline;
use crate::bench::manifest::{LoadedManifest, TaskSpec};
use crate::bench::runner::{Driver, DriverOutcome, RunnerConfig};
use std::path::Path;
use std::time::Instant;

/// vanilla-single: one process handles all tasks sequentially. Closest to
/// "what a single Claude session does without any orchestrator." The same
/// claude_bin is invoked once per task with the prompt; sequential.
pub struct VanillaSingleDriver {
    pub config: RunnerConfig,
}

#[async_trait::async_trait]
impl Driver for VanillaSingleDriver {
    fn baseline(&self) -> Baseline {
        Baseline::VanillaSingle
    }

    async fn run(
        &self,
        _manifest: &LoadedManifest,
        root: &Path,
        tasks: &[TaskSpec],
    ) -> Result<Vec<DriverOutcome>, anyhow::Error> {
        let mut out = Vec::with_capacity(tasks.len());
        for t in tasks {
            out.push(invoke_claude(&self.config, root, &t.prompt).await?);
        }
        Ok(out)
    }
}

/// vanilla-tmux: N parallel processes, one per task. No coordination — each
/// gets the same starting workspace via `git clone` from the seed. After all
/// finish, we merge their output branches into the main workspace by copying
/// any new files (since tasks are independent).
pub struct VanillaTmuxDriver {
    pub config: RunnerConfig,
}

#[async_trait::async_trait]
impl Driver for VanillaTmuxDriver {
    fn baseline(&self) -> Baseline {
        Baseline::VanillaTmux
    }

    async fn run(
        &self,
        _manifest: &LoadedManifest,
        root: &Path,
        tasks: &[TaskSpec],
    ) -> Result<Vec<DriverOutcome>, anyhow::Error> {
        // Clone the workspace once per task so they're truly independent.
        let mut handles = Vec::with_capacity(tasks.len());
        for (i, t) in tasks.iter().enumerate() {
            let cloned = root.join(format!(".clone-{i}"));
            std::process::Command::new("git")
                .args([
                    "clone",
                    "-q",
                    root.to_string_lossy().as_ref(),
                    cloned.to_string_lossy().as_ref(),
                ])
                .status()?;
            // Identity in clone.
            let _ = std::process::Command::new("git")
                .args(["config", "user.email", "bench@yggdrasil.local"])
                .current_dir(&cloned)
                .status();
            let _ = std::process::Command::new("git")
                .args(["config", "user.name", "ygg bench"])
                .current_dir(&cloned)
                .status();
            let prompt = t.prompt.clone();
            let cfg = self.config.clone();
            handles.push(tokio::spawn(async move {
                invoke_claude(&cfg, &cloned, &prompt).await
            }));
        }

        let mut outcomes = Vec::with_capacity(tasks.len());
        for (i, h) in handles.into_iter().enumerate() {
            let mut outcome = h
                .await
                .map_err(|e| anyhow::anyhow!("tmux task {i} join: {e}"))??;
            // Merge files produced in the clone back into root. Independent
            // tasks; conflicts shouldn't happen by scenario design.
            let cloned = root.join(format!(".clone-{i}"));
            let _ = merge_clone(&cloned, root);
            // Also import the commit history so grade.sh sees the messages.
            let _ = std::process::Command::new("git")
                .args([
                    "fetch",
                    "-q",
                    cloned.to_string_lossy().as_ref(),
                    "+refs/heads/*:refs/remotes/clone/*",
                ])
                .current_dir(root)
                .status();
            // Stage + commit the merged files in root with a wrapper commit
            // that preserves the per-task message in the body so grade.sh
            // (which greps git log --all) can find both.
            let _ = std::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(root)
                .status();
            // Get the latest commit message from the clone for the wrapper.
            let msg = std::process::Command::new("git")
                .args(["log", "-1", "--pretty=%s"])
                .current_dir(&cloned)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| format!("vanilla-tmux task {i}"));
            let _ = std::process::Command::new("git")
                .args(["commit", "-q", "--allow-empty", "-m", &msg])
                .current_dir(root)
                .status();
            outcome.commit_sha = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string());
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

fn merge_clone(clone: &Path, dest: &Path) -> std::io::Result<()> {
    for entry in walkdir_files(clone) {
        let rel = entry.strip_prefix(clone).unwrap_or(&entry);
        if rel.starts_with(".git") {
            continue;
        }
        let target = dest.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Independent tasks: only copy files that don't exist yet, to avoid
        // clobbering. Scenario design guarantees no overlap.
        if !target.exists() {
            std::fs::copy(&entry, &target)?;
        }
    }
    Ok(())
}

fn walkdir_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    if entry.file_name() == ".git" {
                        continue;
                    }
                    out.extend(walkdir_files(&path));
                } else {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// ygg driver — for the bench MVP, runs claude_bin sequentially while
/// recording task_runs rows (so the scheduler's data model is exercised).
/// A future iteration uses the real scheduler + tmux + ygg spawn end-to-end.
pub struct YggDriver {
    pub config: RunnerConfig,
}

#[async_trait::async_trait]
impl Driver for YggDriver {
    fn baseline(&self) -> Baseline {
        Baseline::Ygg
    }

    async fn run(
        &self,
        _manifest: &LoadedManifest,
        root: &Path,
        tasks: &[TaskSpec],
    ) -> Result<Vec<DriverOutcome>, anyhow::Error> {
        // Parallel via tokio tasks — same shape as vanilla-tmux but with
        // independent clones to mirror worktree isolation. The scheduler-
        // backed end-to-end driver lands in a follow-up; this MVP exercises
        // the workspace + grader path for CI.
        let mut handles = Vec::with_capacity(tasks.len());
        for (i, t) in tasks.iter().enumerate() {
            let cloned = root.join(format!(".ygg-{i}"));
            std::process::Command::new("git")
                .args([
                    "clone",
                    "-q",
                    root.to_string_lossy().as_ref(),
                    cloned.to_string_lossy().as_ref(),
                ])
                .status()?;
            let _ = std::process::Command::new("git")
                .args(["config", "user.email", "bench@yggdrasil.local"])
                .current_dir(&cloned)
                .status();
            let _ = std::process::Command::new("git")
                .args(["config", "user.name", "ygg bench"])
                .current_dir(&cloned)
                .status();
            let prompt = t.prompt.clone();
            let cfg = self.config.clone();
            handles.push(tokio::spawn(async move {
                invoke_claude(&cfg, &cloned, &prompt).await
            }));
        }

        let mut outcomes = Vec::with_capacity(tasks.len());
        for (i, h) in handles.into_iter().enumerate() {
            let mut outcome = h
                .await
                .map_err(|e| anyhow::anyhow!("ygg task {i} join: {e}"))??;
            let cloned = root.join(format!(".ygg-{i}"));
            let _ = merge_clone(&cloned, root);
            let msg = std::process::Command::new("git")
                .args(["log", "-1", "--pretty=%s"])
                .current_dir(&cloned)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("ygg task {i}"));
            let _ = std::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(root)
                .status();
            let _ = std::process::Command::new("git")
                .args(["commit", "-q", "--allow-empty", "-m", &msg])
                .current_dir(root)
                .status();
            outcome.commit_sha = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string());
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }
}

/// Invoke the configured claude_bin with the prompt; capture wall-clock
/// and parse the JSON usage block if present. The fake binary used by tests
/// reads stdin and writes files according to a simple protocol.
pub async fn invoke_claude(
    cfg: &RunnerConfig,
    cwd: &Path,
    prompt: &str,
) -> Result<DriverOutcome, anyhow::Error> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let started = Instant::now();
    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.arg("-p").arg("--output-format").arg("json");
    cmd.current_dir(cwd);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", cfg.claude_bin.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await.ok();
        drop(stdin);
    }

    // Bound by task_timeout_s.
    let timeout = std::time::Duration::from_secs(cfg.task_timeout_s);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(anyhow::anyhow!("claude wait: {e}")),
        Err(_) => {
            return Ok(DriverOutcome {
                passed: false,
                wall_clock_s: timeout.as_secs() as u32,
                stderr_tail: Some(format!("timeout after {}s", timeout.as_secs())),
                ..Default::default()
            });
        }
    };

    let wall = started.elapsed().as_secs() as u32;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_tail = if output.stderr.is_empty() {
        None
    } else {
        Some(
            String::from_utf8_lossy(&output.stderr[output.stderr.len().saturating_sub(2000)..])
                .to_string(),
        )
    };

    // Parse a usage block if the binary emitted JSON. Real claude -p does;
    // the fake binary in tests omits the field and we tolerate that.
    let (tokens_in, tokens_out, tokens_cache, usd) = parse_usage(&stdout);

    let passed = output.status.success();
    Ok(DriverOutcome {
        passed,
        wall_clock_s: wall,
        tokens_in,
        tokens_out,
        tokens_cache,
        usd,
        commit_sha: None,
        stderr_tail,
    })
}

fn parse_usage(stdout: &str) -> (Option<i64>, Option<i64>, Option<i64>, Option<f64>) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(stdout).ok();
    let Some(v) = parsed else {
        return (None, None, None, None);
    };
    let usage = v
        .get("usage")
        .or_else(|| v.get("response").and_then(|r| r.get("usage")));
    let i = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|n| n.as_i64());
    let o = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|n| n.as_i64());
    let c = usage
        .and_then(|u| u.get("cache_read_input_tokens"))
        .and_then(|n| n.as_i64());
    // Cost is display-only; round to micro-dollar precision to match the
    // old NUMERIC(_,6) storage.
    let usd = v
        .get("total_cost_usd")
        .and_then(|n| n.as_f64())
        .map(|f| (f * 1_000_000.0).round() / 1_000_000.0);
    (i, o, c, usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_usage_handles_missing() {
        let (i, o, c, u) = parse_usage("not json");
        assert!(i.is_none() && o.is_none() && c.is_none() && u.is_none());
    }

    #[test]
    fn parse_usage_extracts_token_counts() {
        let s = r#"{"usage":{"input_tokens":100,"output_tokens":42,"cache_read_input_tokens":12000},"total_cost_usd":0.0123}"#;
        let (i, o, c, u) = parse_usage(s);
        assert_eq!(i, Some(100));
        assert_eq!(o, Some(42));
        assert_eq!(c, Some(12000));
        assert!(u.is_some());
    }
}
