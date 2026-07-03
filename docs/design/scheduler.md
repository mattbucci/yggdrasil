# Design — `ygg scheduler` daemon

> **Superseded in part by the SQLite port (2026-07):** the daemon now runs on embedded SQLite — the advisory lock became an exclusive flock() on a lock file beside the DB, `FOR UPDATE SKIP LOCKED` became an atomic `ready → running` UPDATE in a `BEGIN IMMEDIATE` transaction, and LISTEN/NOTIFY became a unix-datagram socket bus (`$XDG_RUNTIME_DIR/ygg.sock`). The tick structure and stage ordering below are unchanged.

> Single authoritative process that advances the task DAG. Companion to [ADR 0016](../adr/0016-autonomous-execution.md), [Orchestration runtime](../orchestration.md), and [Task-runs data model](task-runs.md).

## Scope

One long-lived process per host. Rust. Reads Postgres. Writes Postgres. Calls `ygg spawn`. Makes no LLM calls. Target size: ~300 LOC in `src/scheduler.rs` + ~200 LOC of repo helpers + ~150 LOC of CLI wiring.

The scheduler does exactly these things and nothing else:

1. Claims ready `task_runs` via `SELECT … FOR UPDATE SKIP LOCKED`.
2. Spawns agents via existing `ygg spawn`.
3. Reconciles terminal runs (reads hook-written outcome, transitions state, releases locks, unblocks downstream).
4. Reaps heartbeat-expired runs to `crashed`.
5. Retries failed/crashed runs per backoff policy, up to `max_attempts`.
6. Enforces deadlines by force-cancelling overdue runs.
7. Detects loops via fingerprint and escalates to `poison`.
8. Respects concurrency budgets (per-host, per-repo, per-epic).
9. Emits events (`run_scheduled`, `run_claimed`, `run_terminal`, `run_retry`, `scheduler_tick`).

Non-goals:

- LLM calls (never)
- Task content reasoning ("should this task be split?") — that's the planner agent's job
- Workflow DSL interpretation (workflows are rows, not code)
- Cross-host distribution (one scheduler per host; advisory-lock-guarded)

## Why one scheduler (not per-agent, not per-repo)

Every distributed-state-write orchestration system we surveyed eventually breaks on reconciliation drift. Centralizing the writer is cheaper, easier to reason about, and matches DBOS's "any app server can be a worker, but state transitions are serialized through Postgres."

A single scheduler does not mean a single point of failure. The scheduler is **idempotent on restart** — state lives on rows, not in memory. A stopped scheduler pauses advancement but doesn't corrupt anything. Locks continue to time out via the watcher. Agents continue to run. Restarting the scheduler picks up where it left off.

Safety: startup acquires `pg_try_advisory_lock(SCHEDULER_LOCK_ID)`. Second instance on the same DB exits with a visible "another scheduler is running" error. The lock releases on connection close; no stuck-lock risk on crash.

## Tick loop

```mermaid
flowchart TD
    Start([scheduler.run]) --> Lock{advisory lock<br/>acquired?}
    Lock -- no --> Exit2([exit: another scheduler is running])
    Lock -- yes --> Listen[LISTEN task_events / lock_events]
    Listen --> Wait{wake?}
    Wait -- NOTIFY OR tick interval --> Tick
    Wait -- ctrl-c --> Cleanup[release advisory lock]

    subgraph Tick["tick()"]
        direction TB
        S1[reap_expired_heartbeats<br/>running → crashed]
        S2[enforce_deadlines<br/>running → cancelled]
        S3[detect_loops<br/>fingerprint match → poison]
        S4[finalize_terminal_runs<br/>terminal run + open task → close task]
        S5[schedule_retries<br/>failed/crashed + backoff → new attempt]
        S6[schedule_ready_tasks<br/>runnable && deps clear → ready run]
        S7[dispatch_ready<br/>SKIP LOCKED claim + ygg spawn]
        S1 --> S2 --> S3 --> S4 --> S5 --> S6 --> S7
    end

    Tick --> Emit[emit scheduler_tick event]
    Emit --> Wait
    Cleanup --> Done([exit])
```

