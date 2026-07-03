-- Squashed SQLite baseline schema for Yggdrasil.
--
-- Port from PostgreSQL (2026-07): the tool now runs on an embedded SQLite
-- database (WAL mode). This migration reproduces the cumulative Postgres
-- schema as of the port, translated to the SQLite dialect:
--
--   * UUID columns        -> TEXT (hyphenated lowercase v4). Rust generates
--                            ids with uuid::Uuid::new_v4(); the DEFAULT
--                            expression below is a safety net so inserts
--                            that historically relied on uuid_generate_v4()
--                            keep working.
--   * TIMESTAMPTZ         -> TEXT, RFC 3339 UTC, exactly the format sqlx's
--                            chrono integration writes DateTime<Utc> binds
--                            in ("%Y-%m-%dT%H:%M:%f+00:00"). All values use
--                            one canonical shape so lexicographic string
--                            comparison == chronological comparison.
--   * PG enums            -> TEXT + CHECK (col IN (...)).
--   * JSONB               -> TEXT containing JSON.
--   * TEXT[]              -> TEXT containing a JSON array.
--   * NUMERIC             -> REAL (display-only money columns).
--   * UNIQUE NULLS NOT DISTINCT -> unique expression index over
--                            COALESCE(uuid_col, '').

---------------------------------------------------------------------
-- LOCKS: Semantic leases
---------------------------------------------------------------------
CREATE TABLE locks (
    id            TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    resource_key  TEXT NOT NULL,
    agent_id      TEXT NOT NULL,
    acquired_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    expires_at    TEXT NOT NULL,
    heartbeat_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    user_id       TEXT NOT NULL DEFAULT '',
    CONSTRAINT uq_lock_resource UNIQUE (resource_key)
);

CREATE INDEX idx_locks_agent ON locks (agent_id);
CREATE INDEX idx_locks_expiry ON locks (expires_at);
CREATE INDEX idx_locks_user ON locks (user_id);

---------------------------------------------------------------------
-- AGENTS: Workflow state machine
---------------------------------------------------------------------
CREATE TABLE agents (
    agent_id        TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    agent_name      TEXT NOT NULL,
    current_state   TEXT NOT NULL DEFAULT 'idle'
        CHECK (current_state IN ('idle','planning','executing','waiting_tool','context_flush','human_override','mediation','error','shutdown')),
    context_tokens  INTEGER NOT NULL DEFAULT 0,
    metadata        TEXT NOT NULL DEFAULT '{}',
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    persona         TEXT,
    archived_at     TEXT,
    user_id         TEXT NOT NULL DEFAULT '',
    message_cursor  TEXT DEFAULT '1970-01-01T00:00:00+00:00'
);

CREATE UNIQUE INDEX agents_name_persona_user_uk
    ON agents (user_id, agent_name, COALESCE(persona, ''));
CREATE INDEX idx_agents_active ON agents (updated_at DESC) WHERE archived_at IS NULL;
CREATE INDEX idx_agents_user ON agents (user_id) WHERE archived_at IS NULL;

---------------------------------------------------------------------
-- AGENT_STATS: Token usage rollups
---------------------------------------------------------------------
CREATE TABLE agent_stats (
    id              TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    agent_id        TEXT NOT NULL REFERENCES agents(agent_id),
    period          TEXT NOT NULL,
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read      INTEGER NOT NULL DEFAULT 0,
    cache_write     INTEGER NOT NULL DEFAULT 0,
    tool_calls      INTEGER NOT NULL DEFAULT 0,
    task_category   TEXT,
    estimated_cost  REAL NOT NULL DEFAULT 0,
    UNIQUE(agent_id, period, task_category)
);

