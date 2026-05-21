-- SPDX-License-Identifier: AGPL-3.0-only
-- Flat, typed projection of the soft (conversation-derived) user profile,
-- shaped for user<->user matching. companion_insights stays the source of
-- truth (JSONB); this table is a derived mirror written by write-through.
--
-- Hard-filter attributes (own gender/age/geo) are intentionally NOT here:
-- they live in the user-self-filled profile table owned outside engine.*
-- and are joined at match time.
CREATE TABLE engine.human_insights (
    user_id            UUID PRIMARY KEY,

    -- soft scalar signal (free text; context / future embedding, not filtered)
    city               TEXT,
    occupation         TEXT,
    mbti_guess         TEXT,
    love_values        TEXT,
    emotional_needs    TEXT,
    life_rhythm        TEXT,

    -- array signal — core matching dimensions (set overlap via &&)
    interests          TEXT[] NOT NULL DEFAULT '{}',
    personality_traits TEXT[] NOT NULL DEFAULT '{}',

    -- flattened matching_preferences: "what the user wants"
    preferred_gender   TEXT,
    age_min            INT,
    age_max            INT,
    deal_breakers      TEXT[] NOT NULL DEFAULT '{}',

    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Only the array-overlap dimensions get indexes. No city index — geo
-- hard-filtering is the profile table's job.
CREATE INDEX idx_human_insights_interests ON engine.human_insights USING GIN(interests);
CREATE INDEX idx_human_insights_traits    ON engine.human_insights USING GIN(personality_traits);

-- Supabase lockdown (mirror of 0013) for the new table.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.human_insights FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.human_insights FROM authenticated;
    END IF;
END
$$;
ALTER TABLE engine.human_insights ENABLE ROW LEVEL SECURITY;