Step order matches `src/scheduler.rs::tick`. Reap → deadlines first so
terminals exist before `detect_loops` backfills fingerprints and poisons
matching tasks; `finalize_terminal_runs` then closes any open task whose
latest run is terminal (including the just-poisoned ones); retries dispatch
after that.

```rust
const SCHEDULER_LOCK_ID: i64 = 0x4547_47_5343_48;  // "YGG SCH"

pub async fn run(pool: PgPool, cfg: SchedulerConfig) -> Result<()> {
    let _guard = acquire_advisory_lock(&pool, SCHEDULER_LOCK_ID).await?;

    let mut listener = PgListener::connect_with(&pool).await?;
    listener.listen("task_events").await?;
    listener.listen("lock_events").await?;

    loop {
        tokio::select! {
            _ = listener.try_recv() => {},
            _ = tokio::time::sleep(cfg.tick_interval) => {},
            _ = shutdown_signal() => break,
        }

        if let Err(e) = tick(&pool, &cfg).await {
            tracing::warn!(error = %e, "scheduler tick failed");
            record_event(&pool, EventKind::SchedulerError, e.to_string()).await.ok();
            // Don't exit; next tick will retry.
        }
    }

    Ok(())
}

async fn tick(pool: &PgPool, cfg: &SchedulerConfig) -> Result<TickStats> {
    let mut stats = TickStats::default();

    // (1) Finalize terminal runs: outcome written by Stop hook, state not yet advanced.
    stats.finalized = finalize_terminal_runs(pool).await?;

    // (2) Reap heartbeat-expired running runs.
    stats.reaped = reap_expired_heartbeats(pool, cfg).await?;

    // (3) Enforce deadlines on running runs.
    stats.deadlined = enforce_deadlines(pool).await?;

    // (4) Schedule retries for failed/crashed runs whose backoff elapsed.
    stats.retried = schedule_retries(pool, cfg).await?;

    // (5) Surface loops → poison.
    stats.poisoned = detect_loops_and_poison(pool, cfg).await?;

    // (6) Dispatch: claim ready runs up to budget, spawn agents.
    let budget = concurrency_budget(pool, cfg).await?;
    stats.dispatched = dispatch_ready(pool, cfg, budget).await?;

    emit_tick_event(pool, &stats).await.ok();
    Ok(stats)
}
```

Each step is a separate function with a single SQL query (plus maybe one follow-up). The whole tick is 6 round-trips in the steady state, not counting spawn. Target: <100ms per tick on an idle DB with <1000 task_runs rows.

## The six steps in detail

### (1) Finalize — the Stop hook → scheduler handoff

The `Stop` hook writes outcome fields on a run row but does **not** change `state`. The scheduler is the only writer of `state`. This split gives us one consumer of hook output (the reconciler), not many, and keeps state transitions auditable.

```sql
-- Runs whose agent stopped but state is still 'running'. The Stop hook
-- sets ended_at and outcome fields when the agent exits.
SELECT run_id, task_id, attempt, output, error, output_commit_sha, reason
FROM task_runs
WHERE state = 'running' AND ended_at IS NOT NULL
FOR UPDATE SKIP LOCKED
LIMIT 100;
```

For each row, the scheduler computes the terminal state:

```
- if error IS NULL and output IS NOT NULL       → 'succeeded'
- if error.reason_code IN ('agent_error',…)     → 'failed'
- if error.reason_code = 'user_cancelled'       → 'cancelled'
- otherwise (shouldn't happen)                   → 'crashed'
```

and writes it in a single UPDATE, fingerprint computed on the side. Emits `run_terminal` event. If the run succeeded and the task had no live children, closes the task. If the task has live children, moves it to `awaiting_children`. If the task is in a DAG, NOTIFY wakes up the next tick to re-check downstream.

