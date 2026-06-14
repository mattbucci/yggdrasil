# ADR 0015 — Retrieval scope reduction (pivot toward orchestrator-only)

**Status:** implemented. Phase 1 landed via yggdrasil-76; Phases 2–4
landed 2026-06-07 (yggdrasil-77/78/79) — the `nodes`, `memories`, and
`embedding_cache` tables, the `tasks.embedding` column, pgvector, and the
Ollama dependency are all gone. `ygg remember` / `ygg memory` / `ygg
trace` / `ygg eval` / `ygg digest` / `ygg observe` / `ygg recovery-test`
and the legacy `ygg run` loop were removed. Task dupe-detection now uses
token-set (Jaccard) string similarity instead of embeddings.
**Date:** 2026-04-18 (decision); 2026-06-07 (implementation)

> **Amendment 2026-06-14 — `ygg remember` re-added (without embeddings).**
> The command is back, but as a *plain note store*, not the retrieval
> corpus this ADR removed. A new `memories` table
> (`migrations/20260614000001_add_memories.sql`) holds free-text notes
> scoped to a repo (or global); there is **no embedding column, no
> similarity search, no Ollama**. Retrieval is deterministic: recent
> repo + global notes are listed newest-first in `ygg prime`
> (SessionStart) and via `ygg remember --list`. This sidesteps every
> failure mode the ADR documented — corpus poisoning requires a
> retriever that feeds its own writes back into ranking, and there is no
> ranking here. It restores the ergonomics of pinning a durable
> directive (`ygg remember "..."`) that the four-phase removal took
> away, while keeping the "agents communicate via tasks, not retrieved
> prior turns" stance intact. The removed `ygg memory` /
> CLAUDE.md-include path (Phase 2) is unchanged and remains the home for
> instructions Claude Code itself should load every session; `remember`
> is for shorter cross-session notes surfaced in the prime block.

## Context

Yggdrasil started as two bets: (1) be the coordination layer for many
Claude Code agents; (2) be their cross-session similarity-retrieved
memory. Bet (1) has held up. Bet (2) has accumulated enough negative
evidence that continuing to invest in it costs more than it returns.

### Evidence against retrieval-as-core-value

- **Pinned-memory injection needed a force-path.** Similarity alone did
  not reliably surface directives the user explicitly pinned
  (yggdrasil-56 era: inject.rs now lists pinned rows unconditionally
  before doing the similarity pass). A retriever that the user has to
  work around to guarantee delivery is a weak retriever.

- **Corpus poisoning is a structural risk, not a tuning knob.** Terse
  and caveman experiments in this session's history showed the same
  pattern: anything we write into the retriever corpus (digests, terse
  memories, summaries) feeds future retrievals. Silent drift over weeks
  is attribution-hard and unfixable after the fact. The adversarial
  review explicitly flagged this as the reason to refuse a global
  pinned terse-rule.

