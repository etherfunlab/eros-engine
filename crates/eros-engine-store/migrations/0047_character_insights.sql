-- SPDX-License-Identifier: AGPL-3.0-only
-- character_insights — the AI character's conversation-derived profile, keyed
-- on the RELATIONSHIP (persona_instances.id), not the genome. Keying on the
-- genome would pool every user's roleplay into one shared character profile:
-- user A's invented detail would unlock for user B, and characters' lines
-- routinely carry the user's own content, so that is a cross-user content leak.
--
-- Deliberately NOT columns here: appearance / background / personality_traits.
-- Their source of truth is persona_genomes.system_prompt, so an extractor that
-- only ever sees turn text can produce nothing but paraphrase-with-
-- embellishment; persisting that is drift, and the drift reads back as fact.
--
-- occupation IS a column and is NOT redundant with the genome or with
-- world_memories. Three different facts coexist: the backstory job
-- (persona_genomes.system_prompt), the job the world director assigned
-- (world_memories), and the job she actually holds in this relationship
-- because the user handed her an offer (here).
--
-- Spec: docs/superpowers/specs/2026-08-15-character-insights-design.md

CREATE TABLE engine.character_insights (
    instance_id       UUID PRIMARY KEY
                      REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    location          TEXT,
    occupation        TEXT,
    current_situation TEXT,
    desires           TEXT,
    vulnerabilities   TEXT,
    habits            TEXT,
    -- personal_values, NOT values: VALUES is a reserved word in Postgres, and
    -- a quoted column name would have to be quoted in every hand-written
    -- statement in this codebase.
    personal_values   TEXT,
    likes             TEXT[] NOT NULL DEFAULT '{}',
    dislikes          TEXT[] NOT NULL DEFAULT '{}',
    relationships     TEXT[] NOT NULL DEFAULT '{}',
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- No indexes on purpose: human_insights carries GIN indexes because matching
-- queries its arrays by set overlap (&&); this table is only ever read by
-- primary key.

-- Append-only audit: ONE row per OpenRouter call that returned a response.
-- The two stages of a run share run_id. A call that never returned (transport
-- error / timeout) writes no row at all.
--
-- stage values name their config block ('extraction' ⇒
-- [tasks.character_insight_extraction], 'structuring' ⇒
-- [tasks.character_insight_structuring]) rather than reusing the human chain's
-- 'facts'/'structured', so reading the audit needs no lookup table.
--
-- No FK on instance_id: the trail must outlive the instance it describes.
-- No owner_uid column: while the instance exists, owner_uid is derivable by
-- joining persona_instances, and it never changes for a given instance. Note
-- the consequence of the missing FK, though: rows whose instance has since been
-- deleted are no longer attributable, because the join has nothing to hit. That
-- is acceptable while nothing reads this table by owner; adding owner_uid would
-- be the fix if these rows ever need per-owner access or deletion.
CREATE TABLE engine.character_insights_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id        UUID NOT NULL,
    instance_id   UUID NOT NULL,
    session_id    UUID,
    message_id    UUID,
    stage         TEXT NOT NULL CHECK (stage IN ('extraction','structuring')),
    status        TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error')),
    payload       JSONB,
    model         TEXT,
    usage         JSONB,
    generation_id TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_character_insights_events_instance_time
    ON engine.character_insights_events (instance_id, created_at DESC);
CREATE INDEX idx_character_insights_events_run
    ON engine.character_insights_events (run_id);

-- State history. Written by apply_extraction itself — there is no sweeper for
-- this table, so the write path is the only writer. snapshot holds
-- to_jsonb(character_insights): the whole row, self-contained, so later
-- ADD COLUMNs need no snapshot migration. captured_at has no DEFAULT; the
-- writer passes the instant explicitly, mirroring human_insights_snapshot.
CREATE TABLE engine.character_insights_snapshot (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id UUID NOT NULL,
    snapshot    JSONB NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_character_insights_snapshot_instance_time
    ON engine.character_insights_snapshot (instance_id, captured_at DESC);

-- Supabase lockdown, mirroring 0013/0015/0025. REVOKEs are wrapped in pg_roles
-- existence checks so non-Supabase Postgres (including the sqlx test DB, where
-- anon/authenticated do not exist) skips them silently. The RLS enable runs
-- unconditionally; with no policy attached, only owner (postgres) and
-- service_role connections can touch the rows.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.character_insights          FROM anon;
        REVOKE ALL ON engine.character_insights_events   FROM anon;
        REVOKE ALL ON engine.character_insights_snapshot FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.character_insights          FROM authenticated;
        REVOKE ALL ON engine.character_insights_events   FROM authenticated;
        REVOKE ALL ON engine.character_insights_snapshot FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.character_insights          ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.character_insights_events   ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.character_insights_snapshot ENABLE ROW LEVEL SECURITY;
