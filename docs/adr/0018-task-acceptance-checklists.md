# ADR 0018 — Task acceptance as a Definition-of-Done checklist

**Status:** proposed
**Date:** 2026-06-20
**Extends:** ADR 0010 (tasks — the `acceptance`/`design`/`notes` columns).
**Relates to:** ADR 0016 (autonomous execution — run terminal states). The checklist is a deterministic, LLM-free signal an autonomous agent can self-verify against before closing.

## Context

The `tasks` table already has dedicated `acceptance`, `design`, and `notes` TEXT columns (ADR 0010), and `ygg task show` renders each as its own labeled block. But the documented convention (CLAUDE.md / AGENTS.md "Ticket body structure") told agents to write **four prose sections — Why / What / Acceptance: / Refs: — all inside `--description`**. The convention and the schema were out of sync: agents crammed acceptance into the description blob, so the first-class `acceptance` column stayed NULL and `ygg task show` had no distinct "done" section to render.

This matters specifically because Yggdrasil tasks are authored *and* consumed by autonomous agents. The dominant failure mode is **premature close**: an agent reads a vague acceptance line ("docs updated", "pool is bigger"), does something plausible, and closes the task as succeeded. There was no structured, machine-checkable notion of "done" — `ygg task close` (`task_cmd.rs`) just flipped status and classified the free-text reason.

The maintainer asked for a format that "better captures tasks and definitions of done." The fix is not a new format — the slots already exist — it is to align the convention with the schema and add a thin, machine-checkable Definition-of-Done layer on top.

## Decision

Three changes, no schema migration.

### Decision 1 — `acceptance` is the Definition of Done, as a checkbox list

Route fields to their real columns: Why+What → `--description`, the DoD → `--acceptance` formatted as a GitHub-style `- [ ]` / `- [x]` checklist, refs → `--notes`. Each box is one *independently verifiable* condition (path, command, numeric threshold). The convention rewrite lives in CLAUDE.md + AGENTS.md, and `ygg task create --template` emits a fill-in scaffold so the structured form is the path of least resistance.

### Decision 2 — Acceptance (per-task) vs Definition of Done (repo gates) are two layers

- **Acceptance** = per-task correctness, "did I build the right thing" — the `--acceptance` checklist, authored per ticket.
- **Repo gates** = "is it shippable" — `cargo test` + `cargo check --all-targets` + `cargo fmt --check` pass, locks released, branch pushed, PR open. These already exist as the "Session Completion" prose. They are a **repo constant**, NOT retyped per ticket; tickets record only deviations in `--notes`.

This split keeps human/agent authoring effort on the part that's actually task-specific, while the constant gates stay in one place.

### Decision 3 — A structural close gate

`ygg task show` parses the checklist and prints a live `(checked/total)` count. `ygg task close` counts unticked boxes and:

- **warns** by default (`⚠ closing <ref> with N of M acceptance criteria unchecked`) and proceeds — preserves today's flow;
- **blocks** (non-zero exit) when `--require-acceptance` or `YGG_CLOSE_REQUIRES_ACCEPTANCE=1` is set, unless `--force`.

The check is **purely structural** — are the boxes ticked? Free-text or empty acceptance has `total == 0` and is never gated, so existing tasks and the default close path are unaffected.

### Decision 4 — Context is full-fidelity; scope is bounded by non-goals

Two authoring guardrails ship with the body rewrite, both addressing failures observed in practice (tasks carrying less knowledge than the conversation that produced them; agents expanding scope past what a ticket named):

- **Context is the one field that is NOT terse.** ADR-era convention applied "terse for AI-tracking fields" to *everything*, including `--description`. That is exactly what opens a knowledge gap between a Claude conversation and the ticket an agent later claims cold. The rewrite carves out the `--description` **Context** paragraph (situation, decisions made, alternatives rejected and why, file/function pointers) as full-fidelity, and points long context at the existing `--body-file` / `--stdin` inputs. Terseness still governs titles, the `--acceptance` checklist, and `ygg learn` rules — compress the *criteria*, not the *context*.
- **`--design` carries constraints + non-goals.** Previously documented only as vague "approach notes," `--design` becomes the scope guardrail: hard constraints ("use exactly this unless a hard blocker", which files to touch) and **non-goals** — what NOT to expand into, and what needs approval first ("ask before adding a dependency/feature/surface the ticket didn't name"). This bounds scope the way `--acceptance` bounds done-ness.

