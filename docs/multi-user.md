# Multi-User Support

## Overview

Yggdrasil partitions data by user identity so multiple humans sharing a
database get isolated namespaces for agents, tasks, locks, nodes,
memories, and events. Cross-user visibility is opt-in.

## Identity Resolution

Resolved once per process, cached via `OnceLock`:

1. `YGG_USER` environment variable (explicit override)
2. Output of `whoami` (OS-level identity)
3. `"default"` (fallback)

The result is stored as `user_id TEXT NOT NULL DEFAULT ''` on every
partitioned table. Existing rows from single-user deployments retain
`user_id = ''` — the legacy namespace.

## Schema Changes

Migration: `20260428000001_multi_user.sql`

### Columns added

| Table | Column | Notes |
|-------|--------|-------|
| `agents` | `user_id TEXT NOT NULL DEFAULT ''` | Part of new compound key |
| `repos` | `user_id TEXT NOT NULL DEFAULT ''` | Part of new prefix uniqueness |
| `tasks` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `locks` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `nodes` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `sessions` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `events` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `memories` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `workers` | `user_id TEXT NOT NULL DEFAULT ''` | |
| `learnings` | `user_id TEXT NOT NULL DEFAULT ''` | |

Child tables (`task_deps`, `task_labels`, `task_events`, `task_seq`,
`agent_stats`, `bench_*`) inherit scoping via FK and need no column.

### Constraint changes

- `agents_name_persona_uk` → `agents_name_persona_user_uk`:
  `(user_id, agent_name, COALESCE(persona, ''))` — same agent name can
  exist for different users.
- `repos_task_prefix_key` → `repos_user_prefix_uk`:
  `(user_id, task_prefix)` — same repo can be registered by different users.

### Indexes

Filtered indexes on `user_id` for agents, tasks, locks, nodes, repos,
memories, sessions, events, and workers.

## Isolation Model

### Default: user-scoped

All `AgentRepo` and `LockManager` queries filter by `user_id`:
- `list()`, `get_by_name()`, `register()`, `find_stale()`, `find_orphaned()`
- `acquire()` inserts with `user_id`; `list_all()` filters by it

UUID-based operations (`get(agent_id)`, `transition()`, `release()`,
`heartbeat()`) are globally unique and don't need user filtering.

### Cross-user visibility

- `AgentRepo::list_all_users()` — returns agents across all users
- `LockManager::list_all_users()` — returns locks across all users
- CLI: `ygg status --all-users` flag (future)

### Messaging

Agent-to-agent messaging (`ygg msg send`, `ygg chat`) targets agents by
name. Cross-user messaging requires knowing the recipient's agent name
within their namespace. Future: `--user <name>` flag on `msg send`.

## Migration Path

1. Run `ygg migrate` — adds columns with `DEFAULT ''`
2. Existing data continues to work: all queries match `user_id = ''`
3. Set `YGG_USER` in agent environments to activate per-user isolation
4. New agents/tasks/locks created after setting `YGG_USER` get the new
   user_id and are invisible to the legacy namespace

## Configuration

| Variable | Purpose | Default |
|----------|---------|---------|
| `YGG_USER` | Override user identity | unset (→ `whoami`) |
| `YGG_DB_POOL` | Connection pool size | `32` |

## Implementation Details

User identity is resolved via `ygg::db::user_id()` — a process-global
`OnceLock` that calls `resolve_user()` once. All repo/manager structs
(`AgentRepo`, `LockManager`) accept `user_id: &str` in their constructor.
CLI functions and the TUI use `crate::db::user_id()` to avoid threading
the parameter through deep call stacks.
