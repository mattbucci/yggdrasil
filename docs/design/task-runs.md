# Design — `task_runs` data model

> **Superseded in part by the SQLite port (2026-07):** the store is now embedded SQLite. The `SKIP LOCKED` claim query below became an atomic `ready → running` UPDATE in a `BEGIN IMMEDIATE` transaction; LISTEN/NOTIFY became a unix-datagram socket bus; the advisory lock became a file lock. Table shapes and state machine are unchanged (UUIDs stored as TEXT, timestamps as RFC 3339 TEXT, JSONB as JSON TEXT).

> One row per execution attempt of a task. DBOS-shaped checkpoint model. Companion to [ADR 0016](../adr/0016-autonomous-execution.md) and [Orchestration runtime](../orchestration.md).

## Why a new table (not columns on `tasks`)

`tasks` is the semantic unit: "title, acceptance, deps, kind, priority, status." It describes *what* to do.

`task_runs` is the execution unit: "input, output, error, state, attempt, heartbeat, agent binding." It describes a *particular doing* of a task.

A task can have many runs. Most will have one. Some will have three (attempt 1 failed, 2 crashed, 3 succeeded). Some will have zero (closed without ever running — manually marked, or cancelled).

Overloading `tasks.status` with execution states (failed, crashed, retrying) would muddy the semantic lifecycle that users already understand. Keeping them separate mirrors DBOS's `workflow_status` + `operation_outputs` split, which has been battle-tested for exactly this need.

## Schema

### Enums

```sql
-- Run-level state machine. Distinct from task.status (which stays semantic).
CREATE TYPE run_state AS ENUM (
    'scheduled',   -- row written; deps may or may not be satisfied
    'ready',       -- deps satisfied, eligible for claim
    'running',     -- claimed by scheduler, agent spawned, heartbeating
    'succeeded',   -- terminal: output stored, downstream may advance
    'failed',      -- terminal: agent reported semantic failure (tests red, acceptance unmet)
    'crashed',     -- terminal: infra failure (tmux gone, heartbeat expired, killed)
    'cancelled',   -- terminal: user/scheduler interrupted before completion
    'retrying',    -- transient bridge: failed/crashed run whose successor is scheduled
    'poison'       -- terminal: max_attempts exhausted; requires human 'unpoison'
);

-- Machine-readable reason. Free-text detail lives in error.message.
CREATE TYPE run_reason AS ENUM (
    'ok',
    'agent_error',          -- agent exited non-zero or reported failure
    'heartbeat_timeout',    -- worker stopped heartbeating past ttl
    'tmux_gone',            -- tmux window disappeared
    'max_attempts',         -- reached max_attempts, moving to poison
    'user_cancelled',
    'dependency_failed',    -- upstream never produced required output
    'lock_conflict',        -- couldn't acquire required lock within grace window
    'timeout',              -- wall-clock timeout exceeded
    'loop_detected',        -- fingerprint of run matches N prior terminals; stop spawning
    'budget_exceeded'       -- per-epic budget_usd cap hit
);
```

### `task_runs`

