//! `ygg bench` — orchestrator eval suite. Specified in docs/eval-benchmarks.md.
//!
//! The bench harness is intentionally separate from `ygg eval` (which is a
//! dashboard over retrieval-pipeline events on the ADR 0015 deprecation
//! track). `ygg bench` runs scripted scenarios across three baselines and
//! produces comparable numbers persisted to `bench_runs` / `bench_task_results`
//! / `bench_metrics`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::DbPool;

pub mod drivers;
pub mod manifest;
pub mod runner;
pub mod scenarios;
pub mod stats;

/// Three baselines per docs/eval-benchmarks.md § Baselines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Baseline {
    /// One `claude -p` for the whole DAG, no shared DB.
    VanillaSingle,
    /// N parallel `claude -p` windows, no coordination.
    VanillaTmux,
    /// `ygg spawn` × N + scheduler.
    Ygg,
}

impl Baseline {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::VanillaSingle => "vanilla-single",
            Self::VanillaTmux => "vanilla-tmux",
            Self::Ygg => "ygg",
        }
    }
}

impl std::str::FromStr for Baseline {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "vanilla-single" | "single" => Ok(Self::VanillaSingle),
            "vanilla-tmux" | "tmux" => Ok(Self::VanillaTmux),
            "ygg" | "ygg-N" => Ok(Self::Ygg),
            _ => Err(format!(
                "unknown baseline: {s} (try vanilla-single|vanilla-tmux|ygg)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    /// <3 min, Scenario 1 only, deterministic-grader-only.
    Smoke,
    /// ~45 min, scenarios 1–4.
    Regression,
    /// 3–6h, all scenarios, full reporting.
    Overnight,
}

impl Tier {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Regression => "regression",
            Self::Overnight => "overnight",
        }
    }
}

impl std::str::FromStr for Tier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "smoke" => Ok(Self::Smoke),
            "regression" | "reg" => Ok(Self::Regression),
            "overnight" => Ok(Self::Overnight),
            _ => Err(format!(
                "unknown tier: {s} (try smoke|regression|overnight)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct BenchRun {
    pub run_id: String,
    pub scenario: String,
    pub baseline: String,
    pub parallelism: i32,
    pub model: String,
    pub harness_sha: String,
    pub seed: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub passed: Option<bool>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct BenchTaskResult {
    pub run_id: String,
    pub task_idx: i32,
    pub passed: bool,
    pub wall_clock_s: i32,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub tokens_cache: Option<i64>,
    pub usd: Option<f64>,
    pub reopened: bool,
}

pub struct BenchRepo<'a> {
    pool: &'a DbPool,
}

impl<'a> BenchRepo<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    pub async fn create_run(
        &self,
        scenario: &str,
        baseline: Baseline,
        parallelism: i32,
        model: &str,
        harness_sha: &str,
        seed: Option<i64>,
    ) -> Result<BenchRun, sqlx::Error> {
        sqlx::query_as::<_, BenchRun>(
            r#"INSERT INTO bench_runs (scenario, baseline, parallelism, model, harness_sha, seed)
               VALUES ($1, $2, $3, $4, $5, $6) RETURNING *"#,
        )
        .bind(scenario)
        .bind(baseline.as_str())
        .bind(parallelism)
        .bind(model)
        .bind(harness_sha)
        .bind(seed)
        .fetch_one(self.pool)
        .await
    }

    pub async fn finalize(
        &self,
        run_id: &str,
        passed: bool,
        notes: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE bench_runs SET ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), passed = $2, notes = $3 WHERE run_id = $1",
        )
        .bind(run_id)
        .bind(passed)
        .bind(notes)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_run(&self, run_id: &str) -> Result<Option<BenchRun>, sqlx::Error> {
        sqlx::query_as::<_, BenchRun>("SELECT * FROM bench_runs WHERE run_id = $1")
            .bind(run_id)
            .fetch_optional(self.pool)
            .await
    }

    pub async fn list_runs(
        &self,
        scenario: Option<&str>,
        limit: i64,
    ) -> Result<Vec<BenchRun>, sqlx::Error> {
        if let Some(s) = scenario {
            sqlx::query_as::<_, BenchRun>(
                "SELECT * FROM bench_runs WHERE scenario = $1 ORDER BY started_at DESC LIMIT $2",
            )
            .bind(s)
            .bind(limit)
            .fetch_all(self.pool)
            .await
        } else {
            sqlx::query_as::<_, BenchRun>(
                "SELECT * FROM bench_runs ORDER BY started_at DESC LIMIT $1",
            )
            .bind(limit)
            .fetch_all(self.pool)
            .await
        }
    }

    pub async fn write_task_result(
        &self,
        run_id: &str,
        result: &BenchTaskResult,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO bench_task_results
               (run_id, task_idx, passed, wall_clock_s, tokens_in, tokens_out, tokens_cache, usd, reopened)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#,
        )
        .bind(run_id)
        .bind(result.task_idx)
        .bind(result.passed)
        .bind(result.wall_clock_s)
        .bind(result.tokens_in)
        .bind(result.tokens_out)
        .bind(result.tokens_cache)
        .bind(result.usd)
        .bind(result.reopened)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_task_results(
        &self,
        run_id: &str,
    ) -> Result<Vec<BenchTaskResult>, sqlx::Error> {
        sqlx::query_as::<_, BenchTaskResult>(
            "SELECT * FROM bench_task_results WHERE run_id = $1 ORDER BY task_idx",
        )
        .bind(run_id)
        .fetch_all(self.pool)
        .await
    }

    pub async fn write_metric(
        &self,
        run_id: &str,
        metric: &str,
        value: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO bench_metrics (run_id, metric, value) VALUES ($1, $2, $3)
               ON CONFLICT (run_id, metric) DO UPDATE SET value = EXCLUDED.value"#,
        )
        .bind(run_id)
        .bind(metric)
        .bind(value)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

/// Best-effort harness fingerprint for the bench run row. Reads the current
/// HEAD via `git rev-parse`. If git fails we fall back to "unknown" — the
/// bench still runs but cross-version comparisons become opt-in. Stays light
/// to keep `ygg bench` usable inside containers without git.
pub fn harness_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
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
        .unwrap_or_else(|| "unknown".to_string())
}
