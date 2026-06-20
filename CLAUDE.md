# Project Instructions for AI Agents

This project **is** Yggdrasil — a multi-agent coordination layer. We dogfood it: the `ygg` CLI and hooks installed in this repo coordinate the agents working on it. Refer to the project as **Yggdrasil** in prose; `ygg` is the command binary.

## Working in This Repo (Dogfooded Coordination)

The SessionStart, UserPromptSubmit, Stop, PreCompact, and PreToolUse hooks are active. They prime agent context, deliver agent-to-agent messages, record token stats, enforce locks, and track state in Postgres. You will see prime output at the top of each session (`<!-- ygg:prime -->`). (ADR 0015: the similarity-retrieval / embedding layer was removed — there is no longer a `[ygg memory | ...]` injection. `ygg remember` was later re-added as a plain, non-embedding note store; see below.)

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in the current repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title" --kind <task|bug|feature|chore|epic> --priority <0-4>
                                            # Priority: 0=critical 1=high 2=med 3=low 4=backlog.
                                            # Accepts "P0".."P4" too. NOT "high"/"medium"/"low".
                                            # --agent-slug <name>: thematic worker name the scheduler
                                            #   spawns this task under (e.g. "oauth-refresh"). Pick one
                                            #   tied to the work, not the worktree. Sanitized to [a-z0-9-].
ygg task claim <ref>                        # Take a task (assign + in_progress)
ygg task show <ref>                         # Full detail for <prefix>-NNN or UUID
ygg task close <ref> [--reason "..."]       # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg task move <ref> <target-prefix>         # Reassign a misfiled task to another repo (renumbers the ref)
ygg task dupes [--all] [--limit N]          # Probable duplicate pairs (string similarity)

ygg remember "..."                          # Durable note (repo-scoped; --global for all repos)
ygg remember --list [--all] [--limit N]     # Read stored notes (also surfaced in `ygg prime`)

ygg handoff save "..."                       # Checkpoint this session before /clear; also accepts stdin (`... | ygg handoff save` or `ygg handoff save -`)
ygg handoff show                             # Print the current resume note
ygg handoff clear                            # Drop it once resumed

ygg status                                  # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>             # Lease a shared resource before editing
ygg lock release <resource-key>             # Release when done
ygg lock list                               # See outstanding locks
ygg spawn --task "..."                      # Spawn a parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over / steer another agent
ygg logs --follow                           # Live event stream
```

### Rules

- **Before editing a resource another agent might touch** (shared file, branch, migration, config), acquire a lock: `ygg lock acquire <path-or-key>`. Release when done. This is Yggdrasil's core contract — bypassing it defeats the coordination layer we're building.
- **For parallel work** that warrants its own context window, prefer `ygg spawn` over the native Task/Agent tool. Spawned agents are tracked in the DB, get their own prime context, and participate in lock coordination.
- **Before assuming you're alone**, check `ygg status`. Other agents may hold locks or be mid-task on related work.
- **Task tracking** — use `ygg task` for anything that outlives the current session: creating work, recording dependencies, claiming, closing. Intra-turn checklists can stay in native TaskCreate; cross-session work lives in `ygg task`.
- **Durable rules** — write hard rules to `CLAUDE.md` (repo) or `~/.claude/CLAUDE.md` (global); for file-scoped engineering corrections use `ygg learn add` with a glob. For shorter cross-session notes use `ygg remember "..."` (repo-scoped, `--global` for everywhere) — a plain note store with no embeddings/similarity; recent notes surface in the prime block and via `ygg remember --list`.
- **Before a context reset** (`/clear` or compaction), write a resume note with `ygg handoff save` — the work in flight, the next concrete step, open PRs/decisions. It is keyed to this repo + agent and leads the next `ygg prime` automatically, so the fresh session continues without re-explaining. `ygg handoff save` replaces the prior note; `ygg handoff clear` drops it once resumed.
- **When you are corrected on a durable, file-scoped rule** (the kind that should fire every time someone touches a path), propose it before the session ends: `ygg learn propose "<rule>" --file-glob "<glob>"` (ADR 0017). Proposals land in an approval gate (`status='pending'`) and fire on nothing until a human runs `ygg learn approve <id>` — so capture is cheap and safe, never auto-promoted into the live corpus. Triage the queue with `ygg learn pending`. Use this for recurring engineering corrections; one-off notes still go to `ygg remember`.
- **Do NOT** use `bd` / beads. This project uses `ygg task` instead.

## Terse for AI-tracking fields

When writing content that only agents consume — `ygg task create`
titles/descriptions/acceptance/design/notes, `ygg learn` rules — be
terse. Drop filler (really/just/basically/actually/very).
Drop articles (`a`/`an`/`the`) when meaning survives. Prefer one sentence
per field where content allows. **Preserve verbatim**: identifiers
(snake_case, CamelCase), paths, commands, numbers, URLs, and modal
keywords (always/never/must/should/cannot/don't/may/shall).

Does **NOT** apply to commit messages, PR descriptions, code comments,
or chat responses — those are human-facing and full fidelity is correct.

## Session Completion

This repo is **public**. Default to **PRs into `main`**, not direct pushes.

Work is NOT complete until your branch is pushed and (where applicable) a PR is open.

1. Run quality gates if code changed (`cargo test`, `cargo check --all-targets`, `cargo fmt --check`).
2. Release any locks you still hold (`ygg lock list` → `ygg lock release <key>`).
3. Branch + PR (the default):
   ```bash
   git checkout -b <topic-branch>
   git push -u origin <topic-branch>
   gh pr create --base main --fill          # see CONTRIBUTING.md for the body template
   ```
4. **Direct push to `main` is acceptable for trivial changes only**: typos, generated-artifact updates, single-file docs edits a maintainer would rubber-stamp. When in doubt, open a PR.
5. Reference any related tasks (`yggdrasil-NNN`) in the PR body so the rollup updates.

**Never** stop before the branch is pushed and the PR is open. **Never** say "ready to push when you are" — push the branch and open the PR yourself.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```

