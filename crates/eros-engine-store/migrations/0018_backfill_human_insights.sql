-- SPDX-License-Identifier: AGPL-3.0-only
-- One-time backfill: project existing companion_insights JSONB into the flat
-- human_insights mirror for users missing a row. Idempotent — only fills gaps
-- (ON CONFLICT DO NOTHING); write-through keeps fresher rows authoritative.
-- Mirrors the Rust project_columns() projection (human_insight.rs).
INSERT INTO engine.human_insights (
    user_id, city, occupation, mbti_guess, love_values, emotional_needs,
    life_rhythm, interests, personality_traits, preferred_gender,
    age_min, age_max, deal_breakers, updated_at
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
        ARRAY(SELECT jsonb_array_elements_text(ci.insights->'interests')),
        '{}'
    ),
    COALESCE(
        ARRAY(SELECT jsonb_array_elements_text(ci.insights->'personality_traits')),
        '{}'
    ),
    ci.insights->'matching_preferences'->>'preferred_gender',
    CASE
        WHEN jsonb_typeof(ci.insights->'matching_preferences'->'age_range'->0) = 'number'
        THEN (ci.insights->'matching_preferences'->'age_range'->>0)::int
    END,
    CASE
        WHEN jsonb_typeof(ci.insights->'matching_preferences'->'age_range'->1) = 'number'
        THEN (ci.insights->'matching_preferences'->'age_range'->>1)::int
    END,
    COALESCE(
        ARRAY(SELECT jsonb_array_elements_text(ci.insights->'matching_preferences'->'deal_breakers')),
        '{}'
    ),
    now()
FROM engine.companion_insights ci
ON CONFLICT (user_id) DO NOTHING;