No new columns — both reuse existing fields (`description`, `design`) and existing inputs (`--body-file`/`--stdin`). The change is convention + the `--template` scaffold.

## Why structural, not executed

A box `- [x] cargo test passes` proves nothing unless something ran the command. The tempting next step is to have `ygg task close` auto-execute acceptance commands. We explicitly **do not**: arbitrary command execution at close time means a sandbox, environment assumptions, and flaky tests that wedge closes — high cost, new failure surface. Instead Yggdrasil's check stays structural (boxes ticked), and *semantic* truth is carried where it already lives: the agent's own run, and ADR 0016's `failed` (acceptance-unmet) vs `crashed` (infra) run states. Honesty of a tick is the agent's responsibility, the same way a human checkbox is.

## Alternatives rejected

- **A new `definition_of_done` / `dod` column, or JSONB-structured acceptance.** ADR 0010 already rejected over-structuring tasks; three TEXT columns + a checkbox convention is enough. JSONB turns every read into a path expression for no gain. Rejected.
- **Renaming the `acceptance` column to `definition_of_done`.** Migration cost + churn across queries and `ygg task show`, zero behavioral benefit. The display label already reads "Acceptance (Definition of Done)". Rejected.
- **A `## Definition of Done` markdown header inside `--description`.** Re-implements the column that already exists, violates the terse-for-agents rule, and leaves `acceptance` NULL. Rejected.
- **Hard-blocking close by default.** Would break the existing manual flow and the scheduler's crash/cancel closes. Warn-by-default, block-on-flag is the safe ordering. Rejected as a default; available via the flag/env.
- **Auto-executing acceptance commands at close.** See "Why structural" — sandbox/flakiness/arbitrary-exec cost. Rejected.
- **Per-task DoD authoring (repeating the repo gates in every ticket).** Noise that drifts. The gates are a repo constant. Rejected.
- **Keep `--description` terse (status quo).** Cheapest, but it is the direct cause of the chat→ticket knowledge gap — an agent claiming a terse ticket re-derives or guesses what the conversation already settled. Rejected for the Context paragraph; terseness retained for criteria/titles.
- **Dedicated `context` / `non_goals` columns.** More structure than needed and a migration; `description` and `design` already hold these. Rejected, consistent with the no-new-columns stance above.

## Consequences

- No migration. The `acceptance`/`design`/`notes` columns and the base `ygg task show` rendering of them already existed; **new** in this change are the task-create/task-close flags (`--template`, `--require-acceptance`, `--force`) and the `(checked/total)` acceptance-count rendering — all of it convention + presentation + an opt-in gate. Fully revertible.
- Old tasks with free-text `acceptance` parse to `(0, 0)` — rendered without a count, never gated. No backfill needed.
- The gate is fail-safe: unset by default (warn only), so nothing that closes today stops closing. Teams that want teeth opt in per-call or via the env var.
- Sets up a future `ygg task close` → `RunState::Failed` wiring on unmet acceptance (ADR 0016 D5) without committing to it now.

## Rollout

1. **M1 — Convention + presentation.** Rewrite CLAUDE.md/AGENTS.md "Ticket body structure"; `ygg task show` checklist count; `ygg task create --template`.
2. **M2 — Close gate.** `ygg task close` warn / `--require-acceptance` / `YGG_CLOSE_REQUIRES_ACCEPTANCE` / `--force`.
3. **M3 (future) — Semantic wiring.** Optionally map unmet-acceptance close to `RunState::Failed` so the scheduler sees a semantic failure, per ADR 0016.

M1 and M2 ship together (this change); M3 is deferred.
