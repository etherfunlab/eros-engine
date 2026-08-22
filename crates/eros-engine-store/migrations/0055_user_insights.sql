-- SPDX-License-Identifier: AGPL-3.0-only
-- user_insights — what the real user has revealed inside ONE relationship,
-- keyed on persona_instances.id. The mirror of character_insights: one
-- instance, two rows, the two sides of one relationship.
--
-- NOT the same fact as engine.human_insights. That table is keyed on user_id
-- and answers "who is this user", globally; it feeds prompt injection and
-- user<->user matching and is untouched by this migration. This table answers
-- "what did this user reveal HERE", is read by nothing but its endpoint, and
-- is never injected into a prompt. Two scopes, two tables, one authority each.
--
-- The name says `user` where the project vocabulary (spec 2026-08-15 §1) says
-- `human`. Deliberate: human_insights already owns the global slot under the
-- correct word, and renaming a live table to free it is a breaking change
-- bought for tidiness.
--
-- personal_values, NOT values: VALUES is a reserved word in Postgres, and a
-- quoted column name would have to be quoted in every hand-written statement
-- in this codebase.
--
-- Spec: docs/superpowers/specs/2026-08-22-user-insights-and-api-v2-design.md

CREATE TABLE engine.user_insights (
    instance_id       UUID PRIMARY KEY
                      REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    location          TEXT,
    occupation        TEXT,
    current_situation TEXT,
    desires           TEXT,
    vulnerabilities   TEXT,
    habits            TEXT,
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
-- stage values name their config block ('extraction' => [tasks.user_insight_extraction],
-- 'structuring' => [tasks.user_insight_structuring]), matching
-- character_insights_events. companion_insights_events keeps its older
-- 'facts'/'structured' vocabulary; that divergence is recorded and deliberate.
--
-- No FK on instance_id: the trail must outlive the instance it describes.
-- No owner_uid column: while the instance exists, owner_uid is derivable by
-- joining persona_instances, and it never changes for a given instance. The
-- consequence of the missing FK: rows whose instance has since been deleted
-- are no longer attributable. Acceptable while nothing reads this table by
-- owner; adding owner_uid is the fix if that changes.
CREATE TABLE engine.user_insights_events (
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

CREATE INDEX idx_user_insights_events_instance_time
    ON engine.user_insights_events (instance_id, created_at DESC);
CREATE INDEX idx_user_insights_events_run
    ON engine.user_insights_events (run_id);

-- State history. Written by apply_extraction itself — there is no sweeper for
-- this table, so the write path is the only writer. snapshot holds
-- to_jsonb(user_insights): the whole row, self-contained, so later ADD COLUMNs
-- need no snapshot migration. captured_at has no DEFAULT; the writer passes
-- the instant explicitly, mirroring the other two snapshot tables.
CREATE TABLE engine.user_insights_snapshot (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id UUID NOT NULL,
    snapshot    JSONB NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX idx_user_insights_snapshot_instance_time
    ON engine.user_insights_snapshot (instance_id, captured_at DESC);

-- Supabase lockdown, mirroring 0013/0015/0025/0047. REVOKEs are wrapped in
-- pg_roles existence checks so non-Supabase Postgres (including the sqlx test
-- DB, where anon/authenticated do not exist) skips them silently. The RLS
-- enable runs unconditionally; with no policy attached, only owner (postgres)
-- and service_role connections can touch the rows.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.user_insights          FROM anon;
        REVOKE ALL ON engine.user_insights_events   FROM anon;
        REVOKE ALL ON engine.user_insights_snapshot FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.user_insights          FROM authenticated;
        REVOKE ALL ON engine.user_insights_events   FROM authenticated;
        REVOKE ALL ON engine.user_insights_snapshot FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.user_insights          ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.user_insights_events   ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.user_insights_snapshot ENABLE ROW LEVEL SECURITY;