## Build & Test

```bash
cargo build --release        # Build the ygg binary
cargo test                   # Run tests (requires Postgres via docker-compose)
docker-compose up -d         # Start Postgres
ygg migrate                  # Run migrations
make install                 # Build + install to ~/.local/bin/ygg + verify
make reinstall               # Re-sign + verify the existing install (recovery)
```

**macOS install gotcha:** `cp -f target/release/ygg ~/.local/bin/ygg` over a
running binary invalidates the Gatekeeper / codesign cache. The first
invocation after that **silently SIGKILLs** (no error, just exit 137 / "ygg
status" hangs). Use `make install` — it copies to a sibling tmp path, atomic
`mv`, re-signs ad-hoc, and runs a 5s `--version` smoke. If you already hit
this and the installed binary hangs, run `make reinstall` to re-sign without
rebuilding.

**DB pool sizing:** `YGG_DB_POOL` overrides the sqlx pool's
`max_connections` (default 32). Bump it when running fleets >50 agents;
the scheduler, watcher, TUI, and per-agent hooks all draw from this
pool, and connection-wait latency manifests as spurious tick lag.

**Spawn permission mode:** `YGG_SPAWN_PERMISSION_MODE` overrides the
Claude Code `--permission-mode` flag for spawned agents (default
`bypassPermissions`). Accepted values: `bypassPermissions`, `dontAsk`,
`acceptEdits`, `default`, `plan`. Each spawned agent also gets its own
git worktree under `.ygg/worktrees/<name>` to avoid working-directory
collisions between concurrent agents.

## Architecture Overview

- **src/models/**: `agent` (state machine), `task` / `task_run` (tracking + scheduler runs), `event` (live stream).
- **src/cli/**: one file per subcommand — `prime`, `spawn`, `lock`, `interrupt`, `status`, `logs`, `task_cmd`, `run_cmd`, `scheduler_cmd`, `hook_cmd`.
- **src/lock.rs, src/scheduler.rs**: coordination primitives.
- **migrations/**: Postgres schema (`uuid-ossp`). ADR 0015 removed `pgvector` and the embedding/`nodes` tables.
- **Hooks** → native `ygg hook <event>` handlers, installed by `ygg init` at Claude Code lifecycle events.

<!-- BEGIN YGG INTEGRATION v:4 hash:8de7570e -->
<!-- markdownlint-disable MD024 -->
## Yggdrasil Agent Coordination

This project uses **Yggdrasil** (`ygg`) for resource coordination and issue
tracking across parallel Claude Code agents. The SessionStart, UserPromptSubmit,
Stop, PreCompact, and PreToolUse hooks are active — they prime agent context,
deliver agent-to-agent messages, record token stats, and track state in
Postgres. You will see prime output at the top of each session
(`<!-- ygg:prime -->`).

### Quick Reference

```bash
ygg task ready                              # Unblocked tasks in the current repo
ygg task list [--all] [--status <...>]      # All tasks in this repo (or everywhere)
ygg task create "title" --kind <k> --priority <0-4>   # See priority/kind values below
ygg task claim <ref>                        # Take a task (assign + in_progress)
ygg task show <ref>                         # Full detail for <prefix>-NNN or UUID
ygg task close <ref> [--reason "..."]       # Complete a task
ygg task dep <task> <blocker>               # Record dependency
ygg task dupes [--all] [--limit N]          # Probable duplicate pairs (string similarity)
```

### Task field values (important — no guessing)

- `--priority <0..4>` — **0 = critical, 1 = high, 2 = medium, 3 = low, 4 = backlog**.
  Also accepts `P0`..`P4`. Do NOT pass strings like "high" / "medium" / "low".
- `--kind <task|bug|feature|chore|epic>` — one of these five. Default is `task`.
- `--status <open|in_progress|blocked|closed>` — for filtering / transitions.
- `--label <a,b,c>` — comma-separated labels. Repeatable.
- `<ref>` is either `<prefix>-<N>` (e.g. `yggdrasil-42`) or a UUID.

### Ticket body structure

Tickets are authored and consumed by autonomous agents. Use the **dedicated
fields** — do NOT cram everything into `--description`. `ygg task show`
renders `acceptance`/`design`/`notes` as their own sections; a blob in
`--description` leaves those columns NULL. Run `ygg task create --template`
for a fill-in scaffold.

- **`--description`** — **Why** (one sentence, cite the source:
  `Adversarial review:`, `Codebase audit:`, `Incident <date>:`), **What** (one
  sentence, imperative), then **Context** — full-fidelity background. The
  agent that claims this task starts cold; it must not know *less* than the
  conversation that spawned the task. Capture the situation, decisions already
  made, alternatives ruled out and *why*, and pointers (files, functions,
  prior tickets). This is the one field where you do **not** compress — close
  the knowledge gap between chat and ticket. Long? pipe it via `--body-file` /
  `--stdin`. No `## headers`.
- **`--acceptance`** — the **Definition of Done**, as a `- [ ]` checkbox list.
  One box per *independently verifiable* condition; pin paths, commands,
  numeric thresholds. No vague verbs ("improve", "consider"). This is
  per-task correctness — "did I build the right thing".
- **`--design`** *(optional)* — **Constraints** ("use exactly this unless a
  hard blocker": stack, approach, which files to touch) and **Non-goals** —
  what NOT to expand into, and what needs approval first. Ask before adding a
  dependency, feature, or surface the ticket didn't name. This bounds scope
  the way `--acceptance` bounds done-ness.
- **`--notes`** *(optional)* — `Refs:` (`yggdrasil-NN`, ADR number, URL) and
  any DoD deviation.

**Definition of Done is two layers.** The per-task `--acceptance` checklist
above, **plus** the repo-wide gates that apply to every task and are NOT
retyped per ticket: `cargo test` + `cargo check --all-targets` +
`cargo fmt --check` pass, locks released, branch pushed, PR open (see Session
Completion). Record only *deviations* from the repo gates in `--notes`.

**Before `ygg task close`:** re-read `ygg task show`, run each acceptance
box's check, and tick the ones you verified (`ygg task update <ref>
--acceptance "..."`). `ygg task show` prints a live `(checked/total)` count.
`ygg task close` warns when boxes are unticked, and **blocks** under
`--require-acceptance` (or `YGG_CLOSE_REQUIRES_ACCEPTANCE=1`) unless `--force`.

Example:

```bash
ygg task create "bump db pool default to 32" --kind chore --priority 1 \
  --description "Why: adversarial review found src/db.rs max_connections(10) starves 50+ agent fleets.
What: raise the pool default to 32; accept a YGG_DB_POOL override.

Context: the scheduler, watcher, TUI, and per-agent hooks all draw from one
sqlx pool. At 10 connections a >50-agent fleet blocks on connection-wait, which
surfaces as spurious tick lag (looked like a scheduler bug for a week). We chose
32 as the floor that cleared the lag in the bench without exhausting Postgres'
default 100 max_connections, leaving headroom for psql/manual sessions. Rejected
making it unbounded (risks hitting Postgres' cap) and per-component pools (more
moving parts). Knob lives next to the pool builder in src/db.rs." \
  --acceptance "- [ ] src/db.rs default = 32
- [ ] YGG_DB_POOL parses to u32, falls back to 32 on parse error
- [ ] CLAUDE.md Build & Test documents YGG_DB_POOL
- [ ] cargo test passes
- [ ] cargo check --all-targets clean" \
  --design "Constraints: change only src/db.rs pool builder + the CLAUDE.md knob doc.
Out of scope — ask first: no migration, no new config file, don't touch hook scripts." \
  --notes "Refs: yggdrasil-141, adversarial-review note 2026-04-23"
```

### Terse for AI-tracking fields

When writing content that only agents consume — `ygg task create` titles,
the `--acceptance` checklist, `ygg learn` rules — be terse. Drop filler
(really/just/basically/actually/very). Drop articles (`a`/`an`/`the`) when
meaning survives. Prefer one sentence per criterion where content allows.
**Preserve verbatim**: identifiers (snake_case, CamelCase), paths, commands,
numbers, URLs, and modal keywords (always/never/must/should/cannot/
don't/may/shall).

**Exception — the `--description` Context paragraph is NOT terse.** Terseness
on context is what creates the knowledge gap between a Claude conversation and
the ticket an agent later picks up cold. Write the background in full: the
situation, the decisions and the reasoning behind them, alternatives rejected,
and file/function pointers. Compress the *criteria*, not the *context*. (The
`--design` constraints/non-goals are likewise full-fidelity where scope is
non-obvious.)

Does **NOT** apply to commit messages, PR descriptions, code comments,
or chat responses — those are human-facing and full fidelity is correct.

Example:
```bash
ygg task create "fix migration ordering" --kind bug --priority 1 --label migrations,sqlx

ygg status                                  # See all agents' state, locks, recent activity
ygg lock acquire <resource-key>             # Lease a shared resource before editing
ygg lock release <resource-key>             # Release when done
ygg lock list                               # See outstanding locks
ygg spawn --task "..."                      # Spawn a parallel agent in a new tmux window
ygg interrupt take-over --agent <name>      # Take over / steer another agent
ygg logs --follow                           # Live event stream
```

### Rules

- **Before editing a resource another agent might touch** (shared file, branch, migration, config), acquire a lock: `ygg lock acquire <path-or-key>`. Release when done. This is Yggdrasil's core contract — bypassing it defeats the coordination layer.
- **For parallel work** that warrants its own context window, prefer `ygg spawn` over the native Task/Agent tool. Spawned agents are tracked in the DB, get their own prime context, and participate in lock coordination.
- **Before assuming you're alone**, check `ygg status`. Other agents may hold locks or be mid-task on related work.
- **Task tracking** — use `ygg task` for anything that outlives the current session: creating work, recording dependencies, claiming, closing. Intra-turn checklists can stay in a native TodoList; cross-session work lives in `ygg task`.
- **Do NOT** use `bd` / beads. This project uses `ygg task` instead.

### Dedup + learnings

- **Before `ygg task create`** → run `ygg task dupes --limit 5` (or
  `--all` for cross-repo). Dupe detection is token-set string similarity
  on title+description (no embeddings). If a pair surfaces near the top,
  prefer claiming/extending the existing task over filing a new one.

- **For recurring engineering corrections** → `ygg learn add` with a file
  glob. Learnings re-fire deterministically when an agent touches a
  matching file — use for "every time someone edits X, also check Y"
  rules. Retrieval is SQL predicates on (repo, file_glob, rule_id), not
  similarity.

## Session Completion

Work is NOT complete until `git push` succeeds.

1. Run quality gates if code changed (tests, linters, build/type-check).
2. Release any locks you still hold (`ygg lock list` → `ygg lock release <key>`).
3. Push:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
4. If push fails, resolve and retry until it succeeds.

**Never** stop before pushing; **never** say "ready to push when you are" — you push.

## Non-Interactive Shell Commands

Some systems alias `cp`/`mv`/`rm` to interactive mode which hangs agents. Use:

```bash
cp -f src dst     mv -f src dst     rm -f file     rm -rf dir     cp -rf src dst
# scp / ssh: -o BatchMode=yes         apt-get: -y         brew: HOMEBREW_NO_AUTO_UPDATE=1
```
<!-- markdownlint-enable MD024 -->
<!-- END YGG INTEGRATION -->
