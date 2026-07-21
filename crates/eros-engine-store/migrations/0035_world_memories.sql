-- SPDX-License-Identifier: AGPL-3.0-only
--
-- World Memories (spec: docs/superpowers/specs/2026-07-21-world-memories-design.md).
--
-- engine.world_enrollments — downstream-managed enablement table. The engine
-- only ever SELECTs it (over the service_role/owner connection); downstream
-- INSERTs/DELETEs rows to enable/disable world simulation per owner. Row
-- present = enabled. Unenrolling stops simulation and injection immediately;
-- accumulated world data is kept so re-enrolling resumes the same world.
--
-- engine.world_states — engine-private: current world seed (opaque LLM JSON),
-- per-instance digests for resident prompt injection, and the director
-- sweeper's scheduling state (claimed_at = SKIP LOCKED claim stamp, same
-- shape as chat_sessions.classification_claimed_at).
--
-- engine.world_memories — per-persona script fragments with Voyage embeddings
-- (512-dim, same as companion_memories) for cosine recall at chat time.
-- script_date (UTC date of the generating round) is the retention key.

CREATE TABLE engine.world_enrollments (
    owner_uid  UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE engine.world_states (
    owner_uid    UUID PRIMARY KEY,
    seed         JSONB NOT NULL,
    digests      JSONB NOT NULL,
    seed_version INT NOT NULL DEFAULT 1,
    last_run_at  TIMESTAMPTZ,
    claimed_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE engine.world_memories (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid   UUID NOT NULL,
    instance_id UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    embedding   VECTOR(512) NOT NULL,
    script_date DATE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_world_memories_owner_instance
    ON engine.world_memories (owner_uid, instance_id);
CREATE INDEX idx_world_memories_embedding
    ON engine.world_memories USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

-- 0013-style lockdown: REVOKE from Supabase browser roles (when present) and
-- enable policy-less RLS so only owner/service_role connections reach rows.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.world_enrollments FROM anon;
        REVOKE ALL ON engine.world_states      FROM anon;
        REVOKE ALL ON engine.world_memories    FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.world_enrollments FROM authenticated;
        REVOKE ALL ON engine.world_states      FROM authenticated;
        REVOKE ALL ON engine.world_memories    FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.world_enrollments ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.world_states      ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.world_memories    ENABLE ROW LEVEL SECURITY;