- **Local-LLM generation corrupts identifiers.** Prior caveman tests
  produced `kind_boost → KIND_BOSTON` class errors. This killed the
  compaction feature (yggdrasil-62, won't-do) and flipped llm_digest
  to opt-in (yggdrasil-74). Embeddings are safe; generation is not.
  But embeddings without downstream generation buy less than they cost
  in ops complexity (Ollama daemon, pgvector, HNSW indexes, embedding
  cache, hybrid-retrieval).

- **Claude Code already ships durable memory.** `MEMORY.md` and
  `CLAUDE.md` load into every system prompt at session start. Prompt
  caching survives the session. `/resume` restores mid-session. For
  cross-agent/cross-session durable rules, our memories table
  duplicates what the native file-based system already does — and
  the file-based system wins on visibility (`cat MEMORY.md`) and
  revertability (`git`).

- **Retrieval competes with native tools.** Claude already uses Grep,
  Read, and specialized agents to find relevant code. Our per-turn
  `[ygg memory | ...]` injections can distract more than they help
  when the model was going to search the right way on its own.

### The orchestration primitives have held up

No erosion of conviction on:

- `ygg task` DAG with epic rollup, external refs, dupe detection, labels
- `ygg lock` coordination across agents
- `ygg spawn` + session-per-worker + observer reconciliation
- `ygg status` / dashboard as a read view over shared state
- Cross-repo shared Postgres (ADR 0008) as the coordination substrate

These are what actually differentiate Yggdrasil from beads, from
Claude Code's native agents, and from ad-hoc tmux.

## Decision

Scope down the retrieval layer in four staged phases. Each phase is
independently reversible — revert the commit and the prior behavior
returns. At each phase, pause long enough to notice regressions in
agent behavior before continuing.

### Phase 1 — `YGG_INJECT=off` default · **landed yggdrasil-76**

Flip the per-turn similarity-inject hook to opt-in. Pinned memories
still surface (separate code path); per-turn top-K node hits stop.

`src/cli/inject.rs::inject_enabled()` reads `YGG_INJECT` from the
environment and treats `on` / `1` / `true` / `yes` as enabled. Default
(unset) is off. Both the vector-search section and the similarity-matched
non-pinned-memory section sit behind that gate. Pinned memories,
context-pressure warnings, and lock-status output stay on unconditionally.

Measures:
- `ygg eval` hit-quality numbers should not regress against baseline
  since the inject output stops flowing but nothing consumed it in a
  way that's measurable here.
- Agent subjective quality — does Claude ask clarifying questions
  more, reference prior sessions less? If no noticeable change in
  1 week of use, continue with Phase 2.

### Phase 2 — drop `memories` table, migrate to CLAUDE.md includes

`ygg memory create --scope global` stops writing to Postgres. The pin
flow becomes: append a line to `CLAUDE.md` (or a linked file under
`.claude/rules/*.md`), which Claude Code already loads.

Global pins → `~/.claude/CLAUDE.md`. Repo pins → project `CLAUDE.md`.
Session pins don't need an analog — they're by definition ephemeral.

Measures: `ygg memory list` still works as a reader over the files
(for the TUI prompt-inspector pane we just shipped). If users still
get the pinned-directive ergonomics, continue.

### Phase 3 — drop `llm_digest`, `HyDE`, `classifier`

All three are already opt-in or opt-in-adjacent after yggdrasil-74.
Phase 3 removes the code paths entirely. Sessions end without writing
digest nodes; inject stops calling the classifier; HyDE is deleted.

Measures: the classifier's relevance filtering was providing precision
gains on top of mechanical scoring (ADR 0011). With inject already
off by Phase 1, the relevance classifier has nothing to filter —
removal is consistent with Phase 1's scope, not a new trade-off.

### Phase 4 — drop `nodes`, `pgvector`, embedding cache

Gut the retrieval substrate. This is the irreversible phase in
practical terms — the schema change is large enough that backing out
means re-importing history. Don't do this phase until Phases 1–3
have run for weeks with no regression.

Schema churn:
- `DROP TABLE nodes, embedding_cache, hit_referenced, classifier_event,
  scoring_event, ... ` (and their indexes)
- Remove HNSW index infrastructure (`CREATE EXTENSION vector` still
  needed only if `memories` table stuck around, which it didn't after
  Phase 2)
- `src/embed.rs`, `src/hyde.rs`, `src/task_classify.rs`,
  `src/scoring.rs`, `src/references.rs`, `src/cli/inject.rs`,
  `src/cli/trace_cmd.rs`, `src/tui/memgraph_view.rs`,
  `src/tui/trace_view.rs`, `src/tui/query_view.rs`, `src/tui/eval_view.rs` — all delete-candidates
- Ollama dependency removed from `ygg init` entirely

Task dupe detection no longer uses embeddings. `tasks.embedding` was
dropped in Phase 4 (see
`migrations/20260607000001_drop_embeddings_and_nodes.sql`); dupe
detection now uses token-set (Jaccard) string similarity on
title+description.

## Consequences

**What we gain:**
- ~40% smaller codebase surface
- No Ollama runtime dependency (except task dupe embedding, which could
  downgrade to Levenshtein/Jaccard on title+description if desired)
- Zero corpus-poisoning failure mode
- `ygg init` installs nothing beyond Postgres + the binary
- Every coordination feature continues working unchanged

**What we give up:**
- Cross-agent similarity-retrieved recall. The mental model shifts to
  "agents communicate via tasks, not via retrieved prior turns."
- Memgraph pane, Trace pane, Eval pane — all become dead UI; either
  remove or repurpose them for task/agent analytics
- `ygg query` free-form similarity search
- The `[ygg memory | sim=N%]` injection mechanic that's currently a
  defining visual of using Yggdrasil

**What stays unchanged:**
- `ygg task` family — DAG, deps, epics, labels, external refs, dupe detection
- `ygg lock`, `ygg spawn`, worker reconciliation, tmux session-per-worker
- `ygg status`, dashboard, workers panel, sessions panel
- Shared-DB coordination semantics (ADR 0008)

## Follow-up

Each phase gets a child task under the pivot epic. Phase 1 is the
reversible canary — ship it, live with it for a week, then decide.

## Relationship to prior ADRs

- **Supersedes** ADR 0001 (Postgres + pgvector single store) in the
  direction of scope — pgvector stays but only for tasks, not for nodes
- **Supersedes** ADR 0002 (local-Ollama embeddings) after Phase 3 —
  Ollama stops being a runtime dependency after the last embedding path
  for nodes is removed
- **Supersedes** ADR 0011 (relevance classifier) after Phase 3
- **Supersedes** ADR 0014 (scoped memories) after Phase 2 — the scope
  semantics move from a database table to Claude's file-based memory
- **Keeps** ADR 0007 (tmux as substrate), ADR 0008 (shared DB across
  repos), ADR 0009 (repos + sessions first-class), ADR 0010 (tasks as
  beads replacement) — all pure-orchestration, untouched
