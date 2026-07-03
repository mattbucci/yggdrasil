# Design principles

> What guides what goes where in Yggdrasil. Some principles describe what the system already does; others describe where it's headed.

See also: [Retrieval and injection](retrieval.md) · [Open questions](open-questions.md)

## Substrate separation

Inference is the expensive substrate; shell, compiled binaries, file I/O, and embedding math are cheap substrates. *Do the cheap work in a cheap substrate so the expensive substrate can think about things that matter.*

Yggdrasil already operates this way:

- Similarity search is compiled Rust against `pgvector`, not a model call.
- The lock check is a database `UNIQUE` constraint, not an agent decision.
- The digest extractor is a shell-plus-Rust pipeline that hands the agent a finished summary.
- Embedding is Ollama HTTP, not a frontier API call during a tool loop.

Every time we reach for Claude's inference to answer a question the database could answer, we're paying tokens for housekeeping. The design bias is: if a deterministic program can produce the answer, write that program.

## Premises, not rules

Injected context should be *reasoning to work from*, not *instructions to follow* — because premises compose and rules collide.

Our `[ygg memory | …]` lines are premises: prior-context evidence an agent can use, ignore, or argue with. This framing defends auto-injection more cleanly than the "models don't know what they don't know" argument: premises are the form context should take when the system doesn't know the situation well enough to dictate behavior.

A rule says "always do X." It becomes a rule you have to remember, a rule the model has to apply even when X doesn't fit. A premise says "here's what you should know to reason well about this situation" and hands the decision back.

## Smarter, cheaper, fewer tokens

Don't ask the same thing twice. Every inference token has a cost; every re-embedding of identical text is waste; every similarity query that returned the same top-k five minutes ago shouldn't run again. The shared global database makes caching pay off at multiple levels:

- **Embedding cache.** `sha256(text) → vector`. Deterministic and safe. Pays off enormously on `tool_result` nodes that repeat across sessions (build errors, `ls` output, the same `Cargo.toml`). Biggest obvious win; not yet implemented.
- **Similarity-query cache.** `(query_vector, kind_filter, threshold) → top-k node IDs`, with a short TTL (minutes, not days — the corpus grows). Turns a flurry of near-identical prompts into one pgvector hit.
- **Negative cache.** If a query returned zero relevant hits, remember that for a brief window so we don't hammer Ollama and Postgres when a user is mid-thought submitting drafty prompts.
- **Disclosure gate** (see habituation below). Also a cache — "I've already told you this, don't re-surface it yet."
- **Response-level caching** is tempting and dangerous: the corpus is a codebase that changes. Any response cache would need invalidation keyed on file mtime or git rev. Probably not worth it for a long time.

A cache hit is zero tokens; a pgvector query is zero tokens to the model; an Ollama embed is zero tokens to the model. Everything we can push below the inference layer is infrastructure we've earned back.

## Habituation / disclosure gate

Once a piece of guidance or a memory hit has been surfaced to the agent, mark it and suppress re-disclosure until a token-based cooldown elapses. Biological habituation, mechanically enforced.

`ygg inject` today happily re-surfaces the same top hit on every prompt if it stays similar — precisely the signal-becomes-noise failure the pollution discussion in [Open questions](open-questions.md) worries about. A disclosure-gate is a concrete next step; it sits with the classifier work as a precision mechanism.

The cooldown should be measured in *tokens of context consumed since the last disclosure*, not wall-clock, not turns. Attention budget is the thing that's actually scarce; turns under-count for tool-heavy work and over-count for chat-like exchanges.

## Epoch reflections, not only end-of-session digests

Fire reflection at context thresholds — roughly 30% (orientation), 50% (what's changed), 70% (consolidation), and pre-compaction (handoff) — with each reflection becoming one short prose entry in a chronological ledger.

Today our `digest` only fires on `Stop`, which means we miss intermediate structure and rely heavily on compaction timing. Probably the next memory-pipeline ADR.

## The forgetting question

Yggdrasil currently keeps every node — user messages, assistant messages, tool calls, tool results — indefinitely. The alternative discipline is to keep only *reflections* (what was reasoned about) and let everything else evaporate when a session ends.

- Our bet: dense retention plus good retrieval beats selective capture.
- Opposing bet: selective capture plus lightweight retrieval beats retention.

Both bets are defensible and both are testable. Dense retention has more recall headroom; selective capture has less pollution risk. We own the tension; we don't yet have the data to resolve it.
