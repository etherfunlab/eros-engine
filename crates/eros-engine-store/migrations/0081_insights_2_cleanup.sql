-- SPDX-License-Identifier: AGPL-3.0-only
-- Insights 2.0 cleanup: the profile columns the fact ledger replaced are
-- dropped, the audit table takes the name of the profile it audits, and the
-- one-shot backfill goes.
--
--   * engine.human_insights   — drops city, location, hometown, nationality,
--                               preferred_gender, age_min, age_max
--   * engine.companion_insights_events -> engine.human_insights_events, with
--                               its indexes and constraints renamed to match
--   * engine.insights_2_backfill() — dropped
--
-- None of the seven columns carries an index or constraint. Rows and ids of
-- the audit table are unchanged. engine.persona_story_insights keeps its own
-- columns of the same names (a different feature), and
-- engine.human_insights_snapshot keeps the old keys inside its history rows.
--
-- Not reversible: a v2.3.x image reads the dropped columns.
--
-- Spec: docs/superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md

ALTER TABLE engine.human_insights
    DROP COLUMN city,
    DROP COLUMN location,
    DROP COLUMN hometown,
    DROP COLUMN nationality,
    DROP COLUMN preferred_gender,
    DROP COLUMN age_min,
    DROP COLUMN age_max;

ALTER TABLE engine.companion_insights_events RENAME TO human_insights_events;

ALTER INDEX engine.idx_companion_insights_events_user_time
    RENAME TO idx_human_insights_events_user_time;
ALTER INDEX engine.idx_companion_insights_events_run
    RENAME TO idx_human_insights_events_run;
ALTER INDEX engine.idx_companion_insights_events_session
    RENAME TO idx_human_insights_events_session;
ALTER INDEX engine.idx_companion_insights_events_message
    RENAME TO idx_human_insights_events_message;
ALTER INDEX engine.idx_companion_insights_events_generation
    RENAME TO idx_human_insights_events_generation;

-- Renaming the primary key constraint renames its index with it.
ALTER TABLE engine.human_insights_events
    RENAME CONSTRAINT companion_insights_events_pkey
    TO human_insights_events_pkey;
ALTER TABLE engine.human_insights_events
    RENAME CONSTRAINT companion_insights_events_status_check
    TO human_insights_events_status_check;
ALTER TABLE engine.human_insights_events
    RENAME CONSTRAINT companion_insights_events_stage_check
    TO human_insights_events_stage_check;
ALTER TABLE engine.human_insights_events
    RENAME CONSTRAINT companion_insights_events_session_id_fkey
    TO human_insights_events_session_id_fkey;
ALTER TABLE engine.human_insights_events
    RENAME CONSTRAINT companion_insights_events_message_id_fkey
    TO human_insights_events_message_id_fkey;
ALTER TABLE engine.human_insights_events
    RENAME CONSTRAINT companion_insights_events_generation_id_fkey
    TO human_insights_events_generation_id_fkey;

DROP FUNCTION engine.insights_2_backfill();
