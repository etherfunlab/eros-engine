-- SPDX-License-Identifier: AGPL-3.0-only
-- Teardown of engine.companion_insights (spec 2026-08-11):
--   1. belt-and-braces gap-fill of human_insights from the JSONB (existing
--      human_insights values win; the live mirror should already match),
--   2. drop the table (off-schema JSONB keys are not projected and are
--      discarded with it),
--   3. create human_insights_snapshot; the sweeper now snapshots
--      to_jsonb(human_insights) — companion_insights_snapshot is frozen
--      in place with its history,
--   4. drop chat_sessions.lead_score (sole writer refresh_lead_score is
--      gone; the lead/CTA chain's only input was training_level).
--      Dropping the column breaks OLD binaries' `SELECT *` ChatSession
--      decodes (ColumnNotFound) — run this migration at deploy time with
--      old instances stopping, not hours ahead.

-- 1. Gap-fill. Array/typeof guards mirror 0018.
INSERT INTO engine.human_insights (
    user_id, city, occupation, mbti_guess, love_values, emotional_needs,
    life_rhythm, interests, personality_traits, preferred_gender,
    age_min, age_max, deal_breakers, location, hometown, nationality,
    education, family, relationship_history, social_pattern,
    future_plans, finance_status, updated_at
)
SELECT
    ci.user_id,
    ci.insights->>'city',
    ci.insights->>'occupation',
    ci.insights->>'mbti_guess',
    ci.insights->>'love_values',
    ci.insights->>'emotional_needs',
    ci.insights->>'life_rhythm',
    COALESCE(
        CASE WHEN jsonb_typeof(ci.insights->'interests') = 'array'
             THEN ARRAY(SELECT jsonb_array_elements_text(ci.insights->'interests')) END,
        '{}'
    ),
    COALESCE(
        CASE WHEN jsonb_typeof(ci.insights->'personality_traits') = 'array'
             THEN ARRAY(SELECT jsonb_array_elements_text(ci.insights->'personality_traits')) END,
        '{}'
    ),
    ci.insights->'matching_preferences'->>'preferred_gender',
    -- Digits-only guard on top of the typeof check: a fractional (22.5) or
    -- out-of-int-range JSON number passes `jsonb_typeof = 'number'` but makes
    -- `::int` raise, aborting the whole migration. Degrade such values to
    -- NULL instead, matching parse_age_range's tolerance in Rust.
    CASE
        WHEN jsonb_typeof(ci.insights->'matching_preferences'->'age_range'->0) = 'number'
         AND (ci.insights->'matching_preferences'->'age_range'->>0) ~ '^\d{1,9}$'
        THEN (ci.insights->'matching_preferences'->'age_range'->>0)::int
    END,
    CASE
        WHEN jsonb_typeof(ci.insights->'matching_preferences'->'age_range'->1) = 'number'
         AND (ci.insights->'matching_preferences'->'age_range'->>1) ~ '^\d{1,9}$'
        THEN (ci.insights->'matching_preferences'->'age_range'->>1)::int
    END,
    COALESCE(
        CASE WHEN jsonb_typeof(ci.insights->'matching_preferences'->'deal_breakers') = 'array'
             THEN ARRAY(SELECT jsonb_array_elements_text(ci.insights->'matching_preferences'->'deal_breakers')) END,
        '{}'
    ),
    ci.insights->>'location',
    ci.insights->>'hometown',
    ci.insights->>'nationality',
    ci.insights->>'education',
    ci.insights->>'family',
    ci.insights->>'relationship_history',
    ci.insights->>'social_pattern',
    ci.insights->>'future_plans',
    ci.insights->>'finance_status',
    now()
FROM engine.companion_insights ci
ON CONFLICT (user_id) DO UPDATE SET
    city                 = COALESCE(human_insights.city, EXCLUDED.city),
    occupation           = COALESCE(human_insights.occupation, EXCLUDED.occupation),
    mbti_guess           = COALESCE(human_insights.mbti_guess, EXCLUDED.mbti_guess),
    love_values          = COALESCE(human_insights.love_values, EXCLUDED.love_values),
    emotional_needs      = COALESCE(human_insights.emotional_needs, EXCLUDED.emotional_needs),
    life_rhythm          = COALESCE(human_insights.life_rhythm, EXCLUDED.life_rhythm),
    interests            = CASE WHEN human_insights.interests = '{}'
                                THEN EXCLUDED.interests ELSE human_insights.interests END,
    personality_traits   = CASE WHEN human_insights.personality_traits = '{}'
                                THEN EXCLUDED.personality_traits ELSE human_insights.personality_traits END,
    preferred_gender     = COALESCE(human_insights.preferred_gender, EXCLUDED.preferred_gender),
    age_min              = COALESCE(human_insights.age_min, EXCLUDED.age_min),
    age_max              = COALESCE(human_insights.age_max, EXCLUDED.age_max),
    deal_breakers        = CASE WHEN human_insights.deal_breakers = '{}'
                                THEN EXCLUDED.deal_breakers ELSE human_insights.deal_breakers END,
    location             = COALESCE(human_insights.location, EXCLUDED.location),
    hometown             = COALESCE(human_insights.hometown, EXCLUDED.hometown),
    nationality          = COALESCE(human_insights.nationality, EXCLUDED.nationality),
    education            = COALESCE(human_insights.education, EXCLUDED.education),
    family               = COALESCE(human_insights.family, EXCLUDED.family),
    relationship_history = COALESCE(human_insights.relationship_history, EXCLUDED.relationship_history),
    social_pattern       = COALESCE(human_insights.social_pattern, EXCLUDED.social_pattern),
    future_plans         = COALESCE(human_insights.future_plans, EXCLUDED.future_plans),
    finance_status       = COALESCE(human_insights.finance_status, EXCLUDED.finance_status),
    updated_at           = human_insights.updated_at;

-- 2. The table (and any off-schema keys inside it) goes away.
DROP TABLE engine.companion_insights;

-- 3. New snapshot target. `snapshot` holds to_jsonb(human_insights) — the
-- full row including user_id/updated_at, so each snapshot is self-contained
-- and future column additions need no snapshot migration. captured_at has no
-- DEFAULT: the sweeper passes the fire instant so all rows of a fire share it
-- (same contract as companion_insights_snapshot, which stays frozen in place).
CREATE TABLE engine.human_insights_snapshot (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      UUID        NOT NULL,
    snapshot     JSONB       NOT NULL,
    captured_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_human_insights_snapshot_user_time
    ON engine.human_insights_snapshot (user_id, captured_at DESC);

-- Supabase lockdown, mirroring migration 0013/0021.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.human_insights_snapshot FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.human_insights_snapshot FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.human_insights_snapshot ENABLE ROW LEVEL SECURITY;

-- 4. lead_score: writer deleted; readers (SSE final frame, session list DTO)
-- removed in the same release.
ALTER TABLE engine.chat_sessions DROP COLUMN lead_score;
