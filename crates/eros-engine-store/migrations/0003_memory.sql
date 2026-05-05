-- SPDX-License-Identifier: AGPL-3.0-only
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                       -- NULL = profile layer (cross-session)
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_memories_user_profile ON companion_memories(user_id) WHERE instance_id IS NULL;
CREATE INDEX idx_memories_session ON companion_memories(session_id) WHERE instance_id IS NOT NULL;
CREATE INDEX idx_memories_embedding ON companion_memories
    USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);
