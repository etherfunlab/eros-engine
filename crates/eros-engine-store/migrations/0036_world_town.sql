-- SPDX-License-Identifier: AGPL-3.0-only
--
-- World Town (spec: docs/superpowers/specs/2026-07-21-world-town-design.md).
--
-- engine.world_posts — director-pre-generated persona posts, published by
-- the town sweeper when scheduled_at passes (status = published_at NULL/not).
-- last_reply_at doubles as the reply-responder cooldown stamp + claim.
--
-- engine.world_post_comments — thread rows. author_instance_id NULL = the
-- user themselves; source tells which pipeline wrote a persona comment
-- ('round' = hourly batch, 'reply' = reply responder) and is NULL exactly
-- for user rows (enforced by CHECK).

CREATE TABLE engine.world_posts (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid     UUID NOT NULL,
    instance_id   UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    content       TEXT NOT NULL,
    scheduled_at  TIMESTAMPTZ NOT NULL,
    published_at  TIMESTAMPTZ,
    last_reply_at TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_world_posts_due ON engine.world_posts (owner_uid, scheduled_at)
    WHERE published_at IS NULL;
CREATE INDEX idx_world_posts_feed ON engine.world_posts (owner_uid, published_at DESC);

CREATE TABLE engine.world_post_comments (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    post_id            UUID NOT NULL REFERENCES engine.world_posts(id) ON DELETE CASCADE,
    author_instance_id UUID REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    source             TEXT CHECK (source IN ('round', 'reply')),
    content            TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((author_instance_id IS NULL) = (source IS NULL))
);
CREATE INDEX idx_world_post_comments_thread ON engine.world_post_comments (post_id, created_at);

ALTER TABLE engine.world_enrollments ADD COLUMN town_enabled BOOL NOT NULL DEFAULT false;
ALTER TABLE engine.world_states     ADD COLUMN last_comment_round_at TIMESTAMPTZ;

-- 0013-style lockdown: REVOKE from Supabase browser roles (when present) and
-- enable policy-less RLS so only owner/service_role connections reach rows.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.world_posts         FROM anon;
        REVOKE ALL ON engine.world_post_comments FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.world_posts         FROM authenticated;
        REVOKE ALL ON engine.world_post_comments FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.world_posts         ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.world_post_comments ENABLE ROW LEVEL SECURITY;
