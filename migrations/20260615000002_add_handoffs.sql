-- Session handoffs: an agent's resume note, written right before /clear so the
-- next session picks up where this one left off. One per (repo, agent) — a new
-- save supersedes the prior. Plain text, no embeddings; surfaced at the top of
-- `ygg prime` (SessionStart). See `ygg handoff`.
CREATE TABLE handoffs (
    handoff_id  UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    repo_id     UUID REFERENCES repos(repo_id) ON DELETE CASCADE,  -- NULL = no detected repo
    agent_id    UUID REFERENCES agents(agent_id) ON DELETE CASCADE,
    text        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id     TEXT NOT NULL DEFAULT '',
    -- Exactly one current handoff per (repo, agent). NULLS NOT DISTINCT (PG15+)
    -- so a (NULL repo, agent) key still collides instead of accumulating rows —
    -- a plain UNIQUE treats NULLs as distinct and would not dedupe. Lets `save`
    -- use ON CONFLICT for an atomic upsert (no DELETE+INSERT race under READ
    -- COMMITTED). The constraint's index also serves the (repo_id, agent_id)
    -- lookup, so no separate index is needed.
    UNIQUE NULLS NOT DISTINCT (repo_id, agent_id)
);
