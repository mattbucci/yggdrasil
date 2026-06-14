-- Re-add durable notes for `ygg remember` (post-ADR-0015).
--
-- ADR 0015 removed the embedding/similarity `nodes` corpus. This table is the
-- replacement substrate for `ygg remember`: a plain, human-readable note store
-- with NO embeddings and NO similarity retrieval. Notes surface deterministically
-- — recent repo + global notes in `ygg prime` (SessionStart) and `ygg remember
-- --list`. repo_id NULL = global (visible from every repo).
CREATE TABLE memories (
    memory_id   UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    repo_id     UUID REFERENCES repos(repo_id) ON DELETE CASCADE,
    text        TEXT NOT NULL,
    created_by  UUID REFERENCES agents(agent_id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    user_id     TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_memories_repo ON memories (repo_id, created_at DESC);
