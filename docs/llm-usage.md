# Using the LLM more effectively

> Where we are, where the unused leverage is, and what to build next.

See also: [Design principles](design-principles.md) · [Retrieval and injection](retrieval.md) · [Open questions](open-questions.md)

## Current state

Today Yggdrasil makes exactly one kind of LLM call:

```
Ollama.embed(text) → 384-dim vector
```

That call fires from `ygg inject` (per user turn, per prompt) and from `ygg digest` (per completed session, per turn block). Everything else in the system — task management, lock coordination, correction detection, digest summarization — runs in shell or Rust against the embedded SQLite database. No inference.

Nothing about this is wrong. It's the [substrate-separation](design-principles.md#substrate-separation) principle in its purest form: the cheap substrate handles everything the cheap substrate can handle, and the expensive substrate (the agent) never pays for housekeeping. But *zero* inference below the agent is too far in the cheap direction — there are jobs that a small local model does dramatically better than heuristics, and we're leaving that capability on the table.

The question isn't "should we use the LLM more." It's **"where does one small additional inference call earn back more than it costs?"** That question has five concrete answers.

## Five underused capabilities

### 1. A relevance classifier for similarity hits

The biggest single source of [pollution risk](open-questions.md) is raw cosine distance as the only filter on `ygg inject`. Cosine-near doesn't mean useful-here. The literature is unambiguous: dense retrieval recall is high, precision is medium; a small cross-encoder reranker moves precision 2-5× with negligible latency for top-k re-scoring.

Concrete shape:

```
top-k from pgvector (k=20)
   → for each candidate: Ollama.score(user_prompt, candidate_snippet) → 0..1
   → keep candidates where score ≥ threshold (say 0.6)
   → truncate to top 3-5 of those
   → inject
```

Run against the same Ollama already running. No new dependency. Latency budget: 20 candidates × ~30ms each on CPU Ollama = ~600ms wall clock; doable because it runs between user prompt and assistant turn, not during the assistant's reasoning.

Label-collection angle: every injected hit that the agent *references in its next turn* is a positive; every hit that's ignored becomes a negative. We collect training data passively and can fine-tune a purpose-built scorer over time. The initial classifier doesn't need to be trained — `nomic-embed-text` or similar can do zero-shot pairwise scoring just by prompting.

**Priority: highest.** This is the single change most likely to turn current pollution concerns into non-issues.

### 2. LLM-generated digests (replacing heuristic extraction)

`ygg digest` today pattern-matches transcripts for "stop," "no," "actually," "correct" tokens to detect corrections, with a simple counter for reinforcements. That's why digest quality is uneven — heuristics don't know what was *resolved*, only what was *said*.

An LLM digest pipeline:

```
transcript → Ollama.generate(prompt="Summarize this session in 4 sections: 
    orientation (what we set out to do), 
    progress (what changed), 
    corrections (what we got wrong and how we fixed it),
    open threads (what's unresolved). 
    ≤80 words each.")
→ JSON-formatted output → write as a Digest node
```

Digest nodes are the load-bearing input to `ygg prime` at session start. Better digests → better next-session orientation → fewer "wait, what was I working on?" restarts. This is the compounding win.

Ollama supports structured JSON output via `format: json`, so the four sections land as parseable fields without regex surgery.

**Priority: high.** Directly improves the context-rot mitigation story in [Open questions](open-questions.md).

### 3. HyDE-style query expansion for `ygg inject`