```sql
CREATE TABLE task_runs (
    run_id          UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    task_id         UUID NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    attempt         INT  NOT NULL,                           -- 1-based; UNIQUE with task_id
    parent_run_id   UUID REFERENCES task_runs(run_id),       -- retry links back to predecessor

    -- Structural idempotency key. Derived from (task_id, attempt). Never
    -- derived from agent output — that's non-deterministic. See
    -- `docs/orchestration.md` on idempotency.
    idempotency_key TEXT NOT NULL,

    state           run_state  NOT NULL DEFAULT 'scheduled',
    reason          run_reason NOT NULL DEFAULT 'ok',

    -- Scheduling / assignment timeline.
    scheduled_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at      TIMESTAMPTZ,     -- scheduler picked it off 'ready'
    started_at      TIMESTAMPTZ,     -- agent session start
    ended_at        TIMESTAMPTZ,     -- terminal transition
    heartbeat_at    TIMESTAMPTZ,     -- PreToolUse hook bumps this
    heartbeat_ttl_s INT NOT NULL DEFAULT 300,  -- crashed if heartbeat_at + ttl < now

    -- Execution binding. Nullable on pre-spawn rows.
    agent_id        UUID REFERENCES agents(agent_id),
    worker_id       UUID REFERENCES workers(worker_id),
    session_id      UUID REFERENCES sessions(session_id),

    -- Retry metadata.
    max_attempts    INT NOT NULL DEFAULT 3,
    retry_strategy  JSONB NOT NULL DEFAULT '{
        "kind": "exponential",
        "base_ms": 60000,
        "cap_ms": 600000,
        "jitter": true
    }',
    deadline_at     TIMESTAMPTZ,     -- per-run wall-clock deadline; null = no deadline

    -- Payloads. Inline-small discipline enforced in Rust: reject > 16 KiB.
    input           JSONB NOT NULL DEFAULT '{}',  -- see payload schemas below
    output          JSONB,                         -- null until terminal
    error           JSONB,                         -- structured error when terminal != succeeded

    -- Large-payload references. The code-producing case; the scheduler
    -- treats a row with only a commit_sha as fully-populated output.
    output_commit_sha   TEXT,
    output_branch       TEXT,
    output_pr_url       TEXT,
    output_worktree     TEXT,
    output_blob_ref     TEXT,         -- sha256 of .ygg/blobs/<hash> file

    -- Loop-detection fingerprint. Computed by scheduler on finalize:
    -- sha256(task.title || task.acceptance || error.reason || error.message_prefix).
    -- If the same fingerprint appears N times on the same task_id across
    -- consecutive terminal attempts, scheduler poisons with reason=loop_detected.
    fingerprint     TEXT,

    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (task_id, attempt),
    UNIQUE (idempotency_key),
    CHECK (attempt >= 1),
    CHECK (max_attempts >= 1)
);

-- Hot path: scheduler scans ready runs ordered by task priority + scheduled age.
CREATE INDEX idx_runs_ready
    ON task_runs (scheduled_at)
    WHERE state = 'ready';

-- Heartbeat reconciler: runs with expired heartbeats need crashing.
CREATE INDEX idx_runs_live_heartbeat
    ON task_runs (heartbeat_at)
    WHERE state = 'running';

-- Retry math.
CREATE INDEX idx_runs_retrying
    ON task_runs (ended_at)
    WHERE state IN ('failed','crashed');

-- Deadline enforcement.
CREATE INDEX idx_runs_deadline
    ON task_runs (deadline_at)
    WHERE state = 'running' AND deadline_at IS NOT NULL;

-- Per-task history readout for `ygg task show`.
CREATE INDEX idx_runs_task     ON task_runs (task_id, attempt DESC);
CREATE INDEX idx_runs_agent    ON task_runs (agent_id, started_at DESC) WHERE agent_id IS NOT NULL;
CREATE INDEX idx_runs_worker   ON task_runs (worker_id) WHERE worker_id IS NOT NULL;
```

Deliberately **not** creating separate `task_inputs` / `task_outputs` tables. DBOS fits input/output on the status row; we do too. Hot-path joins across three tables would hurt the scheduler tick.

### Changes to `tasks`

```sql
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS runnable             BOOLEAN  NOT NULL DEFAULT FALSE;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS current_attempt_id   UUID REFERENCES task_runs(run_id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS max_attempts         INT      NOT NULL DEFAULT 3;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS timeout_ms           BIGINT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS deadline_at          TIMESTAMPTZ;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS approval_level       TEXT     NOT NULL DEFAULT 'auto';
   -- 'auto' | 'approve_plan' | 'approve_completion'
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS approved_at          TIMESTAMPTZ;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS approved_by_agent_id UUID REFERENCES agents(agent_id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS parent_task_id       UUID REFERENCES tasks(task_id);
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS input_spec           JSONB    NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS output_spec          JSONB    NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS agent_role           TEXT;  -- 'planner'|'executor'|'critic'
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS required_locks       TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS result_blob_ref      TEXT;  -- final blob sha
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS plan_strategy        TEXT;  -- 'llm' for epic-planner opt-in

-- Extend status enum with the dynamic-child state.
ALTER TYPE task_status ADD VALUE IF NOT EXISTS 'awaiting_children';
ALTER TYPE task_status ADD VALUE IF NOT EXISTS 'awaiting_approval';
ALTER TYPE task_status ADD VALUE IF NOT EXISTS 'awaiting_review';

-- Runnable + unblocked + not-in-a-live-attempt = scheduler-eligible.
CREATE INDEX IF NOT EXISTS idx_tasks_runnable
    ON tasks (repo_id, priority, updated_at)
    WHERE runnable = TRUE AND status IN ('open', 'in_progress');

CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks (parent_task_id)
    WHERE parent_task_id IS NOT NULL;
```

Existing `tasks.human_flag` becomes redundant with `approval_level`. Leave it in place during migration; deprecate via a comment; remove in a later migration once no caller reads it.

### `task_runs_events` — audit trail for state transitions

Already exists: `task_events` per-task. Extend it with `run_id` to attribute transitions. New event kinds:

