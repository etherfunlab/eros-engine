-- SPDX-License-Identifier: AGPL-3.0-only
-- companion_insights kept STANDALONE (not on user_profiles) so OSS doesn't need
-- to share user_profiles ownership with a host application.
CREATE TABLE engine.companion_insights (
    user_id          UUID PRIMARY KEY,
    insights         JSONB NOT NULL DEFAULT '{}',
    training_level   DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
