-- SPDX-License-Identifier: AGPL-3.0-only
CREATE TABLE engine.companion_affinity (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id          UUID NOT NULL UNIQUE REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id             UUID NOT NULL,
    instance_id         UUID NOT NULL,
    warmth              DOUBLE PRECISION NOT NULL DEFAULT 0.3,
    trust               DOUBLE PRECISION NOT NULL DEFAULT 0.2,
    intrigue            DOUBLE PRECISION NOT NULL DEFAULT 0.5,
    intimacy            DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    patience            DOUBLE PRECISION NOT NULL DEFAULT 0.5,
    tension             DOUBLE PRECISION NOT NULL DEFAULT 0.1,
    ghost_streak        INT NOT NULL DEFAULT 0,
    last_ghost_at       TIMESTAMPTZ,
    total_ghosts        INT NOT NULL DEFAULT 0,
    relationship_label  TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_affinity_user ON engine.companion_affinity(user_id);

CREATE TABLE engine.companion_affinity_events (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    affinity_id  UUID NOT NULL REFERENCES engine.companion_affinity(id) ON DELETE CASCADE,
    event_type   TEXT NOT NULL CHECK (event_type IN ('message','ghost','gift','time_decay')),
    deltas       JSONB NOT NULL DEFAULT '{}',
    context      JSONB DEFAULT '{}',
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_affinity_events_affinity ON engine.companion_affinity_events(affinity_id);