- `run_scheduled` — new row inserted
- `run_claimed` — scheduler picked a ready run, transitioned to running
- `run_heartbeat` — (suppressed by default; trace-only)
- `run_terminal` — run reached terminal state, payload = `{state, reason, commit_sha, pr_url, ...}`
- `run_retry` — new attempt inserted, payload = `{parent_run_id, backoff_ms}`

```sql
ALTER TABLE task_events ADD COLUMN IF NOT EXISTS run_id UUID REFERENCES task_runs(run_id);
CREATE INDEX IF NOT EXISTS idx_task_events_run ON task_events (run_id) WHERE run_id IS NOT NULL;
```

## Payload formats

Versioned. `schema` is the first key of every payload; parsers must tolerate future minor versions.

### Input (`task_runs.input`)

```json
{
  "schema": "ygg.run.input.v1",

  "task_ref": "yggdrasil-142",
  "title": "Add retry to lock acquire",
  "acceptance": "Exponential backoff, 3 attempts, unit tests.",

  "worktree": "/Users/ng/worktrees/yggdrasil-142",
  "base_branch": "main",
  "branch_name": "jon/yggdrasil-142-lock-retry",

  "required_locks": ["src/lock.rs"],

  "upstream": [
    {
      "task_ref": "yggdrasil-138",
      "run_id": "b2c1c0f5-…",
      "state": "succeeded",
      "commit_sha": "abc123def456…",
      "pr_url": "https://github.com/.../pull/203",
      "summary": "scheduler groundwork"
    }
  ],

  "previous_attempt": {
    "run_id": "a1b2c3d4-…",
    "state": "failed",
    "reason": "agent_error",
    "error_hint": "cargo test failed in src/lock.rs: test_retry_backoff",
    "partial_commits": ["aaa111…"]
  },

  "context": {
    "claude_md_extra": "Use cargo check --all-targets before finishing",
    "env": { "YGG_DB": "postgres://..." }
  },

  "budgets": {
    "max_wall_clock_s": 3600,
    "max_tool_calls": 200
  },

  "approval_level": "auto",
  "agent_role": "executor"
}
```

### Output (`task_runs.output`)

```json
{
  "schema": "ygg.run.output.v1",

  "summary": "Added exponential retry to lock.acquire; 4 new tests pass",

  "commits": [
    {
      "sha": "def456…",
      "branch": "jon/yggdrasil-142-lock-retry",
      "message": "lock: retry with exponential backoff",
      "files_changed": 3,
      "insertions": 87,
      "deletions": 12
    }
  ],

  "delivery": {
    "branch_pushed": true,
    "pr_url": "https://github.com/.../pull/204",
    "pr_number": 204,
    "merged": false
  },

  "spawned_tasks": [
    { "task_ref": "yggdrasil-143", "title": "retry backoff config", "relation": "child" }
  ],

  "learnings": [
    {
      "kind": "gotcha",
      "text": "locks.heartbeat_at must be bumped inside the retry loop, not outside.",
      "file_glob": "src/lock.rs"
    }
  ],

  "locks_held": [
    { "resource_key": "src/lock.rs", "acquired_at": "...", "released_at": "..." }
  ],

  "metrics": {
    "tool_calls": 42,
    "wall_clock_s": 812,
    "input_tokens": 38210,
    "output_tokens": 4120,
    "cache_read_tokens": 120000,
    "usd": 0.87
  }
}
```

### Error (`task_runs.error`, populated when state ∈ {failed, crashed, poison})

```json
{
  "schema": "ygg.run.error.v1",
  "reason_code": "agent_error",
  "message": "Claude reported: 2 tests failed in src/lock.rs",
  "detail": "running test_retry_backoff ... FAILED\n...",
  "stderr_tail": "...",
  "last_tool": "Bash",
  "partial_commits": [
    { "sha": "aaa111…", "message": "WIP: wrong fix", "reverted": false }
  ],
  "recoverable": true,
  "recommended_action": "retry_with_hint",
  "hint": "Previous attempt landed on wrong file; see src/lock.rs:142"
}
```

## Blob store

Content-addressed filesystem store at `.ygg/blobs/` under the Yggdrasil home (same directory as `ygg init` places its artifacts). Subdir fanout: `.ygg/blobs/ab/cd/ef123…` (sha256 hash, first 2/2 chars as dirs). Familiar pattern from git's `.git/objects/`.

Rust API sketch (to land in `src/blob.rs`):

