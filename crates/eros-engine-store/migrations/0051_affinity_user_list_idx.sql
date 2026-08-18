-- 0051: index for the user-scoped affinity list (issue #274).
--
-- `GET /bff/v1/comp/affinities/{user_id}` pages a user's rows with a keyset
-- on `(updated_at, session_id)`. `idx_affinity_user` (migration 0002) covers
-- the `user_id` filter but leaves the sort to a per-user sort step, and the
-- keyset predicate cannot seek — with no cap on how many companions a user
-- may talk to, that is a scan whose cost is set by the heaviest user.
--
-- `session_id` is the tiebreak, not decoration: `updated_at` is a timestamp
-- two rows can share, and a cursor that cannot break the tie either skips a
-- row or serves it twice.
CREATE INDEX idx_affinity_user_updated
    ON engine.companion_affinity (user_id, updated_at DESC, session_id DESC);
