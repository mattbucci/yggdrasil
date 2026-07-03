# Yggdrasil

Yggdrasil is a multi-agent coordination layer for AI coding agents. It gives fleets of Claude Code instances (or any CLI-driven agent) the infrastructure they need to work in the same codebase without colliding: resource locking, task tracking with dependency graphs, a scheduler for autonomous task execution, a real-time TUI dashboard, and inter-agent messaging. Built in Rust, backed by PostgreSQL. The CLI binary is `ygg`.

> **Note (ADR 0015, 2026-06-07):** Yggdrasil started as both a coordination layer *and* a similarity-retrieved cross-session memory. The memory bet didn't pay off, so the embedding/retrieval layer (Ollama, pgvector, the `nodes` DAG, scoped memories, per-turn injection) was removed. Yggdrasil is now a focused **orchestration** layer. Durable agent rules live in Claude Code's native `CLAUDE.md` / `MEMORY.md`.

---

## Install

**From source** (requires Rust 1.75+):

```bash
cargo install --path .
```

**From GitHub releases:** coming soon.

**Homebrew:** coming soon.

## Requirements

- **Rust 1.75+** (build from source only)

The database is an embedded SQLite file (created automatically at
`$XDG_DATA_HOME/ygg/ygg.db`, fallback `~/.local/share/ygg/ygg.db`) — no
database server to install or run.

## Quick Start

```bash
# 1. Bootstrap everything: data dir + migrations, hooks
ygg init

# 2. Create a task
ygg task create "my first task" --kind task --priority 2

# 3. Spawn a Claude Code agent to work on it
ygg spawn --task "do something"

# 4. Open the TUI dashboard to watch your fleet
ygg dashboard

# 5. Check fleet state from the command line
ygg status
```

## Architecture

```text
+------------------+         +-------------------------------+
|   Claude Code    |         |     SQLite (embedded, WAL)    |
|                  |         |                               |
|  SessionStart  --+--ygg-->|  agents   (state machine)     |
|  UserPromptSubmit+--msg--->|  events   (live stream)       |
|  Stop          --+--run--->|  locks    (semantic leases)   |
|  PreCompact    --+-prime-->|  tasks    (tracking + deps)   |
|  PreToolUse    --+--lock-->|  task_runs(scheduler runs)    |
+------------------+         +-------------------------------+
        |                          |               |
        |            unix socket bus (ygg.sock)    |
        |            wakes the scheduler on events |
        v                          v               v
   tmux windows           +---------------+  +--------------+
   (one per agent)        | ygg scheduler |  | TUI Dashboard|
                          | (file-locked  |  |  (ratatui)   |
                          |  singleton)   |  +--------------+
                          +---------------+
```

Hooks (installed by `ygg init` as native `ygg hook <event>` handlers) fire at Claude Code lifecycle events:

- **SessionStart / PreCompact** -> `ygg prime` -- emits agent context as markdown
- **UserPromptSubmit** -> delivers unread agent-to-agent messages, records token stats
- **Stop** -> `ygg run capture-outcome` + `ygg stop-check` -- records task-run outcome, blocks premature worker exit
- **PreToolUse** -> `ygg lock` / `ygg agent-tool` -- enforces resource leases, records tool usage

There is no long-running daemon other than the optional `ygg watcher` (heartbeats, lock expiry) and the `ygg scheduler`. Everything else runs as one-shot CLI invocations.

## Why Yggdrasil Exists

Running one agent in a terminal is easy. Running three to seven is taxing but common. Beyond that, things break: too many windows to watch, too much overlap on shared files, too much context lost to compaction, too much prior conversation that never resurfaces.

Yggdrasil focuses on the parts that are hard to get in one place:

- A shared **lock graph** so agents don't clobber each other mid-edit.
- **Task tracking** with a dependency DAG, epic rollups, and a scheduler that dispatches ready tasks to spawned agents.
- **Inter-agent messaging** delivered at the recipient's next turn.
- **Live event streams** and a TUI dashboard for humans watching multiple agents at once.

