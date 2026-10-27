-- SPDX-License-Identifier: AGPL-3.0-only
-- Insights 2.0 backfill: carry what predates the fact ledger into it, once.
--
--   1. events  — the facts the old extractor logged in companion_insights_events
--   2. seeds   — the current profile values, as legacy facts
--   3. pending — one profile rebuild queued per user with a profile row
--
-- Only users that currently have a human_insights row are touched: that row is
-- the one liveness marker the engine has, and a user already cleaned up by the
-- deployment must not be resurrected from audit data. A deployment with no such
-- rows backfills nothing.
--
-- The work lives in a function so it can be replayed: every insert is
-- ON CONFLICT DO NOTHING, a second call reports 0 rows per step, and the
-- migration tests drive it directly. It stays installed for that reason.
--
-- Spec: docs/superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md

CREATE OR REPLACE FUNCTION engine.insights_2_backfill()
RETURNS TABLE (step TEXT, rows BIGINT)
LANGUAGE plpgsql
AS $$
DECLARE
    -- The vocabularies in eros-engine-store/src/human_fact.rs. A label outside
    -- its list folds the same way FactInput::from_json folds it.
    v_domains     TEXT[] := ARRAY[
        'basic_profile', 'career', 'education', 'technology', 'finance', 'family',
        'friendship', 'romance', 'sexuality', 'kink', 'health', 'mental_health',
        'lifestyle', 'hobby', 'ideology', 'personality', 'communication', 'other'];
    v_categories  TEXT[] := ARRAY[
        'fact', 'preference', 'aversion', 'goal', 'value', 'habit', 'skill', 'event',
        'emotion', 'relation', 'communication_style', 'roleplay_tendency',
        'constraint', 'other'];
    v_evidence    TEXT[] := ARRAY[
        'explicit_statement', 'explicit_agreement', 'observed_behavior',
        'roleplay_expression', 'legacy_profile', 'unknown'];
    v_temporality TEXT[] := ARRAY['atemporal', 'current', 'past', 'future', 'unknown'];
    v_persistence TEXT[] := ARRAY['stable', 'ongoing', 'transient', 'one_off', 'unknown'];
    v_confidence  TEXT[] := ARRAY['high', 'medium', 'low', 'unknown'];

    -- What btrim strips: ASCII whitespace, no-break space, ideographic space.
    v_ws TEXT := E' \t\r\n\f\v 　';

    -- The example values the old profile-extraction schema showed the model.
    -- Models echoed them back as if they were the user's own facts, so a stored
    -- value byte-equal to one of them is not evidence about anyone.
    v_examples TEXT[] := ARRAY[
        '深圳南山，工作生活五年多',
        '这周在东京出差',
        '湖南长沙人，大学才离开',
        '中国香港',
        '在深圳一家中厂做后端工程师，常年加班，最近想跳槽',
        '用户自述 INFP',
        '偏 INFP，多次表现出重意义、不爱社交',
        '渴望被理解胜过浪漫仪式，慢热，怕被抛弃所以习惯先推开人',
        '周末常去爬山',
        '沉迷手冲咖啡',
        '养了只橘猫',
        '下班后想有人先听他吐槽、被肯定，不喜欢被讲道理',
        '典型夜猫子，凌晨两三点睡、中午起，靠咖啡和外卖过日子',
        '长期冷暴力',
        '嘴硬心软',
        '难过也说没事',
        '对朋友很讲义气',
        '985 本科计算机，毕业五年',
        '独生子，父母在老家，未婚，和妈妈每周通话',
        '去年和异地恋三年的前任分手，之后一直单身',
        '周末宅家，社交主要靠线上游戏开黑',
        '想两年内跳去外企，攒钱在老家买房',
        '月薪两万出头，房贷压力大'];

    n BIGINT;
