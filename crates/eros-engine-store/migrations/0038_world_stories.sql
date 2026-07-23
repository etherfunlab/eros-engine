-- SPDX-License-Identifier: AGPL-3.0-only
--
-- World Stories (spec: docs/superpowers/specs/2026-07-23-world-stories-design.md).
--
-- world_enrollments.stories_enabled — downstream-written per-owner opt-in for
-- the stories layer, same contract as town_enabled.
--
-- engine.persona_story_insights — flat typed life base (persona-side superset
-- of human_insights; the human_insights lesson applied in advance: no opaque
-- JSONB stage) + the story sweeper's per-instance scheduling state
-- (last_run_at / claimed_at, world_states shape).
--
-- engine.persona_story_events — append-only life-progression log; category is
-- director vocabulary, stored verbatim. story_date is the retention key.
--
-- engine.persona_story_memories — 1:1 recall mirror of events (event_id FK),
-- Voyage 512-dim embeddings for chat-time cosine recall.

ALTER TABLE engine.world_enrollments
    ADD COLUMN stories_enabled BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE engine.persona_story_insights (
    instance_id          UUID PRIMARY KEY REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    owner_uid            UUID NOT NULL,

    city                 TEXT,
    location             TEXT,
    hometown             TEXT,
    nationality          TEXT,
    occupation           TEXT,
    mbti_guess           TEXT,
    love_values          TEXT,
    emotional_needs      TEXT,
    life_rhythm          TEXT,
    education            TEXT,
    family               TEXT,
    relationship_history TEXT,
    social_pattern       TEXT,
    future_plans         TEXT,
    finance_status       TEXT,
    interests            TEXT[] NOT NULL DEFAULT '{}',
    personality_traits   TEXT[] NOT NULL DEFAULT '{}',
    preferred_gender     TEXT,
    age_min              INT,
    age_max              INT,
    deal_breakers        TEXT[] NOT NULL DEFAULT '{}',

    work_history         TEXT,
    romance_history      TEXT,
    family_of_origin     TEXT,
    user_relationship    TEXT,

    digest               TEXT NOT NULL DEFAULT '',
    insight_version      INT  NOT NULL DEFAULT 1,
    last_run_at          TIMESTAMPTZ,
    claimed_at           TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_persona_story_insights_owner
    ON engine.persona_story_insights (owner_uid);

CREATE TABLE engine.persona_story_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid   UUID NOT NULL,
    instance_id UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    category    TEXT NOT NULL,
    content     TEXT NOT NULL,
    story_date  DATE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_persona_story_events_instance_time
    ON engine.persona_story_events (instance_id, created_at DESC);

CREATE TABLE engine.persona_story_memories (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid   UUID NOT NULL,
    instance_id UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    event_id    UUID NOT NULL REFERENCES engine.persona_story_events(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    embedding   VECTOR(512) NOT NULL,
    story_date  DATE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_persona_story_memories_instance
    ON engine.persona_story_memories (owner_uid, instance_id);
CREATE INDEX idx_persona_story_memories_embedding
    ON engine.persona_story_memories USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

-- 0013-style lockdown: REVOKE from Supabase browser roles (when present) and
-- enable policy-less RLS. world_enrollments already carries its own lockdown.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.persona_story_insights FROM anon;
        REVOKE ALL ON engine.persona_story_events   FROM anon;
        REVOKE ALL ON engine.persona_story_memories FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.persona_story_insights FROM authenticated;
        REVOKE ALL ON engine.persona_story_events   FROM authenticated;
        REVOKE ALL ON engine.persona_story_memories FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.persona_story_insights ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.persona_story_events   ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.persona_story_memories ENABLE ROW LEVEL SECURITY;