One deliberate design choice: Yggdrasil is **global per user**, not per repo. One SQLite database backs every repo you work in; agents are auto-keyed by the basename of the current working directory. The trade-off is documented in [ADR 0008](docs/adr/0008-shared-db-across-repos.md) and [Open questions](docs/open-questions.md).

## Subcommand Reference

| Command     | Purpose                                                                 |
|-------------|-------------------------------------------------------------------------|
| `init`      | Bootstrap: data dir + migrations, hooks.                               |
| `up`        | Launch the tmux dashboard (default when run bare).                     |
| `dashboard` | Launch the TUI dashboard directly.                                      |
| `status`    | Quick text output of agent + system state.                              |
| `migrate`   | Run database migrations.                                                |
| `spawn`     | Spawn a new agent in a tmux window, registered in the DB.               |
| `task`      | Task tracking: `create / list / ready / claim / close / dep / show / dupes`. |
| `run`       | Task-run lifecycle: `claim / heartbeat / finalize / show / list / capture-outcome`. |
| `scheduler` | Autonomous task-DAG scheduler: `run / tick / status / dry-run / backfill`. |
| `lock`      | Acquire / release / list / heartbeat resource locks.                    |
| `learn`     | Scoped learnings: deterministic rule capture matched by file glob.      |
| `prime`     | Hook: emits agent context as Markdown.                                  |
| `msg`/`chat`| Agent-to-agent messaging on the events bus.                             |
| `interrupt` | Human overrides: take-over, hand-back.                                  |
| `logs`      | Live event stream (stdout).                                             |
| `watcher`   | Background daemon: heartbeats, lock expiry.                             |
| `recover`   | Recover orphaned agents stuck in active states.                         |
| `rollup`    | Per-repo activity summary over a time window.                           |
| `reap`      | Purge stale locks / sessions. Safe to cron.                             |
| `bar`       | Claude Code statusline generator (context pressure, cache rate, spend). |
| `agent-tool`| Hook: record the tool an agent is about to call.                        |
| `hook`      | Native Claude Code lifecycle hook handlers.                             |

## Project Layout

```text
src/
  cli/          one file per subcommand
  models/       agent, event, task, task_run -- sqlx types + repos
  stats/        token accounting, telemetry
  tui/          dashboard views (ratatui)
  config.rs     env loading
  db.rs         sqlx pool + migrations runner
  executor.rs   RTK-proxied command execution
  interrupt.rs  human-override primitives
  lock.rs       LockManager -- acquire/release/heartbeat
  scheduler.rs  autonomous task-DAG scheduler
  status.rs     status aggregation
  tmux.rs       tmux window management
  watcher.rs    background daemon
migrations/     SQLite schema
docs/           prose docs + ADRs
```

## Build from Source

```bash
cargo build --release            # build the ygg binary
cargo test                       # run tests (embedded SQLite; no services needed)
make install                     # build + install to ~/.local/bin/ygg
```

## Further Reading

- [Orchestration runtime](docs/orchestration.md) -- scheduler, task runs, payload flow, lock integration, failure semantics.
- [Eval benchmarks](docs/eval-benchmarks.md) -- `ygg bench` scenarios, Tier-A metrics, METR-style methodology.
- [ADR 0015](docs/adr/0015-retrieval-scope-reduction.md) -- why the retrieval/embedding layer was removed.
- [Open questions](docs/open-questions.md) -- the shared-memory hypothesis, named LLM failure modes.
- [Architecture Decision Records](docs/adr/) -- one ADR per non-obvious design choice. Some pre-0015 ADRs (0001, 0002, 0004, 0011, 0012) and design docs (`retrieval.md`, `design-principles.md`) describe the removed retrieval layer and are kept as historical records.

## License

MIT.
