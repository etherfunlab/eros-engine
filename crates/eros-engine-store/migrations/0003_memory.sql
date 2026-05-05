-- SPDX-License-Identifier: AGPL-3.0-only
-- pgvector extension is database-wide (lives in `public` schema by default).
-- The CREATE EXTENSION here is idempotent so it's safe even when
-- eros-gateway has already created it in the shared Supabase database.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE engine.companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                       -- NULL = profile layer (cross-session)
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_memories_user_profile ON engine.companion_memories(user_id) WHERE instance_id IS NULL;
CREATE INDEX idx_memories_session ON engine.companion_memories(session_id) WHERE instance_id IS NOT NULL;
CREATE INDEX idx_memories_embedding ON engine.companion_memories
    USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);
