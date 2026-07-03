# Eval benchmarks — `ygg bench`

> Measurable answers to "does Yggdrasil coordinate agents better than vanilla Claude Code?" Companion to [ADR 0016](adr/0016-autonomous-execution.md). Separate from `ygg eval` (which was a dashboard over retrieval-pipeline events and is on the deprecation track in [ADR 0015 Phase 4](adr/0015-retrieval-scope-reduction.md)).

## The thesis that must be tested

The pivot (ADR 0015 + 0016) bets that **pure orchestration is what's valuable** and that **autonomous execution makes multi-agent coding faster and more reliable than a single agent running sequentially**. That bet is either right or wrong. Without measurement it stays vibes. `ygg bench` is how we convert the vibes to numbers.

Three questions this suite must be able to answer:

1. **Throughput.** Does Yggdrasil finish N tasks faster than the alternatives?
2. **Reliability.** Does it finish N tasks *consistently* — not just in the best run?
3. **Cost.** What does the throughput cost in tokens, agent-hours, and human interventions?

A positive result is Pareto dominance on at least two of the three, plus no regression on the third. A negative result is a corrective direction ("locks add overhead that cancels parallelism", "retries burn tokens without improving pass-rate", etc.) and shapes the next iteration.

## Why no existing benchmark works

Every public benchmark we surveyed measures one agent solving one task. SWE-bench Verified, SWE-Lancer, MLE-bench, Terminal-Bench 2.0, cline-bench, OSWorld, GAIA, AgentBench, tau2-bench — all single-agent. MultiAgentBench and MASLab come closest but measure debate/research workloads, not coordinated coding DAGs with shared state.

What transfers:

- **Task-format** from SWE-bench Verified / cline-bench: git repo + tests = ground truth.
- **Reliability metric** from tau2-bench: `pass^k` (all k runs succeeded), not `pass@k` (at least one).
- **Milestone attribution** from MultiAgentBench: tag which agent hit which sub-goal.
- **Time-horizon methodology** from METR: fit a logistic curve of success rate vs log(human-estimated task duration); the 50%-crossing is the headline number that plots cleanly over time.
- **End-state grading only**, no trajectory matching — Anthropic's own finding (multi-agent research system) is that identical starting points produce different valid paths.
- **Deterministic grader first, LLM-judge only as a quality score complement** — never as pass/fail.

What doesn't transfer (and we don't try to fake):

- Benchmarks that assume the unit of work is a single context window.
- Benchmarks that grade on trajectory match or prompt quality.
- LLM-judge as the sole grader.

## Architecture

```
                             ┌──────────────────────────────┐
 ygg bench run <scenario> ──►│  bench runner (Rust)         │
     --baseline ygg|vanilla  │                              │
     --runs 4  --seed 42     │  1. load scenario fixture    │
                             │  2. checkout seed repo       │
                             │  3. start N agents per mode  │
                             │  4. wait for terminal state  │
                             │  5. run grade.sh             │
                             │  6. record metrics           │
                             └──────────┬───────────────────┘
                                        ▼
                            ┌───────────────────────────┐
                            │  SQLite                   │
                            │    bench_runs             │
                            │    bench_task_results     │
                            │    bench_metrics          │
                            └───────────────────────────┘
                                        ▼
                         ygg bench report <run-id>
                         ygg bench diff <a> <b>
                         ygg bench ci   --tier smoke
```

New tables:

