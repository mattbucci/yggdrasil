//! Bench runner — sets up a clean workspace per task, dispatches via the
//! configured driver, runs the deterministic grader, captures wall-clock +
//! tokens + commits, and writes results to bench_runs / bench_task_results.

use crate::bench::manifest::{LoadedManifest, TaskSpec};
use crate::bench::{Baseline, BenchRepo, BenchTaskResult};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Per-task outcome captured by the driver. Tokens are optional (Claude
/// usage block may not be available; the driver returns None).
#[derive(Debug, Clone, Default)]
pub struct DriverOutcome {
    pub passed: bool,
    pub wall_clock_s: u32,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub tokens_cache: Option<i64>,
    pub usd: Option<f64>,
    pub commit_sha: Option<String>,
    pub stderr_tail: Option<String>,
}

/// Driver contract. Implementations: vanilla-single (one process, all tasks
/// sequentially), vanilla-tmux (N parallel processes, no coordination),
/// ygg (scheduler-driven). Pluggable so tests can swap a fake driver.
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    fn baseline(&self) -> Baseline;
    /// Run the given task set in the given root directory. The driver is
    /// responsible for setting up isolated workspaces if it wants
    /// parallelism. Returns one outcome per task in input order.
    async fn run(
        &self,
        manifest: &LoadedManifest,
        root: &Path,
        tasks: &[TaskSpec],
    ) -> Result<Vec<DriverOutcome>, anyhow::Error>;
}

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Path to the binary the driver shells out to. Defaults to "claude" on
    /// PATH; tests override via YGG_BENCH_CLAUDE_BIN to a fake script.
    pub claude_bin: PathBuf,
    /// Per-task wall-clock cap. Drivers SIGKILL on overrun.
    pub task_timeout_s: u64,
    /// Where to set up per-run workspaces. Defaults to a temp dir.
    pub workspace_root: Option<PathBuf>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        let claude_bin = std::env::var("YGG_BENCH_CLAUDE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("claude"));
        Self {
            claude_bin,
            task_timeout_s: 600,
            workspace_root: None,
        }
    }
}

pub async fn execute(
    pool: &crate::db::DbPool,
    manifest: &LoadedManifest,
    driver: &dyn Driver,
    seed: Option<i64>,
    cfg: &RunnerConfig,
) -> Result<String, anyhow::Error> {
    let model =
        std::env::var("YGG_BENCH_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string());

    let repo = BenchRepo::new(pool);
    let bench_run = repo
        .create_run(
            &manifest.manifest.id,
            driver.baseline(),
            manifest.manifest.default_parallelism as i32,
            &model,
            &super::harness_sha(),
            seed,
        )
        .await?;

    // Set up an isolated workspace and clone the seed_repo into it.
    let workspace = match &cfg.workspace_root {
        Some(p) => p.clone(),
        None => {
            let base = std::env::temp_dir().join(format!("ygg-bench-{}", bench_run.run_id));
            std::fs::create_dir_all(&base)?;
            base
        }
    };
    init_seed_workspace(&manifest.seed_repo(), &workspace)?;

    let started = Instant::now();
    let driver_result = driver
        .run(manifest, &workspace, &manifest.manifest.tasks)
        .await;
    let wall_clock_total_s = started.elapsed().as_secs() as i64;

    let outcomes = match driver_result {
        Ok(v) => v,
        Err(e) => {
            repo.finalize(
                &bench_run.run_id,
                false,
                Some(&format!("driver error: {e}")),
            )
            .await?;
            return Err(e);
        }
    };

    let mut all_passed = !outcomes.is_empty();
    for (idx, out) in outcomes.iter().enumerate() {
        let result = BenchTaskResult {
            run_id: bench_run.run_id.clone(),
            task_idx: idx as i32,
            passed: out.passed,
            wall_clock_s: out.wall_clock_s as i32,
            tokens_in: out.tokens_in,
            tokens_out: out.tokens_out,
            tokens_cache: out.tokens_cache,
            usd: out.usd,
            reopened: false,
        };
        repo.write_task_result(&bench_run.run_id, &result).await?;
        all_passed &= out.passed;
    }
    repo.write_metric(
        &bench_run.run_id,
        "wall_clock_total_s",
        wall_clock_total_s as f64,
    )
    .await?;
    repo.write_metric(
        &bench_run.run_id,
        "tasks_passed",
        outcomes.iter().filter(|o| o.passed).count() as f64,
    )
    .await?;
    repo.finalize(&bench_run.run_id, all_passed, None).await?;

    // Run the deterministic grader on the final workspace state.
    let grader_passed = run_grader(manifest, &workspace).is_ok();
    if !grader_passed {
        // The driver said pass but the structural grader disagrees. Mark
        // the run failed and append a note so the user sees the disconnect.
        repo.finalize(
            &bench_run.run_id,
            false,
            Some("driver reported pass but grade.sh failed"),
        )
        .await?;
    }
    Ok(bench_run.run_id)
}

fn init_seed_workspace(seed: &Path, dest: &Path) -> Result<(), anyhow::Error> {
    if !dest.exists() {
        std::fs::create_dir_all(dest)?;
    }
    if seed.is_dir() {
        copy_dir_contents(seed, dest)?;
    }
    // Initialize git so commits work.
    if !dest.join(".git").exists() {
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dest)
            .status()?;
        if !status.success() {
            anyhow::bail!("git init failed in {}", dest.display());
        }
        // Identity required for commits in CI environments.
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "bench@yggdrasil.local"])
            .current_dir(dest)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "ygg bench"])
            .current_dir(dest)
            .status();
        // Initial commit so the workspace has HEAD.
        // Exclude per-task clone dirs so wrapper commits don't accidentally
        // pull in the embedded .git directories of .ygg-{i} / .clone-{i}.
        std::fs::write(
            dest.join(".gitignore"),
            "*.swp\n.DS_Store\n.ygg-*\n.clone-*\n",
        )?;
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dest)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "bench: seed"])
            .current_dir(dest)
            .status();
    }
    Ok(())
}

fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == ".gitkeep" {
            continue;
        }
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_dir_contents(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

fn run_grader(manifest: &LoadedManifest, workspace: &Path) -> Result<(), anyhow::Error> {
    let grader = manifest.grader_path();
    if !grader.exists() {
        anyhow::bail!("grader not found: {}", grader.display());
    }
    let status = std::process::Command::new("bash")
        .arg(&grader)
        .arg(workspace)
        .status()?;
    if !status.success() {
        anyhow::bail!("grader exited non-zero: {status}");
    }
    Ok(())
}
