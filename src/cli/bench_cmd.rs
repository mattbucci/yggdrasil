//! `ygg bench` subcommand. Specified in docs/eval-benchmarks.md.

use crate::bench::drivers::{VanillaSingleDriver, VanillaTmuxDriver, YggDriver};
use crate::bench::manifest::LoadedManifest;
use crate::bench::runner::{Driver, RunnerConfig};
use crate::bench::stats::{Verdict, bootstrap_mean_ci, ci_diff_verdict, pass_at_k, pass_power_k};
use crate::bench::{Baseline, BenchRepo, Tier, harness_sha, scenarios};
use crate::db::DbPool;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const GRAY: &str = "\x1b[38;5;245m";

pub fn list() {
    println!("{DIM}id{RESET}                          {DIM}status{RESET}      {DIM}title{RESET}");
    for s in scenarios::ALL {
        let status = if s.implemented {
            format!("{GREEN}ready{RESET}")
        } else {
            format!("{GRAY}stub{RESET} ")
        };
        println!("  {:<26} {status}    {}", s.id, s.title);
    }
    println!();
    println!("{DIM}Specs in docs/eval-benchmarks.md.{RESET}");
}

pub async fn report(pool: &DbPool, run_id: String) -> anyhow::Result<()> {
    let repo = BenchRepo::new(pool);
    let run = repo
        .get_run(&run_id.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no bench run with id {run_id}"))?;
    let results = repo.list_task_results(&run_id.clone()).await?;

    let pass_count = results.iter().filter(|r| r.passed).count();
    let total = results.len();
    let wall = results.iter().map(|r| r.wall_clock_s).max().unwrap_or(0);
    let tokens_in: i64 = results.iter().filter_map(|r| r.tokens_in).sum();
    let tokens_out: i64 = results.iter().filter_map(|r| r.tokens_out).sum();

    println!("Bench run: {run_id}");
    println!("  scenario:    {}", run.scenario);
    println!(
        "  baseline:    {} (parallelism={})",
        run.baseline, run.parallelism
    );
    println!("  model:       {}", run.model);
    println!("  harness_sha: {}", run.harness_sha);
    if let Some(seed) = run.seed {
        println!("  seed:        {seed}");
    }
    println!("  started_at:  {}", run.started_at);
    if let Some(ended) = run.ended_at {
        println!(
            "  duration:    {}s",
            (ended - run.started_at).num_seconds().max(0)
        );
    }
    let verdict = match run.passed {
        Some(true) => format!("{GREEN}PASS{RESET}"),
        Some(false) => format!("{RED}FAIL{RESET}"),
        None => format!("{DIM}in-progress{RESET}"),
    };
    println!("  verdict:     {verdict}");
    println!("  tasks:       {pass_count}/{total} passed");
    println!("  wall-clock:  {wall}s");
    println!("  tokens:      in={tokens_in} out={tokens_out}");
    if let Some(notes) = run.notes {
        println!("  notes:       {notes}");
    }
    Ok(())
}

pub async fn run(
    pool: &DbPool,
    scenario_id: &str,
    baseline: Baseline,
    parallelism: i32,
    model: &str,
    seed: Option<i64>,
) -> anyhow::Result<String> {
    let spec = scenarios::find(scenario_id)
        .ok_or_else(|| anyhow::anyhow!("unknown scenario: {scenario_id}"))?;

    if !spec.implemented {
        let repo = BenchRepo::new(pool);
        let run = repo
            .create_run(spec.id, baseline, parallelism, model, &harness_sha(), seed)
            .await?;
        repo.finalize(
            &run.run_id,
            false,
            Some("scenario driver not implemented yet"),
        )
        .await?;
        anyhow::bail!("scenario driver not implemented: {scenario_id}");
    }

    // Load fixture from CARGO_MANIFEST_DIR/benches/scenarios/<id>/. When run
    // installed (no source tree alongside), users can override via
    // YGG_BENCH_SCENARIOS_DIR.
    let scenarios_dir = std::env::var("YGG_BENCH_SCENARIOS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/scenarios")
        });
    let manifest = LoadedManifest::load(scenarios_dir.join(spec.id))?;

    let cfg = RunnerConfig::default();
    let driver: Box<dyn Driver> = match baseline {
        Baseline::VanillaSingle => Box::new(VanillaSingleDriver {
            config: cfg.clone(),
        }),
        Baseline::VanillaTmux => Box::new(VanillaTmuxDriver {
            config: cfg.clone(),
        }),
        Baseline::Ygg => Box::new(YggDriver {
            config: cfg.clone(),
        }),
    };

    let _ = parallelism; // honored by the manifest's default_parallelism
    let _ = model; // recorded by create_run inside execute()
    let run_id =
        crate::bench::runner::execute(pool, &manifest, driver.as_ref(), seed, &cfg).await?;
    println!("bench run: {run_id}");
    Ok(run_id)
}