### (2) Reap — heartbeat-expired runs

```sql
UPDATE task_runs
SET state = 'crashed',
    reason = 'heartbeat_timeout',
    ended_at = now()
WHERE state = 'running'
  AND heartbeat_at IS NOT NULL
  AND heartbeat_at < now() - (heartbeat_ttl_s || ' seconds')::interval
RETURNING run_id, task_id, agent_id, worker_id;
```

Returns rows for event emission. The existing watcher's `release_all_for_agent` is called for each agent_id touched (or we rely on the watcher's own tick to handle it — cheaper: piggyback).

Heartbeat is bumped by the `PreToolUse` hook on every tool call the agent makes. Default TTL 300s. A Claude agent thinking for 5 uninterrupted minutes without calling a tool is suspicious, so this is a reasonable ceiling.

### (3) Deadlines

```sql
UPDATE task_runs
SET state = 'cancelled',
    reason = 'timeout',
    ended_at = now()
WHERE state = 'running'
  AND deadline_at IS NOT NULL
  AND deadline_at < now()
RETURNING run_id, task_id, agent_id;
```

For each row: tmux kill the agent's window (via `src/tmux.rs`), release locks, emit event. Forceful. Logged loudly. The deadline is an escape hatch; normal runs should complete within their default `timeout_ms`.

### (4) Retries

```sql
-- Candidate failed/crashed runs where retry window elapsed.
SELECT r.run_id, r.task_id, r.attempt, r.reason, r.retry_strategy,
       r.input, r.output, r.error, r.ended_at, r.max_attempts,
       t.approval_level, t.approved_at
FROM task_runs r
JOIN tasks t USING (task_id)
WHERE r.state IN ('failed', 'crashed')
  AND r.attempt < r.max_attempts
  AND r.ended_at + compute_backoff(r.retry_strategy, r.attempt) < now()
  -- Skip tasks already retrying (i.e., a successor row exists).
  AND NOT EXISTS (
      SELECT 1 FROM task_runs r2
      WHERE r2.task_id = r.task_id AND r2.attempt = r.attempt + 1
  )
FOR UPDATE OF r SKIP LOCKED
LIMIT 100;
```

For each: transition the old row to `retrying` (transient bridge), insert a new `scheduled` row with `parent_run_id = old`, `attempt = old.attempt + 1`, input populated to carry the previous attempt's error as `input.previous_attempt`. NOTIFY so the next tick considers it for dispatch.

`compute_backoff` in Rust:

```rust
fn compute_backoff(strategy: &RetryStrategy, attempt: i32) -> Duration {
    match strategy.kind {
        RetryKind::Exponential => {
            let base = strategy.base_ms;
            let cap  = strategy.cap_ms;
            let exp  = base.saturating_mul(1 << (attempt.max(0).min(20) as u64 - 1));
            let raw  = exp.min(cap);
            if strategy.jitter { jittered(raw) } else { raw }
        }
        RetryKind::Fixed => Duration::from_millis(strategy.base_ms),
    }
}
```

### (5) Loop detection

Fingerprint is computed on run finalize (step 1). Loops happen when the same (task, error pattern) recurs. Poison conditions:

- N ≥ 3 consecutive terminal runs with the same fingerprint on the same task
- Total attempts ≥ `max_attempts`

```sql
UPDATE tasks
SET status = 'blocked',
    close_reason = 'poison: ' || $1
WHERE task_id = $2 AND status NOT IN ('closed', 'cancelled');

UPDATE task_runs
SET state = 'poison', reason = 'loop_detected'
WHERE task_id = $2 AND state IN ('failed', 'crashed');
```

Emits `run_terminal` with `state=poison`. Dashboard shows poisoned tasks distinctly; human runs `ygg task unpoison <ref>` to reset attempts and try again.

### (6) Dispatch

