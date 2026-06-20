# Architecture Decision Records

This directory holds the non-obvious architectural choices behind Yggdrasil, in the [Markdown Any Decision Records (MADR)](https://adr.github.io/madr/) style. Each record captures the context, the decision, and the consequences — especially the ones we'll regret if we forget.

ADRs are append-only. Superseded decisions get a new ADR that references the old one; the old file stays in place for provenance.

## Index

| #    | Title                                                            | Status    |
|------|------------------------------------------------------------------|-----------|
| 0001 | [Postgres + pgvector as the single source of truth](0001-postgres-pgvector-single-store.md) | Accepted  |
| 0002 | [Local Ollama embeddings over API-hosted models](0002-local-ollama-embeddings.md) | Accepted  |
| 0003 | [Explicit resource leases over optimistic coordination](0003-lock-graph-coordination.md) | Accepted  |
| 0004 | [Conversation as a typed DAG, not a flat transcript](0004-conversation-dag.md) | Accepted  |
| 0005 | [Shell hooks over MCP server or Agent SDK plugin](0005-shell-hook-integration.md) | Accepted  |
| 0006 | [Dogfood Yggdrasil; remove beads directives from this repo](0006-dogfood-drop-beads.md) | Accepted  |
| 0007 | [tmux as the multi-agent display substrate](0007-tmux-as-substrate.md) | Accepted  |
| 0008 | [One global database across repos; agents auto-keyed by pwd](0008-shared-db-across-repos.md) | Accepted (partially superseded by 0009) |
| 0009 | [Repos and sessions as first-class dimensions](0009-repos-and-sessions-first-class.md) | Accepted (partially superseded by 0013) |
| 0010 | [Tasks: beads replacement inside Yggdrasil](0010-tasks-beads-replacement.md) | Accepted |
| 0011 | [Relevance classifier for retrieval gating](0011-relevance-classifier.md) | Accepted |
| 0012 | [Mechanical scoring as the primary retrieval precision mechanism](0012-mechanical-scoring.md) | Accepted |
| 0013 | [Split agent identity from CC session state](0013-session-state-split.md) | Accepted |
| 0014 | [First-class scoped memories](0014-scoped-memories.md) | Accepted |
| 0015 | [Retrieval scope reduction — pivot toward orchestrator-only](0015-retrieval-scope-reduction.md) | Proposed |
| 0016 | [Autonomous execution — scheduler + durable task runs](0016-autonomous-execution.md) | Proposed |
| 0017 | [Learnings: approval gate + agent-proposed capture](0017-learning-capture-and-approval.md) | Proposed |
| 0018 | [Task acceptance as a Definition-of-Done checklist](0018-task-acceptance-checklists.md) | Proposed |

## Writing a new ADR

1. Copy the most recent ADR as a template.
2. Number sequentially (zero-padded to four digits).
3. Keep it short — one page is the target. If it needs to be longer, the decision probably isn't crisp yet.
4. State the alternatives you rejected and *why*. Future-you needs to know what you considered, not just what you picked.
5. Link from this index.
