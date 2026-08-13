-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.chat_images_events — append-only, best-effort telemetry: one row per
-- image-composer call, from any caller (chat turn or the standalone compose
-- endpoint). Records the five composer input slots, the composed wire prompt,
-- and the chain-walk facts. NOT a guaranteed ledger — the write is fail-open,
-- so a row may be dropped without costing the turn.
--
-- Deliberately NO message_id column: the composer runs before the assistant row
-- exists, so linkage runs the other way — the assistant row carries
-- metadata.image.compose_event_id pointing at this table's `id`. That keeps the
-- composer auditable from callers that have no message at all.
--
-- Spec: docs/superpowers/specs/2026-08-14-image-audit-events-design.md

CREATE TABLE engine.chat_images_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source          TEXT NOT NULL CHECK (source IN (
                        'chat_reply_text_image','chat_reply_image',
                        'compose_endpoint','compose_endpoint_stream')),
    user_id         UUID NOT NULL,
    instance_id     UUID,
    session_id      UUID,
    status          TEXT NOT NULL CHECK (status IN ('ok','exhausted','not_configured')),
    inputs          JSONB NOT NULL,
    subject         TEXT,
    caption         TEXT,
    composed_prompt TEXT,
    variant         TEXT,
    model           TEXT,
    usage           JSONB,
    generation_id   TEXT,
    attempts        SMALLINT NOT NULL,
    last_failure    TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_chat_images_events_user_time
    ON engine.chat_images_events (user_id, created_at DESC);
CREATE INDEX idx_chat_images_events_session_time
    ON engine.chat_images_events (session_id, created_at DESC);

-- Supabase lockdown, mirroring 0028. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.chat_images_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.chat_images_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.chat_images_events ENABLE ROW LEVEL SECURITY;