The hot query. Already detailed in [task-runs.md § the SKIP LOCKED claim query](task-runs.md#the-skip-locked-claim-query). After claim:

```rust
for claim in claimed {
    // Acquire required locks if any. If conflict, release claim, leave in 'ready'.
    if !acquire_required_locks(&claim).await? {
        release_claim(&claim).await?;  // move back to 'ready'
        continue;
    }

    // Spawn via existing ygg spawn subprocess. The spawned agent's
    // SessionStart hook reads YGG_RUN_ID env var to know which run it's
    // bound to, and calls `ygg run claim` to confirm.
    let spawn_result = spawn_for_run(&claim).await?;

    // Persist binding; transition to 'running'; emit event.
    bind_run_to_agent(&claim, &spawn_result).await?;
}
```

Concurrency budget:

```rust
async fn concurrency_budget(pool: &PgPool, cfg: &SchedulerConfig) -> Result<i64> {
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM task_runs WHERE state = 'running'"
    ).fetch_one(pool).await?;
    Ok((cfg.max_concurrent as i64 - live).max(0))
}
```

Per-repo and per-epic budgets layer on top (subquery in the ready scan). Details in the migration tasks.

## Config

Loaded from env + optional `~/.config/yggdrasil/scheduler.toml`:

```toml
[scheduler]
tick_interval_ms       = 2000
max_concurrent_agents  = 6        # global cap
max_agents_per_repo    = 3
max_agents_per_epic    = 3
default_max_attempts   = 3
default_timeout_ms     = 3600000  # 1 hour
heartbeat_ttl_s        = 300

[scheduler.retry]
kind     = "exponential"
base_ms  = 60000
cap_ms   = 600000
jitter   = true
```

Env overrides: `YGG_SCHEDULER_TICK_MS`, `YGG_SCHEDULER_MAX_CONCURRENT`, `YGG_AUTO_APPROVE` (bypasses `approve_plan` gates — emergency), `YGG_SCHEDULER_DRY_RUN` (print decisions, don't spawn).

## CLI surface

```
ygg scheduler run                 # run the daemon in foreground
ygg scheduler status              # print current stats, last tick, lock state
ygg scheduler tick                # run one tick synchronously (for testing)
ygg scheduler dry-run             # print what would happen, don't act
```

Integration with existing `ygg up`: `ygg up --with-scheduler` opens a dedicated tmux window running `ygg scheduler run`. Default off; humans opt in.

## Observability

The event stream is the primary surface.

- `scheduler_started` — daemon came up, advisory lock acquired
- `scheduler_stopped` — graceful shutdown
- `scheduler_tick` — periodic summary: `{ finalized, reaped, deadlined, retried, poisoned, dispatched }` (suppressed by default; `--verbose` enables)
- `scheduler_error` — tick failed; payload includes error chain
- `run_scheduled` — new run row inserted by scheduler
- `run_claimed` — scheduler transitioned a ready run to running
- `run_terminal` — scheduler finalized a run, payload `{state, reason, commit_sha?, pr_url?}`
- `run_retry` — scheduler inserted a retry successor

Dashboard's new "Scheduler" tile polls the latest `scheduler_tick` event every few seconds and shows:

```
 Scheduler (PID 12345)           tick +1.3s ago (wake: NOTIFY)
   running:   4 / 6     repos: 3 active     ready: 2 queued
   last:      spawned task-142 on agent-7
   retries:   0 in last 5m      poisoned: 0
```

## Failure modes to handle correctly

| Failure | Handling |
|---|---|
| Scheduler restart mid-tick | Advisory lock auto-releases on conn close; next scheduler takes over; all state on rows, no in-memory recovery needed |
| Postgres restart | `PgListener` reconnects; tick resumes; partial transactions rolled back by Postgres itself |
| Agent tmux window killed externally | `run.heartbeat_at` stops advancing; step (2) crashes the run on TTL expiry |
| Agent hung mid-turn (no tool calls) | Same as above; heartbeat TTL catches it |
| Hook failed to write outcome | Run stuck in `running` past heartbeat → step (2) crashes it; retry (step 4) handles re-dispatch |
| Lock acquired but spawn failed | `bind_run_to_agent` is atomic with state transition; if spawn fails, state stays `ready`, locks released in same txn |
| Two schedulers accidentally started | Second one fails to acquire advisory lock and exits; no double-writes possible |
| Concurrent `ygg task close` by human during scheduler dispatch | Status-check in the dispatch query guards: `WHERE t.status NOT IN ('closed','cancelled')`; race loser just transitions to `cancelled` |
| NOTIFY message dropped | Tick interval catches any missed work within `tick_interval` seconds |

## What about multi-host?

Out of scope for MVP. Yggdrasil is a developer tool; one host per developer is the realistic deployment.

If it ever matters: advisory lock prevents accidental concurrent schedulers, but a true multi-host scheduler design would need leader election and repo-sharded tick loops. Hatchet's architecture is the closest reference and would be the starting point for that ADR, not this one.

## Ordering of work

Four stages, each shippable:

**Stage 1 — MVP dispatch (week 1).**
Steps (1), (6). `dispatch_ready` + `finalize_terminal_runs`. No retry, no deadline, no loop detection. Per-host concurrency budget only.
Unblocks: running a simple DAG autonomously.

**Stage 2 — Resilience (week 2).**
Steps (2), (3), (4). Heartbeat reap + deadline enforcement + retry math.
Unblocks: Scenario 5 (failure-recovery) in the eval suite.

**Stage 3 — Safety (week 3).**
Step (5) + per-repo/per-epic budgets + fingerprint. `ygg task unpoison`.
Unblocks: long-running autonomous operation without cost runaway.

**Stage 4 — UX (week 4).**
Dashboard Scheduler tile + Runs pane + `ygg scheduler status` polish + `--dry-run` mode + docs.
Unblocks: a human being able to operate the scheduler without reading source.

Each stage is a PR. Each PR is shippable on its own.

## Testing

- **Unit** — every function in `src/scheduler.rs` has a test against an ephemeral Postgres (`sqlx::test`).
- **Integration** — `tests/scheduler.rs` starts a real scheduler, inserts fixture tasks, asserts state transitions. Uses a minimal `claude -p` stand-in (bash script) instead of real Claude to keep CI cheap.
- **Bench** — `ygg bench run independent-parallel-n --tier smoke` is the scheduler's acceptance gate. Runs in <3 min with real Claude. Runs nightly on main.

## Open questions

- Should the scheduler be able to preempt a running task if a higher-priority task arrives? Default no (kills in-flight work); add `preempt_on_priority_n` opt-in per task later.
- Should the scheduler handle cross-repo tasks centrally, or delegate per-repo? Central for MVP; per-repo routing is a later optimization.
- Should the "budget" be expressible in dollars (`budget_usd`) rather than only concurrency? Yes, but punt to Stage 3 — need the metric plumbing first.
- How aggressive should the scheduler be about reading task output for *useful* downstream inputs? MVP: just the schema-mandated `upstream[]` array. Later: classifier/summarizer (outside the hot loop).

## Sources

- DBOS `SELECT … FOR UPDATE SKIP LOCKED` pattern on status table: [DBOS Architecture](https://docs.dbos.dev/architecture)
- Hatchet dispatch design: [Hatchet Architecture](https://docs.hatchet.run/home/architecture)
- Dagster daemon + tick pattern: [Dagster Sensors](https://docs.dagster.io/guides/automate/sensors)
- Prefect heartbeat + crashed-vs-failed distinction: [Prefect States](https://docs.prefect.io/v3/concepts/states)
- Postgres advisory locks for singleton workers: pg docs
- LISTEN/NOTIFY semantics and message-drop caveats: pg docs