```sql
CREATE TABLE bench_runs (
    run_id        UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    scenario      TEXT NOT NULL,        -- e.g. "independent-parallel-n"
    baseline      TEXT NOT NULL,        -- 'vanilla-single'|'vanilla-tmux'|'ygg-N'
    parallelism   INT NOT NULL,
    model         TEXT NOT NULL,        -- claude model id, for audit
    harness_sha   TEXT NOT NULL,        -- ygg bench git SHA
    seed          BIGINT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at      TIMESTAMPTZ,
    passed        BOOLEAN,              -- deterministic grader result
    notes         TEXT
);

CREATE TABLE bench_task_results (
    run_id        UUID NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    task_idx      INT NOT NULL,         -- index within scenario
    passed        BOOLEAN NOT NULL,
    wall_clock_s  INT NOT NULL,
    tokens_in     BIGINT,
    tokens_out    BIGINT,
    tokens_cache  BIGINT,
    usd           NUMERIC,
    reopened      BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (run_id, task_idx)
);

CREATE TABLE bench_metrics (
    run_id     UUID NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    metric     TEXT NOT NULL,            -- 'lock_wait_p50_ms', 'agent_hours', …
    value      DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (run_id, metric)
);
```

## Scenarios (MVP: six)

Each scenario lives under `benches/scenarios/<name>/` with:

- `manifest.toml` — task list, DAG edges, expected terminal state
- `seed_repo/` — starting git state (or URL + commit hash; preferred for reproducibility)
- `grade.sh` — deterministic grader (exit 0 pass, non-zero fail; JSON on stderr for partials)

### 1. `independent-parallel-n` — throughput baseline

N = 4 independent tasks with no shared files. Tasks: "add a new markdown doc page for topic T_i" for T_i ∈ {api-retry, db-config, graphql-errors, test-patterns}.

- **Primary metric:** wall-clock.
- **Grading:** each file exists with required headings.
- **Baseline expectation:** `ygg-4` ≈ 4× faster than `vanilla-single`. If not, coordination overhead dominates.

### 2. `dag-linear-3` — sequencing correctness

A → B → C. A adds a utility function. B adds a caller. C adds a test for the caller.

- **Primary metric:** pass^4.
- **Grading:** tests pass; commit order enforced (B commit sha parent chain includes A).
- **Failure mode to catch:** agents starting B before A's work is visible in the worktree.

### 3. `fan-out-fan-in` — aggregation

