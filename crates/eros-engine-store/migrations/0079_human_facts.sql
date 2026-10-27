-- SPDX-License-Identifier: AGPL-3.0-only
-- Insights 2.0 schema: the fact ledger and what the derived profile needs.
--
--   * engine.fact_norm        — the one text normaliser the ledger dedups on
--   * engine.human_facts      — one row per distinct fact per user
--   * engine.human_profile_state — per-user rebuild counter and claim
--   * engine.user_erasures    — user-level erasure tombstones
--   * engine.human_insights   — five more profile columns
--   * companion_insights_events.stage — accepts 'profile'
--
-- Spec: docs/superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md

-- Normalisation lives here once; every writer and the dedup index call it, so
-- no caller re-implements it. NFKC folds width and compatibility forms, then
-- case, whitespace and punctuation are dropped. The CJK punctuation is listed
-- by hand because [:punct:] covers non-ASCII punctuation only under some locales.
-- It must stay IMMUTABLE: the generated column below calls it.
CREATE FUNCTION engine.fact_norm(t TEXT) RETURNS TEXT
    LANGUAGE sql IMMUTABLE STRICT
AS $$
    SELECT regexp_replace(
        lower(normalize(t, NFKC)),
        '[[:space:][:punct:]，。！？、；：「」『』（）《》〈〉【】…—～·]',
        '', 'g')
$$;

-- user_id is an attribution column, so no FK: erasure is an explicit endpoint,
-- not a cascade. first_run_id / last_run_id are correlation ids for the
-- extraction run, with no runs table behind them. The six label columns are
-- free TEXT because the vocabulary is validated by the engine and a retired
-- value must not block a write.
--
-- Dedup key: (user_id, roleplay, content_hash). Hashing the normalised text
-- keeps the unique index small; a roleplay line never merges with a
-- first-person statement of the same words.
CREATE TABLE engine.human_facts (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id          UUID NOT NULL,
    content          TEXT NOT NULL,
    category         TEXT NOT NULL,
    domain           TEXT NOT NULL,
    evidence_type    TEXT NOT NULL,
    temporality      TEXT NOT NULL,
    persistence      TEXT NOT NULL,
    confidence       TEXT NOT NULL,
    occurrences      INT  NOT NULL DEFAULT 1,
    session_count    INT  NOT NULL DEFAULT 1,
    first_seen_at    TIMESTAMPTZ NOT NULL,
    last_seen_at     TIMESTAMPTZ NOT NULL,
    first_session_id UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    first_message_id UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    first_run_id     UUID,
    last_session_id  UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    last_message_id  UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    last_run_id      UUID,
    roleplay         BOOLEAN GENERATED ALWAYS AS (evidence_type = 'roleplay_expression') STORED,
    content_hash     TEXT GENERATED ALWAYS AS (md5(engine.fact_norm(content))) STORED
);

CREATE UNIQUE INDEX uq_human_facts_dedup
    ON engine.human_facts (user_id, roleplay, content_hash);
CREATE INDEX idx_human_facts_user_seen
    ON engine.human_facts (user_id, last_seen_at DESC);
-- Keyset pagination of the export stream.
CREATE INDEX idx_human_facts_export
    ON engine.human_facts (last_seen_at, id);
-- One index per FK column, so a session or message delete does not scan the ledger.
CREATE INDEX idx_human_facts_first_session ON engine.human_facts (first_session_id);
CREATE INDEX idx_human_facts_first_message ON engine.human_facts (first_message_id);
CREATE INDEX idx_human_facts_last_session  ON engine.human_facts (last_session_id);
CREATE INDEX idx_human_facts_last_message  ON engine.human_facts (last_message_id);

-- Derived-profile rebuild bookkeeping. claimed_at marks a rebuild in flight so
-- two workers do not rebuild one user; rebuild_failures counts consecutive
-- failures.
CREATE TABLE engine.human_profile_state (
    user_id          UUID PRIMARY KEY,
    pending_facts    INT NOT NULL DEFAULT 0,
    last_fact_at     TIMESTAMPTZ,
    last_rebuilt_at  TIMESTAMPTZ,
    rebuild_failures INT NOT NULL DEFAULT 0,
    claimed_at       TIMESTAMPTZ
);

-- User-level tombstones; the deletion stream downstream mirrors read this.
CREATE TABLE engine.user_erasures (
    user_id   UUID PRIMARY KEY,
    erased_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX idx_user_erasures_erased_at
    ON engine.user_erasures (erased_at, user_id);

-- Supabase lockdown, mirroring 0025_insight_events_and_audit_cols.sql. REVOKEs
-- are wrapped in pg_roles existence checks so non-Supabase Postgres (incl. the
-- sqlx test DB, where anon/authenticated don't exist) skips them silently. The
-- RLS enable runs unconditionally; with no policy attached, only owner
-- (postgres) and service_role connections can touch the tables.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.human_facts FROM anon;
        REVOKE ALL ON engine.human_profile_state FROM anon;
        REVOKE ALL ON engine.user_erasures FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.human_facts FROM authenticated;
        REVOKE ALL ON engine.human_profile_state FROM authenticated;
        REVOKE ALL ON engine.user_erasures FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.human_facts ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.human_profile_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.user_erasures ENABLE ROW LEVEL SECURITY;

-- Nullable / defaulted adds = metadata-only change; the profile rebuild fills
-- them.
ALTER TABLE engine.human_insights
    ADD COLUMN kinks            TEXT[] NOT NULL DEFAULT '{}',
    ADD COLUMN ideology         TEXT,
    ADD COLUMN mental_health    TEXT,
    ADD COLUMN education_level  TEXT,
    ADD COLUMN education_status TEXT;

-- The profile rebuild is the third audited call kind.
ALTER TABLE engine.companion_insights_events
    DROP CONSTRAINT companion_insights_events_stage_check;
ALTER TABLE engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_stage_check
    CHECK (stage IN ('facts', 'structured', 'profile'));