---------------------------------------------------------------------
-- EVENTS
---------------------------------------------------------------------
CREATE TABLE events (
    id                  TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    event_kind          TEXT NOT NULL
        CHECK (event_kind IN ('lock_acquired','lock_released','hook_fired','task_created','task_status_changed','agent_state_changed','message','run_scheduled','run_claimed','run_terminal','run_retry','scheduler_tick','scheduler_error','agent_stale_warning')),
    agent_id            TEXT,
    agent_name          TEXT NOT NULL DEFAULT '',
    payload             TEXT NOT NULL DEFAULT '{}',
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    cc_session_id       TEXT,
    session_id          TEXT REFERENCES sessions(session_id),
    recipient_agent_id  TEXT REFERENCES agents(agent_id),
    user_id             TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_events_created ON events (created_at DESC);
CREATE INDEX idx_events_agent ON events (agent_id, created_at DESC);
CREATE INDEX idx_events_cc_session ON events (cc_session_id, created_at DESC)
    WHERE cc_session_id IS NOT NULL;
CREATE INDEX idx_events_session ON events (session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX idx_events_recipient_unread
    ON events (recipient_agent_id, created_at DESC)
    WHERE recipient_agent_id IS NOT NULL;
CREATE INDEX idx_events_message_created
    ON events (created_at DESC)
    WHERE event_kind = 'message';
CREATE INDEX idx_events_user ON events (user_id);

---------------------------------------------------------------------
-- REPOS
---------------------------------------------------------------------
CREATE TABLE repos (
    repo_id       TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    canonical_url TEXT,
    name          TEXT NOT NULL,
    task_prefix   TEXT NOT NULL,
    local_paths   TEXT NOT NULL DEFAULT '[]',
    metadata      TEXT NOT NULL DEFAULT '{}',
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    user_id       TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_repos_prefix ON repos (task_prefix);
CREATE UNIQUE INDEX repos_user_prefix_uk ON repos (user_id, task_prefix);
CREATE INDEX idx_repos_user ON repos (user_id);

---------------------------------------------------------------------
-- SESSIONS
---------------------------------------------------------------------
CREATE TABLE sessions (
    session_id      TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    agent_id        TEXT NOT NULL REFERENCES agents(agent_id),
    repo_id         TEXT REFERENCES repos(repo_id),
    cc_session_id   TEXT UNIQUE,
    current_state   TEXT NOT NULL DEFAULT 'idle'
        CHECK (current_state IN ('idle','planning','executing','waiting_tool','context_flush','human_override','mediation','error','shutdown')),
    context_tokens  INTEGER NOT NULL DEFAULT 0,
    last_tool       TEXT,
    started_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    ended_at        TEXT,
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    metadata        TEXT NOT NULL DEFAULT '{}',
    user_id         TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_sessions_agent ON sessions (agent_id, started_at DESC);
CREATE INDEX idx_sessions_repo ON sessions (repo_id, started_at DESC);
CREATE INDEX idx_sessions_live ON sessions (agent_id, updated_at DESC) WHERE ended_at IS NULL;
CREATE INDEX idx_sessions_user ON sessions (user_id, agent_id);

---------------------------------------------------------------------
-- TASKS
---------------------------------------------------------------------
CREATE TABLE tasks (
    task_id             TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    repo_id             TEXT NOT NULL REFERENCES repos(repo_id) ON DELETE CASCADE,
    seq                 INTEGER NOT NULL,
    title               TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    acceptance          TEXT,
    design              TEXT,
    notes               TEXT,
    kind                TEXT NOT NULL DEFAULT 'task'
        CHECK (kind IN ('task','bug','feature','chore','epic')),
    status              TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open','in_progress','blocked','closed','awaiting_children','awaiting_approval','awaiting_review')),
    priority            INTEGER NOT NULL DEFAULT 2 CHECK (priority BETWEEN 0 AND 4),
    created_by          TEXT REFERENCES agents(agent_id),
    assignee            TEXT REFERENCES agents(agent_id),
    human_flag          INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    closed_at           TEXT,
    close_reason        TEXT,
    relevance           INTEGER NOT NULL DEFAULT 50,
    external_ref        TEXT,
    deleted_at          TEXT,
    user_id             TEXT NOT NULL DEFAULT '',
    -- Execution columns (ADR 0016)
    runnable            INTEGER NOT NULL DEFAULT 0,
    current_attempt_id  TEXT REFERENCES task_runs(run_id),
    max_attempts        INTEGER NOT NULL DEFAULT 3,
    timeout_ms          INTEGER,
    deadline_at         TEXT,
    approval_level      TEXT NOT NULL DEFAULT 'auto',
    approved_at         TEXT,
    approved_by_agent_id TEXT REFERENCES agents(agent_id),
    parent_task_id      TEXT REFERENCES tasks(task_id),
    input_spec          TEXT NOT NULL DEFAULT '{}',
    output_spec         TEXT NOT NULL DEFAULT '{}',
    agent_role          TEXT,
    required_locks      TEXT NOT NULL DEFAULT '[]',
    result_blob_ref     TEXT,
    plan_strategy       TEXT,
    -- yggdrasil-183: thematic agent names. NULL = fall back to the generic
    -- ygg-<prefix>-<seq> scheme. Sanitized at the CLI boundary to [a-z0-9-].
    agent_slug          TEXT,
    UNIQUE (repo_id, seq),
    CONSTRAINT tasks_relevance_range CHECK (relevance BETWEEN 0 AND 100),
    CONSTRAINT tasks_approval_level_chk CHECK (approval_level IN ('auto', 'approve_plan', 'approve_completion')),
    CONSTRAINT tasks_agent_role_chk CHECK (agent_role IS NULL OR agent_role IN ('planner', 'executor', 'critic')),
    CONSTRAINT tasks_plan_strategy_chk CHECK (plan_strategy IS NULL OR plan_strategy IN ('llm')),
    CHECK (task_id <> parent_task_id)
);

CREATE INDEX idx_tasks_repo_status ON tasks (repo_id, status, priority);
CREATE INDEX idx_tasks_assignee ON tasks (assignee) WHERE assignee IS NOT NULL;
CREATE INDEX idx_tasks_external_ref ON tasks (external_ref) WHERE external_ref IS NOT NULL;
CREATE INDEX idx_tasks_deleted_at ON tasks (deleted_at) WHERE deleted_at IS NOT NULL;
CREATE INDEX idx_tasks_user ON tasks (user_id, repo_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_tasks_runnable ON tasks (repo_id, priority, updated_at)
    WHERE runnable = 1 AND status IN ('open', 'in_progress');
CREATE INDEX idx_tasks_parent ON tasks (parent_task_id) WHERE parent_task_id IS NOT NULL;

-- Task dependencies
CREATE TABLE task_deps (
    task_id     TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    blocker_id  TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    PRIMARY KEY (task_id, blocker_id),
    CHECK (task_id <> blocker_id)
);
CREATE INDEX idx_task_deps_blocker ON task_deps (blocker_id);

-- Labels
CREATE TABLE task_labels (
    task_id  TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    label    TEXT NOT NULL,
    PRIMARY KEY (task_id, label)
);
CREATE INDEX idx_task_labels_label ON task_labels (label);

-- Task audit trail
CREATE TABLE task_events (
    event_id    TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    task_id     TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    agent_id    TEXT REFERENCES agents(agent_id),
    kind        TEXT NOT NULL,
    payload     TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    run_id      TEXT REFERENCES task_runs(run_id)
);
CREATE INDEX idx_task_events_task ON task_events (task_id, created_at DESC);
CREATE INDEX idx_task_events_run ON task_events (run_id) WHERE run_id IS NOT NULL;

-- Per-repo sequence counter
CREATE TABLE task_seq (
    repo_id   TEXT PRIMARY KEY REFERENCES repos(repo_id) ON DELETE CASCADE,
    next_seq  INTEGER NOT NULL DEFAULT 1
);

-- Task links (non-blocking relationships)
CREATE TABLE task_links (
    task_id       TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    target_id     TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    kind          TEXT NOT NULL
        CHECK (kind IN ('see_also','superseded_by','duplicate_of','related')),
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    PRIMARY KEY (task_id, target_id, kind)
);
CREATE INDEX idx_task_links_task ON task_links (task_id);
CREATE INDEX idx_task_links_target ON task_links (target_id);

---------------------------------------------------------------------
-- SESSION SUMMARIES
---------------------------------------------------------------------
CREATE TABLE session_summaries (
    session_id          TEXT PRIMARY KEY REFERENCES sessions(session_id),
    agent_id            TEXT NOT NULL REFERENCES agents(agent_id),
    agent_name          TEXT NOT NULL,
    repo_id             TEXT REFERENCES repos(repo_id),
    repo_prefix         TEXT,
    started_at          TEXT NOT NULL,
    ended_at            TEXT,

    user_prompts        INTEGER NOT NULL DEFAULT 0,
    max_context_tokens  INTEGER,

    -- Coordination
    locks_acquired      INTEGER NOT NULL DEFAULT 0,
    locks_released      INTEGER NOT NULL DEFAULT 0,
    lock_conflicts      INTEGER NOT NULL DEFAULT 0,
    interrupts          INTEGER NOT NULL DEFAULT 0,

    -- Work
    tasks_created       INTEGER NOT NULL DEFAULT 0,
    tasks_closed        INTEGER NOT NULL DEFAULT 0,

    finalized_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now'))
);

CREATE INDEX idx_session_summaries_agent ON session_summaries (agent_id, started_at DESC);
CREATE INDEX idx_session_summaries_repo ON session_summaries (repo_id, started_at DESC);
CREATE INDEX idx_session_summaries_start ON session_summaries (started_at DESC);

---------------------------------------------------------------------
-- WORKERS
---------------------------------------------------------------------
CREATE TABLE workers (
    worker_id           TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    task_id             TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    session_id          TEXT REFERENCES sessions(session_id) ON DELETE SET NULL,
    tmux_session        TEXT NOT NULL,
    tmux_window         TEXT NOT NULL,
    worktree_path       TEXT NOT NULL,
    state               TEXT NOT NULL DEFAULT 'spawned'
        CHECK (state IN ('spawned','running','idle','needs_attention','completed','failed','abandoned')),
    started_at          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    last_seen_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    ended_at            TEXT,
    exit_reason         TEXT,
    branch_pushed       INTEGER NOT NULL DEFAULT 0,
    branch_merged       INTEGER NOT NULL DEFAULT 0,
    pr_url              TEXT,
    delivery_checked_at TEXT,
    intent              TEXT,
    user_id             TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_workers_task ON workers (task_id, started_at DESC);
CREATE INDEX idx_workers_live ON workers (tmux_session, tmux_window) WHERE ended_at IS NULL;
CREATE INDEX idx_workers_state ON workers (state, started_at DESC);
CREATE INDEX idx_workers_user ON workers (user_id);

---------------------------------------------------------------------
-- TASK RUNS (ADR 0016: autonomous execution)
---------------------------------------------------------------------
CREATE TABLE task_runs (
    run_id          TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    task_id         TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE,
    attempt         INTEGER NOT NULL,
    parent_run_id   TEXT REFERENCES task_runs(run_id),

    idempotency_key TEXT NOT NULL,

    state           TEXT NOT NULL DEFAULT 'scheduled'
        CHECK (state IN ('scheduled','ready','running','succeeded','failed','crashed','cancelled','retrying','poison')),
    reason          TEXT NOT NULL DEFAULT 'ok'
        CHECK (reason IN ('ok','agent_error','heartbeat_timeout','tmux_gone','max_attempts','user_cancelled','dependency_failed','lock_conflict','timeout','loop_detected','budget_exceeded')),

    scheduled_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    claimed_at      TEXT,
    started_at      TEXT,
    ended_at        TEXT,
    heartbeat_at    TEXT,
    heartbeat_ttl_s INTEGER NOT NULL DEFAULT 300,

    agent_id        TEXT REFERENCES agents(agent_id),
    worker_id       TEXT,
    session_id      TEXT REFERENCES sessions(session_id),

    max_attempts    INTEGER NOT NULL DEFAULT 3,
    retry_strategy  TEXT NOT NULL DEFAULT
        '{"kind":"exponential","base_ms":60000,"cap_ms":600000,"jitter":true}',
    deadline_at     TEXT,

    input           TEXT NOT NULL DEFAULT '{}',
    output          TEXT,
    error           TEXT,

    output_commit_sha TEXT,
    output_branch     TEXT,
    output_pr_url     TEXT,
    output_worktree   TEXT,
    output_blob_ref   TEXT,

    fingerprint     TEXT,

    -- Pre-recovery git checkpoint (yggdrasil-115): SHA of the WIP commit the
    -- watcher stashes on refs/ygg/recovery/<run_id> before reaping.
    pre_recovery_commit TEXT,

    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),

    UNIQUE (task_id, attempt),
    UNIQUE (idempotency_key),
    CHECK (attempt >= 1),
    CHECK (max_attempts >= 1)
);

CREATE INDEX idx_runs_ready ON task_runs (scheduled_at) WHERE state = 'ready';
CREATE INDEX idx_runs_live_heartbeat ON task_runs (heartbeat_at) WHERE state = 'running';
CREATE INDEX idx_runs_retry_candidates ON task_runs (ended_at) WHERE state IN ('failed', 'crashed');
CREATE INDEX idx_runs_deadline ON task_runs (deadline_at) WHERE state = 'running' AND deadline_at IS NOT NULL;
CREATE INDEX idx_runs_task ON task_runs (task_id, attempt DESC);
CREATE INDEX idx_runs_agent ON task_runs (agent_id, started_at DESC) WHERE agent_id IS NOT NULL;
CREATE INDEX idx_runs_worker ON task_runs (worker_id) WHERE worker_id IS NOT NULL;

---------------------------------------------------------------------
-- LEARNINGS
---------------------------------------------------------------------
CREATE TABLE learnings (
    learning_id    TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    repo_id        TEXT REFERENCES repos(repo_id) ON DELETE CASCADE,
    file_glob      TEXT,
    rule_id        TEXT,
    text           TEXT NOT NULL,
    context        TEXT,
    created_by     TEXT REFERENCES agents(agent_id),
    created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    applied_count  INTEGER NOT NULL DEFAULT 0,
    scope_tags     TEXT NOT NULL DEFAULT '{}',
    user_id        TEXT NOT NULL DEFAULT '',
    -- ADR 0017: approval gate. Only 'active' learnings are ever surfaced.
    status         TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('pending', 'active')),
    source         TEXT NOT NULL DEFAULT 'manual'
        CHECK (source IN ('manual', 'proposed')),
    approved_at    TEXT,
    approved_by    TEXT REFERENCES agents(agent_id),
    -- yggdrasil-180: recency of learning application. NULL = never applied.
    last_applied_at TEXT
);

CREATE INDEX idx_learnings_repo ON learnings (repo_id) WHERE repo_id IS NOT NULL;
CREATE INDEX idx_learnings_rule_id ON learnings (rule_id) WHERE rule_id IS NOT NULL;
CREATE INDEX idx_learnings_file_glob ON learnings (file_glob) WHERE file_glob IS NOT NULL;
CREATE INDEX idx_learnings_status ON learnings (status) WHERE status = 'pending';

---------------------------------------------------------------------
-- MEMORIES (`ygg remember`): plain, human-readable note store.
-- No embeddings, no similarity retrieval (ADR 0015). repo_id NULL = global.
---------------------------------------------------------------------
CREATE TABLE memories (
    memory_id   TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    repo_id     TEXT REFERENCES repos(repo_id) ON DELETE CASCADE,
    text        TEXT NOT NULL,
    created_by  TEXT REFERENCES agents(agent_id),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    user_id     TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_memories_repo ON memories (repo_id, created_at DESC);

---------------------------------------------------------------------
-- HANDOFFS: an agent's resume note, written right before /clear.
-- Exactly one current handoff per (repo, agent); NULL repo/agent collapse to
-- '' in the unique expression index (Postgres used UNIQUE NULLS NOT DISTINCT).
---------------------------------------------------------------------
CREATE TABLE handoffs (
    handoff_id  TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    repo_id     TEXT REFERENCES repos(repo_id) ON DELETE CASCADE,  -- NULL = no detected repo
    agent_id    TEXT REFERENCES agents(agent_id) ON DELETE CASCADE,
    text        TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    user_id     TEXT NOT NULL DEFAULT ''
);

CREATE UNIQUE INDEX handoffs_repo_agent_uk
    ON handoffs (COALESCE(repo_id, ''), COALESCE(agent_id, ''));

---------------------------------------------------------------------
-- BENCH TABLES
---------------------------------------------------------------------
CREATE TABLE bench_runs (
    run_id        TEXT PRIMARY KEY DEFAULT (lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-4' || substr(hex(randomblob(2)),2) || '-' || substr('89ab', (abs(random()) % 4) + 1, 1) || substr(hex(randomblob(2)),2) || '-' || hex(randomblob(6)))),
    scenario      TEXT NOT NULL,
    baseline      TEXT NOT NULL,
    parallelism   INTEGER NOT NULL,
    model         TEXT NOT NULL,
    harness_sha   TEXT NOT NULL,
    seed          INTEGER,
    started_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f+00:00','now')),
    ended_at      TEXT,
    passed        INTEGER,
    notes         TEXT,
    CHECK (parallelism >= 1)
);

CREATE INDEX idx_bench_runs_scenario ON bench_runs (scenario, baseline, started_at DESC);
CREATE INDEX idx_bench_runs_started ON bench_runs (started_at DESC);

CREATE TABLE bench_task_results (
    run_id        TEXT NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    task_idx      INTEGER NOT NULL,
    passed        INTEGER NOT NULL,
    wall_clock_s  INTEGER NOT NULL,
    tokens_in     INTEGER,
    tokens_out    INTEGER,
    tokens_cache  INTEGER,
    usd           REAL,
    reopened      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, task_idx)
);

CREATE TABLE bench_metrics (
    run_id     TEXT NOT NULL REFERENCES bench_runs(run_id) ON DELETE CASCADE,
    metric     TEXT NOT NULL,
    value      REAL NOT NULL,
    PRIMARY KEY (run_id, metric)
);