Root: "split module `Y` into three submodules `y/a`, `y/b`, `y/c`." Root spawns three children. Each child produces one submodule. Aggregator (a critic run on root's task) verifies the three land coherently.

- **Primary metric:** DAG completion rate across 4 runs.
- **Grading:** all three submodules present; original file references updated.
- **Failure mode:** partial aggregation (two of three submodules materialized).

### 4. `contention` — lock correctness

Two tasks both need to edit the same file (`Cargo.toml`, bumping two different dependencies).

- **Primary metric:** contention events recorded AND both updates landed.
- **Grading:** both versions bumped; file parses; `cargo check` green.
- **Failure mode to catch:** race — one edit overwrites the other.

### 5. `failure-recovery` — fault tolerance

DAG with injected failure. Harness sends `SIGKILL` to one agent mid-work (triggered at event count 50 within the run). Scheduler should reap the crashed attempt, retry or re-route, reach final state.

- **Primary metric:** recovery rate.
- **Grading:** final state equivalent to a no-failure run.
- **Failure mode to catch:** stuck task; deadlock; retry exhaustion when retry is appropriate.

### 6. `long-horizon-refactor` — METR-style

Real-ish task: "rename `Foo` to `Bar` across the codebase; update call sites; update tests; update docs." Human-time estimate derived from a past real rename of similar scope (from `git log` on this or a fixture repo).

- **Primary metric:** success rate at the human-time-estimate wall-clock point.
- **Grading:** `cargo check && cargo test && grep -r Foo src/ = empty`.
- **Use:** feed into a cross-scenario METR logistic fit to plot "Yggdrasil's 50%-horizon grew from X to Y over the quarter."

## Baselines

Three baselines, identical task set, identical model tier.

### `vanilla-single`
One `claude -p` subprocess, given the whole DAG as a prompt, left to sequence it internally using native TodoWrite and Task agents. No shared DB, no `ygg` commands. Represents "what a solo developer gets out of the box."

### `vanilla-tmux`
N independent `claude -p` subprocesses in parallel tmux windows. Each given one sub-task of the scenario's DAG. No shared DB, no `ygg` commands, no coordination. Represents "what a developer does with a naive bash loop."

### `ygg-N`
N agents spawned via `ygg spawn` with the DAG pre-loaded via `ygg task create`. Scheduler running. Locks, messaging, run-tracking, retry — everything.

Each baseline gets k=4 runs per scenario (so 24 runs per full sweep per baseline). Smoke tier runs 1 baseline × 1 scenario × 1 run.

## Metrics

### Tier A — ship day one

| Metric | Definition | Source |
|---|---|---|
| **Wall-clock** | max(task.ended_at) − min(task.started_at) per run | `bench_task_results` |
| **Pass^k (k=4)** | fraction of (scenario × baseline) quartets where all 4 passed | aggregate over `bench_runs` |
| **Token cost per run** | Σ (tokens_in + tokens_out + cache_read × 0.1) per run | `claude -p`'s `usage` field in JSON output |
| **USD per run** | Σ usd per run (from same source) | `bench_task_results` |
| **Agent-hours per run** | Σ (agent.stopped_at − agent.started_at) | `agents` table joined on session |
| **DAG completion rate** | tasks(final == done) / tasks(total) per run | `tasks` table at terminal state |
| **Lock-wait p50/p95/max** | time from lock-request event → lock-acquired event | `events` table |
| **Rework rate** | tasks reopened after first close / tasks ever closed | `tasks.close_reason` history |

All Tier A metrics are cheap to compute, low-noise after k=4 averaging, and directly map to a value claim.

### Tier B — add when tooling exists

| Metric | Notes |
|---|---|
| **Routing latency** | Ready → first claim. Needs `task_ready` event distinct from `task_created`. |
| **Message-bus utilization** | Messages sent / tasks completed. Already tracked. |
| **Deadlock rate** | No progress > N min + at least one task blocked. |
| **Human-intervention rate** | `ygg interrupt` events per run. |
| **Time-to-first-commit / PR / merge** | Needs git hooks feeding timestamps. |
| **Context efficiency** | Merged-diff tokens / total tokens consumed. |

### Tier C — deferred, noisy or expensive

- Communication-quality via LLM-judge (too variable).
- Subjective agent-satisfaction scores (meaningless without humans).
- Cross-DAG "planning-quality" scores (subjective; requires labeled correct-routing set).

## Grading hierarchy

1. **Deterministic grader** (`grade.sh`). Always preferred. Binary pass/fail.
2. **Structural end-state diff.** For "many valid shapes" scenarios (fan-out-fan-in's aggregate PR), assert structural properties: git log shape, file existence, semver delta, module import graph.
3. **LLM-judge as a quality score** (NOT pass/fail). Score the output's PR description against a fixed rubric. Use length-controlled pairwise preference (AlpacaEval trick) when comparing vanilla vs Yggdrasil. Spot-check 5% against a human reviewer to keep the judge honest. Report agreement.

**Explicit rejections:**
- Trajectory grading ("did the agent think right things?") — see Anthropic multi-agent research system.
- Single-sample scoring — every scenario runs k ≥ 4. Report mean ± 95% CI via bootstrap + pass^k.

## Reliability strategy

Yggdrasil's benchmarks are evaluating a stochastic system with a stochastic ground truth. This is irreducible. Mitigations:

1. **Pin everything we control.** Model version, prompt templates, tool allowlist, `--temperature 0` where supported, initial git commit hash, scenario fixture SHA, harness SHA (the SHA of the `ygg bench` binary at run time). The `bench_runs` table records all of these per run.
2. **k ≥ 4 independent runs** per (scenario, baseline) tuple. Always. No single-sample claims.
3. **Report pass^k alongside pass@k.** Pass@k ≥ pass^k always; the gap is the reliability loss.
4. **Bootstrap 95% CIs on all means.** Non-parametric, cheap, appropriate for pass-rate data.
5. **Refuse to declare a winner if CIs overlap.** `ygg bench diff` prints "inconclusive" rather than picking a misleading point estimate.
6. **Pin benchmark tasks by date** (SWE-rebench lesson). Any scenario synthesized from repo history must derive from PRs post-dating the model's training cutoff.
7. **Freeze and version the harness.** Cross-harness comparisons require a calibration run on both.

## Three tiers for three cadences

| Tier | Runs | Scenarios | Model | Wall-clock | Cadence | Purpose |
|---|---|---|---|---|---|---|
| **smoke** | 1 | Scenario 1 w/ N=2 | Haiku or cheap stand-in | <3 min | every PR | "does it start & finish at all" |
| **regression** | 3 | 1, 2, 3, 4 | Sonnet | ~45 min | nightly on main | catch orchestration regressions |
| **overnight** | 4–8 | all 6 | Sonnet/Opus | 3–6 h | weekly + pre-release | headline numbers, METR fit, blog-quality |

Smoke is deterministic-grader-only; no LLM-judge; <$1/run. Overnight ~$50/run across all baselines.

Regression tier gates main: if pass^4 drops > 1σ below rolling median on any scenario, CI fails. Requires green rerun or explicit `[bench-waived]` in the PR.

## CLI surface

```
ygg bench run <scenario> [--baseline vanilla-single|vanilla-tmux|ygg-N]
                         [--runs 4] [--seed 42] [--model sonnet]
                         [--tier smoke|regression|overnight]

ygg bench list                 # known scenarios
ygg bench report <run-id>      # pretty-print one run
ygg bench diff <run-a> <run-b> # compare two runs
ygg bench ci --tier smoke      # CI-mode: non-zero exit on regression
```

## Example output

```
$ ygg bench diff $VANILLA_RUN $YGG_RUN

Scenario:  independent-parallel-n (N=4)
Harness:   a1b2c3d (2026-04-24)
Model:     claude-sonnet-4-6

                           vanilla-single   vanilla-tmux      ygg-4
wall-clock (mean ± 95%)       18m 42s ± 82     7m 10s ± 51    6m 20s ± 34
pass^4                        3/4             1/4            4/4
agent-hours                  18m 42s          26m 40s        24m  8s
tokens (input + output)          42k ± 3k       55k ± 7k       51k ± 4k
usd                          $0.41 ± 0.03    $0.58 ± 0.09   $0.53 ± 0.05
rework rate                   1/4              3/4            0/4
lock-wait p50                 —                —              0s
lock-wait p95                 —                —              0s

Verdict (CI non-overlap):
  ygg-4 > vanilla-single on wall-clock, pass^4, rework
  ygg-4 ≈ vanilla-tmux on wall-clock
  ygg-4 > vanilla-tmux on pass^4, rework, usd
```

Read this as: "ygg-4 achieves vanilla-tmux's throughput with vanilla-single's reliability, at roughly-equivalent cost." If the numbers aren't that clean, we know what to fix.

## What "overnight-deliverable" looks like

Specification + skeleton, not a full suite. A half-shipped flaky full suite produces misleading numbers that people cite; better to ship an honest MVP.

**Must ship in the epic's deliverable window:**

1. `ygg bench` subcommand skeleton (`run`, `list`, `report`)
2. `bench_runs`, `bench_task_results`, `bench_metrics` tables + migration
3. Scenario 1 (`independent-parallel-n`) end-to-end runnable against all three baselines
4. `claude -p` driver for vanilla-single and vanilla-tmux
5. `ygg spawn`-based driver for ygg-N
6. Report format printing wall-clock + pass^k + token-cost + agent-hours with 95% CIs
7. `ygg bench ci --tier smoke` that fits in <3 min and exits non-zero on regression

**Should ship soon after:**

8. Scenario 4 (`contention`) to prove lock correctness is measurable
9. `ygg bench diff` for before/after PR comparisons

**Nice-to-have (explicitly later):**

10. Scenarios 2, 3, 5, 6
11. LLM-as-judge for PR-quality subscore on Scenario 1
12. METR-style logistic-fit plotting (needs Scenario 6 first)
13. Dashboard integration

**Explicit non-goals:**

- Beat SWE-bench Verified. That's a single-agent bench; doesn't answer our question.
- Replace `ygg eval`'s retrieval view. That gets deleted in ADR 0015 Phase 4 independently.
- Publish a leaderboard. Internal only until methodology has ≥10 internal runs without surprises.

## Known limitations

- **`claude -p` stalls on mixed web-search + long-write.** Documented Claude Code bug. Any scenario tripping it will look like a Yggdrasil failure. Mitigation: every task has a hard timeout; `failed_timeout` is a distinct terminal state from `failed_grader`.
- **Vanilla-tmux baseline is subjective.** The "ad-hoc tmux" setup varies across humans. Proposed rule: vanilla-tmux gets the same per-task prompts Yggdrasil uses, but no shared DB and no `ygg` commands at all. That's the ceiling of naive parallelism; anything beyond is coordination we're building.
- **Token-cost variance up to 10× on the same task.** Known from recent agent benchmark literature. Report percentiles, not means, for cost. Always condition cost on pass ("conditional on pass, median cost was $X").
- **Temperature 0 ≠ determinism.** Batching, hardware, model updates all introduce variance even at temp 0. We can't have repeatability; we can have statistical stability. Hence k ≥ 4.
- **This repo's test suite gates the grader.** If we bench on this repo, grader flakiness = benchmark flakiness. Mitigation: use external vendored fixtures under `benches/fixtures/` so grader depends on the fixture's suite, not this codebase.

## Why a separate subcommand

`ygg eval` is a read-view over live events — it summarizes retrieval-pipeline telemetry (hit rate, classifier decisions, cache savings). That surface is on the ADR 0015 retirement track. Even once modernized, `ygg eval` remains a dashboard over *live* state, not a benchmark harness.

`ygg bench` orchestrates subprocesses, captures metrics, writes its own tables, produces reproducible reports. Entangling the two makes the dashboard carry subprocess complexity and the benchmark carry UI coupling. The file split is `src/cli/bench_cmd.rs` + `src/bench/{harness,scenarios,grading,baselines,report}.rs`.

## Sources

- [SWE-bench Verified](https://openai.com/index/introducing-swe-bench-verified/) — task format, test-patch grading
- [SWE-Lancer](https://openai.com/index/swe-lancer/) — manager-task style, real-world payoff
- [cline-bench](https://cline.bot/blog/cline-bench-initiative) — real-user-session-to-benchmark pipeline
- [SWE-rebench](https://swe-rebench.com) — training-cutoff decontamination discipline
- [Terminal-Bench 2.0](https://www.tbench.ai/) — terminal-agent baseline
- [tau-bench / tau2-bench](https://arxiv.org/abs/2406.12045) — pass^k reliability metric
- [METR time horizons](https://metr.org/blog/2025-03-19-measuring-ai-ability-to-complete-long-tasks/) — 50%-horizon methodology
- [MultiAgentBench](https://arxiv.org/abs/2503.01935) — milestone attribution, topology-as-variable
- [MASLab](https://arxiv.org/pdf/2505.16988) — heterogeneous-workload benchmarking
- [Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system) — end-state-only grading finding
- [Demystifying evals for AI agents (Anthropic)](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)
- [AlpacaEval length-controlled](https://arxiv.org/html/2404.04475v1) — LLM-judge length-control trick
- [Claude Code headless mode](https://code.claude.com/docs/en/headless) — `claude -p` substrate and known stall behavior
