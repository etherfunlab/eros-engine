-- SPDX-License-Identifier: AGPL-3.0-only
-- Teardown of engine.companion_insights (spec 2026-08-11):
--   1. reconciliation backfill of human_insights from the JSONB. The JSONB
--      WINS on conflict, because it is the source and human_insights is the
--      projection of it: the old write path committed the companion_insights
--      merge first and then treated a failed project_from_insights as a
--      `warn!`, so any user whose projection ever failed has a stale typed
--      row and an authoritative blob. Letting the typed row win would pin
--      that user to the stale value and then destroy the good one at step 2,
--      silently and permanently. Only genuinely absent JSONB values (NULL
--      scalar, empty array) leave the existing typed value in place.
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

-- 1. Reconciliation backfill. Array/typeof guards mirror 0018.
--
-- Array elements are filtered for NULL: `jsonb_array_elements_text` turns a
-- JSON `null` element into a SQL NULL, and `ARRAY(...)` would keep it, giving
-- a text[] with a NULL element. The Rust rows type these columns `Vec<String>`
-- (human_insight.rs), so sqlx cannot decode such a row — every later profile
-- read, prompt build and reverse projection for that user would fail, with the
-- source JSON already dropped at step 2. `["coffee", null]` is enough.
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
             THEN ARRAY(SELECT v FROM jsonb_array_elements_text(ci.insights->'interests') AS t(v)
                        WHERE v IS NOT NULL) END,
        '{}'
    ),
    COALESCE(
        CASE WHEN jsonb_typeof(ci.insights->'personality_traits') = 'array'
             THEN ARRAY(SELECT v FROM jsonb_array_elements_text(ci.insights->'personality_traits') AS t(v)
                        WHERE v IS NOT NULL) END,
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
             THEN ARRAY(SELECT v FROM jsonb_array_elements_text(ci.insights->'matching_preferences'->'deal_breakers') AS t(v)
                        WHERE v IS NOT NULL) END,
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
-- EXCLUDED (the JSONB source) wins; the typed row only survives where the
-- source has nothing to say. Empty array == absent, matching the scalars'
-- NULL: these columns are NOT NULL DEFAULT '{}', so COALESCE never fires on
-- them and the emptiness test has to be explicit.
ON CONFLICT (user_id) DO UPDATE SET
    city                 = COALESCE(EXCLUDED.city, human_insights.city),
    occupation           = COALESCE(EXCLUDED.occupation, human_insights.occupation),
    mbti_guess           = COALESCE(EXCLUDED.mbti_guess, human_insights.mbti_guess),
    love_values          = COALESCE(EXCLUDED.love_values, human_insights.love_values),
    emotional_needs      = COALESCE(EXCLUDED.emotional_needs, human_insights.emotional_needs),
    life_rhythm          = COALESCE(EXCLUDED.life_rhythm, human_insights.life_rhythm),
    interests            = CASE WHEN EXCLUDED.interests = '{}'
                                THEN human_insights.interests ELSE EXCLUDED.interests END,
    personality_traits   = CASE WHEN EXCLUDED.personality_traits = '{}'
                                THEN human_insights.personality_traits ELSE EXCLUDED.personality_traits END,
    preferred_gender     = COALESCE(EXCLUDED.preferred_gender, human_insights.preferred_gender),
    age_min              = COALESCE(EXCLUDED.age_min, human_insights.age_min),
    age_max              = COALESCE(EXCLUDED.age_max, human_insights.age_max),
    deal_breakers        = CASE WHEN EXCLUDED.deal_breakers = '{}'
                                THEN human_insights.deal_breakers ELSE EXCLUDED.deal_breakers END,
    location             = COALESCE(EXCLUDED.location, human_insights.location),
    hometown             = COALESCE(EXCLUDED.hometown, human_insights.hometown),
    nationality          = COALESCE(EXCLUDED.nationality, human_insights.nationality),
    education            = COALESCE(EXCLUDED.education, human_insights.education),
    family               = COALESCE(EXCLUDED.family, human_insights.family),
    relationship_history = COALESCE(EXCLUDED.relationship_history, human_insights.relationship_history),
    social_pattern       = COALESCE(EXCLUDED.social_pattern, human_insights.social_pattern),
    future_plans         = COALESCE(EXCLUDED.future_plans, human_insights.future_plans),
    finance_status       = COALESCE(EXCLUDED.finance_status, human_insights.finance_status),
    -- Was `human_insights.updated_at` when the typed row won and nothing could
    -- change. Now a conflicting row can actually be rewritten, so the stamp has
    -- to move or it would claim a freshness the values no longer have.
    updated_at           = EXCLUDED.updated_at;

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
