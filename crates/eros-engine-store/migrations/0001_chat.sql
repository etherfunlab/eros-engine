-- SPDX-License-Identifier: AGPL-3.0-only
CREATE TABLE chat_sessions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID NOT NULL,
    instance_id     UUID,
    lead_score      DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    is_converted    BOOLEAN NOT NULL DEFAULT false,
    last_active_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_chat_sessions_user ON chat_sessions(user_id);
CREATE INDEX idx_chat_sessions_instance ON chat_sessions(instance_id);

CREATE TABLE chat_messages (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id       UUID NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    role             TEXT NOT NULL CHECK (role IN ('user','assistant','gift_user','system_error')),
    content          TEXT NOT NULL,
    extracted_facts  JSONB,
    sent_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_chat_messages_session ON chat_messages(session_id, sent_at DESC);
