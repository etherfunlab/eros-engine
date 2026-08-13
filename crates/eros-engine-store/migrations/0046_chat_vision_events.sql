-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.chat_vision_events — append-only, best-effort telemetry: one row per
-- image-carrying, non-tipped chat turn THAT REACHES THE TEXT-REPLY PATH,
-- recording the `chat_vision` describe call. A turn that never reaches that
-- path — ghosted, routed to product_qa, or answered with an image-only reply
-- — writes no row: the describe never runs on those paths either. Written
-- even when the describe never ran ('not_configured') or failed on every
-- chain model ('exhausted') — those two cases leave no trace anywhere else,
-- which is the gap this table closes.
--
-- Keeps message_id (unlike chat_images_events): vision runs AFTER the
-- role='user' row exists, so the id is already in hand, and there is exactly
-- one call site.
--
-- Spec: docs/superpowers/specs/2026-08-14-image-audit-events-design.md

CREATE TABLE engine.chat_vision_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL,
    session_id    UUID NOT NULL,
    message_id    UUID NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('ok','exhausted','not_configured')),
    image_url     TEXT NOT NULL,
    vision        JSONB,
    model         TEXT,
    usage         JSONB,
    generation_id TEXT,
    attempts      SMALLINT NOT NULL,
    last_failure  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_chat_vision_events_user_time
    ON engine.chat_vision_events (user_id, created_at DESC);
CREATE INDEX idx_chat_vision_events_message
    ON engine.chat_vision_events (message_id);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.chat_vision_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.chat_vision_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.chat_vision_events ENABLE ROW LEVEL SECURITY;