pub async fn diff(pool: &DbPool, a: String, b: String) -> anyhow::Result<()> {
    let repo = BenchRepo::new(pool);
    let run_a = repo
        .get_run(&a.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no bench run {a}"))?;
    let run_b = repo
        .get_run(&b.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no bench run {b}"))?;

    if run_a.scenario != run_b.scenario {
        anyhow::bail!(
            "scenarios differ ({} vs {}); diff requires same scenario",
            run_a.scenario,
            run_b.scenario
        );
    }

    let res_a = repo.list_task_results(&a).await?;
    let res_b = repo.list_task_results(&b).await?;

    let wall_a: Vec<f64> = res_a.iter().map(|r| r.wall_clock_s as f64).collect();
    let wall_b: Vec<f64> = res_b.iter().map(|r| r.wall_clock_s as f64).collect();
    let (m_a, lo_a, hi_a) = bootstrap_mean_ci(&wall_a, 1000);
    let (m_b, lo_b, hi_b) = bootstrap_mean_ci(&wall_b, 1000);

    let pass_a = res_a.iter().filter(|r| r.passed).count();
    let pass_b = res_b.iter().filter(|r| r.passed).count();

    println!("Scenario:  {}", run_a.scenario);
    println!();
    println!(
        "                  {DIM}{}{RESET}            {DIM}{}{RESET}",
        run_a.baseline, run_b.baseline
    );
    println!(
        "wall-clock mean   {:>7.1}s [{:>5.1},{:>5.1}]   {:>7.1}s [{:>5.1},{:>5.1}]",
        m_a, lo_a, hi_a, m_b, lo_b, hi_b
    );
    println!(
        "tasks-passed      {:>6}                 {:>6}",
        pass_a, pass_b
    );

    let v = ci_diff_verdict(lo_a, hi_a, lo_b, hi_b);
    println!();
    let verdict = match v {
        Verdict::Inconclusive => format!("{DIM}inconclusive (95% CIs overlap){RESET}"),
        Verdict::ALess => format!(
            "{GREEN}{} faster than {} (95% CIs disjoint){RESET}",
            run_a.baseline, run_b.baseline
        ),
        Verdict::AMore => format!(
            "{GREEN}{} faster than {} (95% CIs disjoint){RESET}",
            run_b.baseline, run_a.baseline
        ),
    };
    println!("Verdict: {verdict}");
    Ok(())
}

pub async fn ci(pool: &DbPool, tier: Tier) -> anyhow::Result<()> {
    let scenario_ids: Vec<&str> = match tier {
        Tier::Smoke => vec!["independent-parallel-n"],
        Tier::Regression => vec![
            "independent-parallel-n",
            "dag-linear-3",
            "fan-out-fan-in",
            "contention",
        ],
        Tier::Overnight => scenarios::ALL.iter().map(|s| s.id).collect(),
    };

    for scenario_id in scenario_ids {
        let Some(spec) = scenarios::find(scenario_id) else {
            continue;
        };
        if !spec.implemented {
            eprintln!("ci: skipping {scenario_id} (driver not implemented)");
            continue;
        }
        eprintln!("ci: running {scenario_id} ygg baseline...");
        let run_id = run(
            pool,
            scenario_id,
            Baseline::Ygg,
            4,
            "claude-sonnet-4-6",
            None,
        )
        .await?;
        let bench = BenchRepo::new(pool)
            .get_run(&run_id.clone())
            .await?
            .ok_or_else(|| anyhow::anyhow!("just-created run vanished"))?;
        if bench.passed != Some(true) {
            anyhow::bail!("ci: {scenario_id} did not pass (run {run_id})");
        }
    }
    eprintln!("ci: all scenarios passed");
    Ok(())
}

/// Aggregate over the last N runs of (scenario, baseline) to compute pass^k.
/// Used by `ygg bench ci` regression-gate logic; exposed for tests.
pub async fn pass_power_k_for(
    pool: &DbPool,
    scenario: &str,
    baseline: Baseline,
    k: usize,
) -> anyhow::Result<f64> {
    let runs: Vec<(bool,)> = sqlx::query_as(
        r#"SELECT COALESCE(passed, false) FROM bench_runs
            WHERE scenario = $1 AND baseline = $2
            ORDER BY started_at DESC LIMIT $3"#,
    )
    .bind(scenario)
    .bind(baseline.as_str())
    .bind(k as i64)
    .fetch_all(pool)
    .await?;
    let bools: Vec<bool> = runs.into_iter().map(|(b,)| b).collect();
    Ok(pass_power_k(&bools, k))
}

#[allow(dead_code)]
fn _ensure_pass_at_k_used(p: &[bool]) -> f64 {
    pass_at_k(p, p.len())
}
