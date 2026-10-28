-- SPDX-License-Identifier: AGPL-3.0-only
-- Memory 2.0 (dreaming-plus), spec 2026-10-28: the per-relationship scanner's
-- tables.
--
--   * engine.relationship_scan_state  — per-instance cursor, counters, claim
--   * engine.relationship_episodes    — one summary per scanned stretch
--   * engine.relationship_arcs        — the rolling relationship arc
--   * engine.relationship_journal     — dated events of the relationship
--   * engine.relationship_scan_events — counts-only audit of scan runs
--
-- Spec: docs/superpowers/specs/2026-10-28-memory-2-dreaming-plus-design.md
--
-- No table carries user_id: the instance determines the owner.

-- scanned_through_at is an exclusive cursor on chat_messages.sent_at;
-- pending_turns counts user rows after it. claimed_at marks a scan in flight
-- so two workers do not scan one relationship.
CREATE TABLE engine.relationship_scan_state (
    instance_id           UUID PRIMARY KEY REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    scanned_through_at    TIMESTAMPTZ NOT NULL,
    pending_turns         INT NOT NULL DEFAULT 0,
    last_turn_at          TIMESTAMPTZ,
    last_scanned_at       TIMESTAMPTZ,
    scan_failures         INT NOT NULL DEFAULT 0,
    claimed_at            TIMESTAMPTZ,
    user_timezone         TEXT,
    episodes_since_arc    INT NOT NULL DEFAULT 0,
    arc_failures          INT NOT NULL DEFAULT 0,
    backfilled_through_at TIMESTAMPTZ
);

-- started_at / ended_at come from the rows the engine read. generation_id is
-- a correlation id, not a reference.
CREATE TABLE engine.relationship_episodes (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id      UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    summary          TEXT NOT NULL,
    started_at       TIMESTAMPTZ NOT NULL,
    ended_at         TIMESTAMPTZ NOT NULL,
    first_message_id UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    last_message_id  UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    folded           BOOLEAN NOT NULL DEFAULT false,
    generation_id    TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_relationship_episodes_instance_ended
    ON engine.relationship_episodes (instance_id, ended_at DESC);
-- One index per FK column, so a message delete does not scan the episodes.
CREATE INDEX idx_relationship_episodes_first_message
    ON engine.relationship_episodes (first_message_id);
CREATE INDEX idx_relationship_episodes_last_message
    ON engine.relationship_episodes (last_message_id);

CREATE TABLE engine.relationship_arcs (
    instance_id    UUID PRIMARY KEY REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    opening        TEXT NOT NULL,
    arc            TEXT NOT NULL,
    previous_arc   TEXT,
    folded_through TIMESTAMPTZ NOT NULL,
    generation_id  TEXT,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- embedding stays NULL until embedded; searches add `embedding IS NOT NULL`.
-- dedup_key NULL marks a repeatable kind.
CREATE TABLE engine.relationship_journal (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id       UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    occurred_on       DATE NOT NULL,
    kind              TEXT NOT NULL,
    realm             TEXT NOT NULL CHECK (realm IN ('real', 'roleplay')),
    summary           TEXT NOT NULL,
    source            TEXT NOT NULL CHECK (source IN ('llm', 'engine')),
    message_id        UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    affinity_event_id UUID REFERENCES engine.companion_affinity_events(id) ON DELETE SET NULL,
    dedup_key         TEXT,
    due_on            DATE,
    recurrence        TEXT NOT NULL CHECK (recurrence IN ('none', 'monthly_then_yearly', 'on_due')),
    embedding         vector(512),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_relationship_journal_instance_occurred
    ON engine.relationship_journal (instance_id, occurred_on);
CREATE INDEX idx_relationship_journal_message
    ON engine.relationship_journal (message_id);
CREATE INDEX idx_relationship_journal_affinity_event
    ON engine.relationship_journal (affinity_event_id);
-- The sweeper tick embeds every row still waiting for a vector.
CREATE INDEX idx_relationship_journal_unembedded
    ON engine.relationship_journal (created_at) WHERE embedding IS NULL;
CREATE UNIQUE INDEX uq_relationship_journal_dedup
    ON engine.relationship_journal (instance_id, dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX idx_relationship_journal_embedding
    ON engine.relationship_journal USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

-- Counts and ids only, no free text. An audit row outlives its instance
-- (SET NULL), so an operator can still read the silence of a deleted one.
CREATE TABLE engine.relationship_scan_events (
    id               BIGSERIAL PRIMARY KEY,
    instance_id      UUID REFERENCES engine.persona_instances(id) ON DELETE SET NULL,
    kind             TEXT NOT NULL CHECK (kind IN ('scan', 'arc', 'backfill')),
    status           TEXT NOT NULL CHECK (status IN ('ok', 'empty', 'parse_error', 'rejected', 'failed')),
    model            TEXT,
    served_fallback  BOOLEAN NOT NULL DEFAULT false,
    rows_covered     INT NOT NULL DEFAULT 0,
    memories_written INT NOT NULL DEFAULT 0,
    episode_written  BOOLEAN NOT NULL DEFAULT false,
    journal_written  INT NOT NULL DEFAULT 0,
    generation_id    TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_relationship_scan_events_instance_created
    ON engine.relationship_scan_events (instance_id, created_at DESC);

-- Supabase lockdown, mirroring 0079_human_facts.sql. REVOKEs are wrapped in
-- pg_roles existence checks so non-Supabase Postgres (incl. the sqlx test DB,
-- where anon/authenticated don't exist) skips them silently. The RLS enable
-- runs unconditionally; with no policy attached, only owner (postgres) and
-- service_role connections can touch the tables.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.relationship_scan_state FROM anon;
        REVOKE ALL ON engine.relationship_episodes FROM anon;
        REVOKE ALL ON engine.relationship_arcs FROM anon;
        REVOKE ALL ON engine.relationship_journal FROM anon;
        REVOKE ALL ON engine.relationship_scan_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.relationship_scan_state FROM authenticated;
        REVOKE ALL ON engine.relationship_episodes FROM authenticated;
        REVOKE ALL ON engine.relationship_arcs FROM authenticated;
        REVOKE ALL ON engine.relationship_journal FROM authenticated;
        REVOKE ALL ON engine.relationship_scan_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.relationship_scan_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.relationship_episodes ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.relationship_arcs ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.relationship_journal ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.relationship_scan_events ENABLE ROW LEVEL SECURITY;