```rust
pub struct BlobStore { root: PathBuf }

impl BlobStore {
    pub fn put(&self, bytes: &[u8]) -> Result<BlobRef>;   // sha256 + write
    pub fn get(&self, r: &BlobRef) -> Result<Vec<u8>>;
    pub fn stat(&self, r: &BlobRef) -> Result<BlobStat>;  // size, mtime
    pub fn gc(&self, keep: &HashSet<BlobRef>) -> Result<usize>;
}

pub struct BlobRef(String);  // sha256 hex
```

What goes in blobs:
- tool_use transcripts too large for inline JSONB
- test-output bundles
- analysis documents that aren't git-tracked
- agent-produced artifacts that aren't code

What does *not* go in blobs:
- code changes (use commit SHAs)
- small structured metadata (inline JSONB)
- prompts (already in `nodes`/session log; don't duplicate)

GC policy: `ygg reap --blobs --before <days>` walks `task_runs` for blob refs, deletes unreferenced files. Add to the existing `watcher` tick after a stable period.

## Idempotency

The formula, copied from production-agent best practices and echoed across all surveyed systems:

```
idempotency_key = "run:" || task_id || ":attempt:" || attempt
```

Structural. Derived from data fixed *before* the LLM runs. Never derived from the LLM's output.

For side effects during a run (branch push, PR open, task spawn, memory write), the receiving system's native idempotency is used where supported (GitHub PRs have deterministic titles we can set to include `[run-<run_id>]`; `ygg task create` already dedupes on embedding similarity). Where no native idempotency exists, the run row *is* the idempotency record — the agent checks `output.delivery.pr_url` before opening a new PR within the same run.

A retried attempt gets a new `run_id` and a new `attempt` number, hence a new idempotency key. The retry deliberately gets a fresh side-effect identity because its purpose is to re-do the work. The previous attempt's partial side effects are recorded in `error.partial_commits` so the new attempt can clean up if needed.

## The `SKIP LOCKED` claim query

The single hottest query. Must stay fast.

```sql
-- The one-liner that makes Postgres a queue.
WITH claimed AS (
    SELECT tr.run_id
    FROM task_runs tr
    JOIN tasks t USING (task_id)
    WHERE tr.state = 'ready'
      AND t.repo_id = $1                       -- scheduler can scope to a repo
      AND NOT EXISTS (
          SELECT 1 FROM task_deps d
          JOIN tasks bt ON bt.task_id = d.blocker_id
          WHERE d.task_id = tr.task_id
            AND bt.status <> 'closed'
      )
      AND (t.approval_level = 'auto' OR t.approved_at IS NOT NULL)
    ORDER BY t.priority, tr.scheduled_at
    LIMIT $2                                    -- N = budget_remaining
    FOR UPDATE OF tr SKIP LOCKED
)
UPDATE task_runs
   SET state       = 'running',
       claimed_at  = now(),
       started_at  = now(),
       heartbeat_at= now(),
       updated_at  = now()
 WHERE run_id IN (SELECT run_id FROM claimed)
 RETURNING run_id, task_id, input, agent_id;
```

Single transaction, race-free, safe under concurrent schedulers. DB Pro, Inferable, and Neon all report ~1800 jobs/sec on modest Postgres hardware for this exact pattern, which is wildly more headroom than Yggdrasil needs.

`LISTEN` for `task_events` on the scheduler side gives sub-second wake-up for newly-ready work. `NOTIFY` emitted by run-state transitions, task-closes, and lock-releases. Channel payload: the `task_id` or `run_id` so the scheduler can tick-scope its subsequent scan.

## Retention & GC

- `task_runs` — indefinite. Cheap enough; retain for audit. If pressured in months, move terminal runs > 1 year old to `task_runs_archive` table.
- `task_events` — 180 days (already in ADR 0015-adjacent discussions, formalized here). Partition by month via `pg_partman`, drop oldest monthly.
- `blobs` — `ygg reap --blobs --before 30d`. Orphans only.
- `events` — 30 days, partitioned by day.

These are retention goals, not urgent constraints. None of them are hot for the MVP.

## Migration path

**M1 — schema additions (reversible, zero-behavior-change).**

```
20260424000001_run_enums.sql        -- run_state, run_reason
20260424000002_task_runs.sql        -- task_runs table + indexes
20260424000003_tasks_exec_columns.sql -- ALTER tasks ADD COLUMN …
20260424000004_task_events_run_id.sql -- ALTER task_events ADD COLUMN run_id
```

Each migration is a pure additive schema change. `runnable` defaults to FALSE, so nothing auto-schedules until you flip it. Existing code compiles; existing behavior unchanged.

**M2 — backfill (idempotent script).**

```
for task in tasks.status = 'in_progress':
    if exists live worker for task:
        upsert task_runs(task_id=task.id, attempt=1, state='running',
                         agent_id=worker.agent_id, worker_id=worker.id,
                         started_at=worker.started_at, heartbeat_at=worker.last_seen_at)

for task in tasks.status = 'closed':
    if workers had a branch_pushed record:
        upsert task_runs(attempt=1, state='succeeded',
                         output_commit_sha=last_commit_on_branch,
                         ended_at=task.closed_at)
```

Idempotent. Running twice is safe.

**M3 — CLI writes runs (manual-mode parity).**

- `ygg task claim` inserts a run, state=running.
- `ygg task close` finalizes the current run.
- `ygg spawn` pre-inserts a scheduled run, the spawned agent claims it via `ygg run claim` on start.
- `ygg task show` prints the run history table.

No scheduler yet. Human drivers continue to operate exactly as today.

**M4 — ship `ygg scheduler`.**

Flag-gated. `runnable=TRUE` per task is opt-in. Daemon runs as `ygg scheduler --tick 2s` in its own tmux window. Dashboard gets the Runs pane.

**M5 — flip default `runnable=TRUE` for new tasks of kind task/bug/feature/chore.**

Epics stay manual unless `plan_strategy='llm'` is set. Behavior flip, not schema. Reversible by toggling the default back.

**M1–M5 are all reversible.** Backing out M1 means dropping the new table/columns — safe if no one's written to them yet. Backing out M4 means `pkill ygg-scheduler`. There is no irreversible step.

## Risks

- **JSONB payloads crossing TOAST threshold (~2 KiB).** Mitigation: Rust-side size guard; anything > 16 KiB must go to blob store. Enforced in the repo helper.
- **Partial side effects on crash.** Mitigation: `output_commit_sha` and `output_branch` are written *before* marking terminal; the reconciler verifies remote state. For `failed` state, partial commits are recorded on the error row so the retry can see them.
- **Clock skew for heartbeat timeouts.** Mitigation: all `heartbeat_at` writes use server-side `now()`, not client-supplied timestamps.
- **Retry storm.** Mitigation: `max_attempts` defaults to 3; `poison` state surfaces to human; fingerprint-based loop detection before the max is hit.
- **Concurrent schedulers.** Should be exactly one per host. Enforced by a Postgres advisory lock acquired at startup (`pg_try_advisory_lock(SCHEDULER_LOCK_ID)`). Second instance exits cleanly with a visible error.

## Open questions

- Should fingerprint computation include last-tool used? (Heuristic: same tool + same error pattern = same bug.) Lean yes; defer until we see real loops.
- Should `required_locks` be declared on the task or auto-learned from previous runs' `locks_held`? Declared MVP; auto-learning is a later classifier.
- Should `retry_strategy` be per-task-kind default (bugs retry more, epics not at all)? Probably yes, as a follow-up table.
- Does the blob store need network-visible storage for multi-host deployments? Not now — Yggdrasil is single-host. A future ADR if multi-host becomes real.

## Sources

DBOS schema modeled most closely here:
- [DBOS System Tables](https://docs.dbos.dev/explanations/system-tables)
- [Why Workflows Should Be Postgres Rows](https://www.dbos.dev/blog/why-workflows-should-be-postgres-rows)

`SKIP LOCKED` pattern:
- [The Unreasonable Effectiveness of SKIP LOCKED (Inferable)](https://www.inferable.ai/blog/posts/postgres-skip-locked)
- [DB Pro SKIP LOCKED benchmark](https://www.dbpro.app/blog/postgresql-skip-locked)
- [Neon queue guide](https://neon.com/guides/queue-system)

State-model lineage:
- [Prefect States Concepts](https://docs.prefect.io/v3/concepts/states) — failed vs crashed distinction
- [Hatchet durable child-spawning](https://docs.hatchet.run/home/child-spawning) — `awaiting_children`
- [Temporal external payload storage](https://docs.temporal.io/external-storage) — blob-pointer claim-check

JSONB sizing:
- [Evan Jones on large JSONB performance](https://www.evanjones.ca/postgres-large-json-performance.html)
- [pganalyze on JSONB TOAST](https://pganalyze.com/blog/5mins-postgres-jsonb-toast)

Retention / partitioning:
- [pg_partman auto-archiving (Crunchy Data)](https://www.crunchydata.com/blog/auto-archiving-and-data-retention-management-in-postgres-with-pg_partman)
- [Time-based retention (Sequin)](https://blog.sequinstream.com/time-based-retention-strategies-in-postgres/)

Idempotency rules:
- [Gunnar Morling — Idempotency keys](https://www.morling.dev/blog/on-idempotency-keys/)
- [useworkflow idempotency foundations](https://useworkflow.dev/docs/foundations/idempotency)