BEGIN
    -- 1. events. One candidate per non-blank `content` in a details array. A
    -- fact seen several times becomes one row: counters and first/last
    -- sightings come from the event rows, text and labels from the earliest.
    -- `location_culture` is not a ledger domain, and its facts are dropped
    -- rather than folded into `other`. Pointers are copied only where the target still
    -- exists: 0058 left the audit table's FKs unvalidated, so old rows can name
    -- a deleted session or message. The session still counts as a session.
    INSERT INTO engine.human_facts
        (user_id, content, category, domain, evidence_type, temporality, persistence,
         confidence, occurrences, session_count, first_seen_at, last_seen_at,
         first_session_id, first_message_id, first_run_id,
         last_session_id, last_message_id, last_run_id)
    WITH cand AS (
        SELECT e.id AS event_id, e.user_id, e.created_at, e.run_id,
               e.session_id AS raw_session_id,
               cs.id AS session_ptr,
               cm.id AS message_ptr,
               btrim(d->>'content', v_ws) AS content,
               CASE WHEN d->>'category' = ANY (v_categories)
                    THEN d->>'category' ELSE 'other' END AS category,
               CASE WHEN d->>'domain' IN ('politics', 'religion') THEN 'ideology'
                    WHEN d->>'domain' = ANY (v_domains) THEN d->>'domain'
                    ELSE 'other' END AS domain,
               CASE WHEN d->>'evidence_type' = ANY (v_evidence)
                    THEN d->>'evidence_type' ELSE 'unknown' END AS evidence_type,
               CASE WHEN d->>'temporality' = ANY (v_temporality)
                    THEN d->>'temporality' ELSE 'unknown' END AS temporality,
               CASE WHEN d->>'persistence' = ANY (v_persistence)
                    THEN d->>'persistence' ELSE 'unknown' END AS persistence,
               CASE WHEN d->>'confidence' = ANY (v_confidence)
                    THEN d->>'confidence' ELSE 'unknown' END AS confidence
          FROM engine.companion_insights_events e
         CROSS JOIN LATERAL jsonb_array_elements(
                CASE WHEN jsonb_typeof(e.payload->'details') = 'array'
                     THEN e.payload->'details' ELSE '[]'::jsonb END) d
          LEFT JOIN engine.chat_sessions cs ON cs.id = e.session_id
          LEFT JOIN engine.chat_messages cm ON cm.id = e.message_id
         WHERE e.stage = 'facts'
           AND e.status = 'ok'
           AND e.user_id IN (SELECT user_id FROM engine.human_insights)
           AND jsonb_typeof(d->'content') = 'string'
           AND btrim(d->>'content', v_ws) <> ''
           AND COALESCE(d->>'domain', '') <> 'location_culture'
    )
    SELECT user_id,
           (array_agg(content       ORDER BY created_at, event_id))[1],
           (array_agg(category      ORDER BY created_at, event_id))[1],
           (array_agg(domain        ORDER BY created_at, event_id))[1],
           (array_agg(evidence_type ORDER BY created_at, event_id))[1],
           (array_agg(temporality   ORDER BY created_at, event_id))[1],
           (array_agg(persistence   ORDER BY created_at, event_id))[1],
           (array_agg(confidence    ORDER BY created_at, event_id))[1],
           count(*)::int,
           GREATEST(1, count(DISTINCT raw_session_id))::int,
           min(created_at),
           max(created_at),
           (array_agg(session_ptr ORDER BY created_at, event_id))[1],
           (array_agg(message_ptr ORDER BY created_at, event_id))[1],
           (array_agg(run_id      ORDER BY created_at, event_id))[1],
           (array_agg(session_ptr ORDER BY created_at DESC, event_id DESC))[1],
           (array_agg(message_ptr ORDER BY created_at DESC, event_id DESC))[1],
           (array_agg(run_id      ORDER BY created_at DESC, event_id DESC))[1]
      FROM cand
     GROUP BY user_id,
              (evidence_type = 'roleplay_expression'),
              md5(engine.fact_norm(content))
    ON CONFLICT (user_id, roleplay, content_hash) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    step := 'events'; rows := n; RETURN NEXT;

    -- 2. seeds. The profile columns that survive into 2.0, one row per scalar
    -- column and per array item. The retired columns are not seeded, nor are the
    -- five columns 2.0 added: those are empty until a user's first rebuild.
    -- Placeholder strings and the schema examples above carry no information.
    INSERT INTO engine.human_facts
        (user_id, content, category, domain, evidence_type, temporality, persistence,
         confidence, first_seen_at, last_seen_at)
    SELECT h.user_id,
           c.label || '：' || btrim(item.val, v_ws),
           c.category, c.domain,
           'legacy_profile', 'unknown', 'unknown', 'unknown',
           h.updated_at, h.updated_at
      FROM engine.human_insights h
     CROSS JOIN LATERAL (VALUES
            ('职业',     'career',      'fact',       ARRAY[h.occupation]),
            ('MBTI',     'personality', 'fact',       ARRAY[h.mbti_guess]),
            ('感情观',   'romance',     'value',      ARRAY[h.love_values]),
            ('情感需求', 'romance',     'emotion',    ARRAY[h.emotional_needs]),
            ('作息',     'lifestyle',   'habit',      ARRAY[h.life_rhythm]),
            ('兴趣',     'hobby',       'preference', h.interests),
            ('性格特质', 'personality', 'fact',       h.personality_traits),
            ('择偶雷区', 'romance',     'constraint', h.deal_breakers),
            ('教育',     'education',   'fact',       ARRAY[h.education]),
            ('家庭',     'family',      'fact',       ARRAY[h.family]),
            ('感情经历', 'romance',     'event',      ARRAY[h.relationship_history]),
            ('社交模式', 'friendship',  'habit',      ARRAY[h.social_pattern]),
            ('未来计划', 'other',       'goal',       ARRAY[h.future_plans]),
            ('经济状况', 'finance',     'fact',       ARRAY[h.finance_status])
          ) AS c(label, domain, category, vals)
     CROSS JOIN LATERAL unnest(c.vals) AS item(val)
     WHERE btrim(item.val, v_ws) <> ''
       AND lower(btrim(item.val, v_ws)) NOT IN ('string', 'null', 'unknown', 'n/a')
       AND btrim(item.val, v_ws) NOT IN ('未知', '无')
       AND btrim(item.val, v_ws) <> ALL (v_examples)
    ON CONFLICT (user_id, roleplay, content_hash) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    step := 'seeds'; rows := n; RETURN NEXT;

    -- 3. pending. Each profile row is rebuilt once from the ledger. 20 is the
    -- rebuild threshold, and last_fact_at a day back makes the idle sweeper see
    -- the queue as due; where the sweeper is off, the threshold trips on the
    -- user's next turn instead.
    INSERT INTO engine.human_profile_state (user_id, pending_facts, last_fact_at)
    SELECT user_id, 20, now() - interval '1 day'
      FROM engine.human_insights
    ON CONFLICT (user_id) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    step := 'pending'; rows := n; RETURN NEXT;
END
$$;

-- Run it once and report the counts. `pending` is the number of profile
-- rebuilds the idle sweeper will make, i.e. the LLM calls to expect after the
-- deploy.
DO $$
DECLARE
    r RECORD;
    summary TEXT := '';
BEGIN
    FOR r IN SELECT * FROM engine.insights_2_backfill() LOOP
        summary := summary || ' ' || r.step || '=' || r.rows;
    END LOOP;
    RAISE NOTICE 'insights_2_backfill:%', summary;
END
$$;
