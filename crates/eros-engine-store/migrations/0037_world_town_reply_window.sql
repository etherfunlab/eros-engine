-- SPDX-License-Identifier: AGPL-3.0-only
--
-- World Town reply-scan activity window (spec:
-- docs/superpowers/specs/2026-07-21-world-town-reply-window-design.md).
--
-- last_user_comment_at is a denormalized "latest user-comment time" on each
-- post, maintained by the user-comment insert (see WorldTownRepo::
-- insert_user_comment). It bounds the reply-responder scan — driven by
-- idx_world_posts_reply — to posts with a user comment inside the configured
-- window, so scan cost no longer grows with total published-post count.
-- Persona comments (source 'round'/'reply') never touch it. Rows are kept
-- forever; this is a scan bound, not retention.

ALTER TABLE engine.world_posts ADD COLUMN last_user_comment_at TIMESTAMPTZ;

-- Only posts that ever received a user comment enter the index; the vast
-- majority (no user comment) stay NULL and out of it, so the index is
-- inherently small and is the sole driving access path for the reply scan.
CREATE INDEX idx_world_posts_reply ON engine.world_posts (last_user_comment_at)
    WHERE last_user_comment_at IS NOT NULL;

-- One-time backfill for posts that already have user comments.
UPDATE engine.world_posts p SET last_user_comment_at = sub.max_created
FROM (
    SELECT post_id, max(created_at) AS max_created
    FROM engine.world_post_comments
    WHERE author_instance_id IS NULL
    GROUP BY post_id
) sub
WHERE sub.post_id = p.id;
