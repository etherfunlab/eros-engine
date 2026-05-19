-- SPDX-License-Identifier: AGPL-3.0-only
--
-- engine.chat_messages gains the metadata needed for SSE streaming + idempotent
-- replay. Spec §2.5.
--
-- Columns are nullable / default-safe so the existing sync `/message` handler
-- (which never sets them) keeps working. The replay path tolerates NULL
-- columns by synthesising sensible defaults.

ALTER TABLE engine.chat_messages
    -- Caller-supplied idempotency key, set only on role='user' rows from the
    -- streaming handler. NULL for everything else (sync path, gift_user,
    -- system_error, all assistant rows).
    ADD COLUMN client_msg_id TEXT NULL,
    -- Marker set on the user row when the AI chose Ghost for this turn,
    -- so replay can distinguish ghost-result from "still generating".
    ADD COLUMN ghost_decision BOOLEAN NOT NULL DEFAULT false,
    -- On assistant rows, the user-message id that drove this burst. NULL
    -- for sync-path rows (which the streaming path never reads). Used by
    -- the replay path to find all assistant rows for a (session, client_msg_id).
    ADD COLUMN user_message_id UUID NULL,
    -- On assistant rows, the prior assistant message this one continues.
    -- Only set when a fallback model fired after a truncated primary.
    ADD COLUMN continues_from_message_id UUID NULL,
    -- Assistant-row only: was this logical message cut off mid-stream?
    ADD COLUMN truncated BOOLEAN NOT NULL DEFAULT false,
    -- Replay metadata: model id actually served, OpenRouter usage block,
    -- OpenRouter generation id. NULL on rows the streaming path never
    -- produced (sync handler keeps writing only id/role/content).
    ADD COLUMN model TEXT NULL,
    ADD COLUMN usage JSONB NULL,
    ADD COLUMN generation_id TEXT NULL,
    -- Assistant-row only: encodes whether the row is a normal reply or a
    -- gift_reaction. NULL on user / gift_user / system_error rows and on
    -- legacy assistant rows from the sync path.
    ADD COLUMN assistant_action_type TEXT NULL
        CHECK (assistant_action_type IS NULL
               OR assistant_action_type IN ('reply', 'gift_reaction'));

-- Unique on (session_id, client_msg_id) for user rows — application enforces
-- the 24 h window by filtering on sent_at when checking for collisions. The
-- partial predicate excludes legacy NULL rows (every pre-0.2 row) so the
-- index is small.
CREATE UNIQUE INDEX chat_messages_client_msg_id_uidx
    ON engine.chat_messages (session_id, client_msg_id)
    WHERE client_msg_id IS NOT NULL;

-- Speeds replay-lookup of "all assistant rows for this user message".
CREATE INDEX chat_messages_user_message_id_idx
    ON engine.chat_messages (user_message_id)
    WHERE user_message_id IS NOT NULL;
