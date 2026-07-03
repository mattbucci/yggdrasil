-- Hermes scheduler execution backend (ygg hermes / Part B).
--
-- A task_run is now dispatched to one of two backends:
--   * 'tmux'   — the default: a local Claude Code worker in a tmux window
--                with its own git worktree (unchanged behavior).
--   * 'hermes' — a sandboxed agent behind the hermes-gateway router. The run
--                is submitted via POST /v1/tasks; `remote_task_id` holds the
--                gateway task id the scheduler polls to reconcile terminal
--                state. No tmux window, no worktree.
--
-- Every existing row defaults to 'tmux', so the SQLite dialect NOT NULL DEFAULT
-- backfill matches the pre-migration behavior exactly (see the port's
-- 20260702000001_initial.sql for column-style conventions: TEXT columns,
-- NOT NULL DEFAULT literals, nullable TEXT for optional references).

ALTER TABLE task_runs ADD COLUMN backend TEXT NOT NULL DEFAULT 'tmux';
ALTER TABLE task_runs ADD COLUMN remote_task_id TEXT;

-- Per-task backend selector. NULL / '' / 'tmux' => local tmux (default).
-- 'hermes' or 'hermes:<agent>' routes the task to a sandboxed gateway agent.
-- A bare 'hermes' uses the gateway's default agent (YGG_HERMES_DEFAULT_AGENT,
-- else 'feature-dev'). The smallest, self-documenting opt-in column; keeping it
-- on `tasks` (not buried in input_spec JSON) makes `ygg hermes back` and the
-- dispatcher a single, explicit column read.
ALTER TABLE tasks ADD COLUMN backend TEXT;

CREATE INDEX idx_runs_hermes_running
    ON task_runs (updated_at)
    WHERE backend = 'hermes' AND state = 'running';
