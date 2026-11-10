-- Row-level deletions (spec 2026-11-10-row-deletion-and-deletion-stream-design.md).
--
-- engine.row_deletions: one tombstone per profile memory or ledger fact the
-- owner deleted, read by mirrors through GET /svc/v1/rows/deletions. `id` is
-- the id the deleted row had; the row is gone, so there is nothing to
-- reference. No user_id: the row holds ids only, and user erasure has nothing
-- of the user's to remove here.
--
-- human_profile_state.rebuild_due: a delete marks the user's profile due, and
-- the sweeper takes due users on its next tick instead of after the idle
-- window.

CREATE TABLE engine.row_deletions (
    kind       TEXT        NOT NULL CHECK (kind IN ('memory', 'fact')),
    id         UUID        NOT NULL,
    deleted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, id)
);

CREATE INDEX idx_row_deletions_export ON engine.row_deletions (deleted_at, id);

ALTER TABLE engine.human_profile_state
    ADD COLUMN rebuild_due BOOLEAN NOT NULL DEFAULT false;

-- Supabase lockdown, as in 0079: REVOKEs only where the roles exist; RLS with
-- no policy leaves the table to the owner and service_role.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.row_deletions FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.row_deletions FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.row_deletions ENABLE ROW LEVEL SECURITY;