[HyDE](https://arxiv.org/abs/2212.10496) (Hypothetical Document Embeddings) is the retrieval trick of having the model generate a *plausible answer* to the user's question, then embedding *that* instead of (or alongside) the raw question. It works because answers and answer-shaped past content cluster tighter than questions and answer-shaped past content.

For `ygg inject`:

```
user prompt: "how should I handle the auth refactor rollout?"
  ↓
Ollama.generate("Write a short plausible answer:") → 
  "We'll use a feature flag and staged rollout; the auth middleware change 
   needs a backwards-compatible shim for sessions created before the migration."
  ↓
embed(hypothetical_answer) → query vector
  ↓
pgvector search
```

Cheap (~200ms for a short generation), strictly additive — combine with the raw-prompt embedding for a hybrid query. In the retrieval literature, HyDE typically adds 5-20% recall@k at low cost.

**Priority: medium.** Compounds with the classifier (better candidates in, better scoring out).

### 4. Structured extraction for task creation

`ygg task create "title"` today takes whatever title the user gives it. A small LLM call could auto-suggest:

- **Kind** (task / bug / feature / chore / epic) — classifiable from text in 2 tokens
- **Priority** — often signalled by phrases ("critical", "blocks release", "nice to have")
- **Labels** — extractable from domain terms ("auth", "migration", "docs")
- **Acceptance criteria** — can be drafted from a feature description

Ollama `format: json` returns `{"kind": "bug", "priority": 1, "labels": ["auth", "rollout"], "suggested_acceptance": "..."}`. User overrides on the CLI; defaults are the LLM's guess. Makes task capture friction-free.

**Priority: medium-low.** Quality-of-life, not load-bearing.

### 5. Weekly / per-repo rollup summaries

For a human operator (or for an agent orienting into an unfamiliar repo), the most useful document is often the one that doesn't exist yet: *"what has happened in this repo recently, summarized."* Today `ygg prime` shows the single most recent digest. A rolling summary is one LLM call away:

```
fetch: last N digest nodes for this repo, last M remembered directives
→ Ollama.generate("Roll these up into a weekly summary, grouped by theme, ≤200 words")
→ write as a rollup node, kind='digest', tag='weekly'
```

Surfaced in `ygg prime` and in the TUI dashboard. Scheduled via `ygg watcher` on a cron-ish interval.

**Priority: low.** Useful, not urgent.

## What we should NOT add

The temptation with a local model is to reach for it whenever anything is vaguely fuzzy. Resist. Substrate-separation cuts both ways:

- **Don't use the LLM for lock decisions.** A SQL `UNIQUE` constraint is already correct. Adding a "does this lock conflict" LLM call is worse on every axis.
- **Don't use the LLM for task dependency cycles.** The recursive CTE catches every cycle in O(edges). Asking a model to reason about it is slower and less correct.
- **Don't use the LLM for exact-match work.** If grep or `tsvector` answers the question, ship grep.
- **Don't pipe every tool call through the LLM.** Most tool results carry their own semantics — don't paraphrase output the agent will read directly anyway.

## Log + TUI surface (the observability side)

The effectiveness question has a flip side: even the calls we make are under-surfaced. `ygg logs --follow` today shows embedding calls, similarity hits, locks, digests, hook fires, node writes, corrections. Good baseline. Gaps worth closing as we add the above:

- **Task lifecycle events.** `TaskCreated`, `TaskStatusChanged`, `TaskClosed` with the `<prefix>-<seq>` ref and title snippet. Surfaces work movement inline with conversation events.
- **Relevance-classifier scores** per injected hit, as an optional column in the `SimilarityHit` line. Makes threshold tuning a human-in-the-loop exercise instead of a cold-start configuration.
- **Cache events** (embedding-cache hit/miss, query-cache hit/miss, negative-cache hit) once caching lands. Tells you immediately how much inference you're saving.
- **Pressure crossings.** `ContextPressure` event when an agent crosses 30/50/70% thresholds — prerequisite for the epoch-reflection pipeline in [Design principles](design-principles.md).
- **Digest quality signals.** When a digest lands, show the four section lengths; long "open threads" sections are a hint that something was left unfinished.
- **Rollup triggers.** When a weekly/per-repo rollup writes, log which digests it consumed.

For the TUI dashboard, the natural additions mirror the above plus some cross-cutting views:

- A **tasks pane** (current repo's open/in-progress/blocked tasks, sortable by priority) alongside the existing agents pane.
- A **retrieval inspector** — for a selected user prompt, show the candidate set *before* classifier filtering and the scores each got. Makes the retrieval pipeline legible.
- A **substrate meter** — tokens saved via cache hits vs. tokens spent on classification vs. tokens spent on generation. Concrete feedback on how well substrate-separation is working.
- A **"ready to do" strip** at the top — combined ready tasks + pending interrupts + lock contention. One glance, one decision.

## Proposed order of attack

1. **Task-lifecycle events in logs/TUI.** Cheap, useful immediately, zero new inference. *Ships alongside existing tasks work.*
2. **Relevance classifier** for `ygg inject`. Highest-leverage LLM addition. Ship behind a feature flag; compare inject outputs with/without.
3. **LLM-generated digests.** Swap the heuristic extractor. Measure digest length + section-fill-rate as quality proxies.
4. **Embedding cache.** Cheap to build (sha256 → vector table), immediate token savings. Not an LLM feature but compounds with every other.
5. **HyDE query expansion** — gated by the classifier. Combine with raw-prompt embedding.
6. **Structured task extraction** + **weekly rollups**. Polish features; ship after the first four stabilize.

Each of these is a real ADR waiting to be written (ADR 0011 onward). The classifier, digests, and cache together probably constitute the v2 retrieval story.
