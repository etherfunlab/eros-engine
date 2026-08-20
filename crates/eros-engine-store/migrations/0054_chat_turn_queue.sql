-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Queue table for the v2 async chat turn endpoint
-- (spec: docs/superpowers/specs/2026-08-20-async-chat-endpoint-design.md).
-- One row per enqueued turn, 1:1 with the driving user message. Claim /
-- attempt / failure facts live here so engine.chat_messages keeps its
-- "a row exists => it happened" invariant. done/failed rows are retained
-- as audit; no cleanup job.

CREATE TABLE engine.chat_turn_queue (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id      UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_message_id UUID NOT NULL UNIQUE REFERENCES engine.chat_messages(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','claimed','done','failed')),
    attempts        INT  NOT NULL DEFAULT 0,
    claimed_at      TIMESTAMPTZ,
    last_error      TEXT,
    -- Per-turn request knobs (tier / scopes / prompt_traits / audit / image /
    -- tips / reply_to) serialized at enqueue time. The stream path threads
    -- these straight from the request into the generator; the async path
    -- generates later in the worker, so they must survive the gap. Processing
    -- state, not message facts — hence here and not on chat_messages.
    params          JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX chat_turn_queue_active_idx
    ON engine.chat_turn_queue (session_id, created_at DESC)
    WHERE status IN ('pending','claimed');

-- Supabase lockdown, same posture as 0013/0016: no PostgREST access,
-- RLS on with no policies (owner + service_role bypass).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.chat_turn_queue FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.chat_turn_queue FROM authenticated;
    END IF;
END $$;

ALTER TABLE engine.chat_turn_queue ENABLE ROW LEVEL SECURITY;
