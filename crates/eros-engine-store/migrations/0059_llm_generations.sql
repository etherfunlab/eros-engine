-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.llm_generations — one row per billable LLM generation, from every
-- call site in the engine. The parent this schema never had: nine
-- generation_id columns across eight tables, and five tasks (world_director,
-- world_stories_director, world_comment, world_reply, memory_extraction) that
-- record nothing at all, so "what did this deployment spend, and on what"
-- could only be answered from the provider's dashboard.
--
-- Keyed on the provider's own opaque handle, stored verbatim: it IS the join
-- key to the provider's log, so a surrogate id would add a column and remove
-- nothing.
--
-- session_id is nullable because several tasks legitimately have none (the
-- standalone compose endpoint, the world/story sweepers, world_comment and
-- world_reply), and nullable ⇒ ON DELETE SET NULL: the cost record must
-- outlive the conversation it came from.
--
-- task is NOT NULL — a row that cannot say which task it belongs to answers
-- none of the questions this table exists for — but carries NO CHECK. The
-- vocabulary is [tasks.*] config, which a deployer may extend; a CHECK would
-- turn adding a config section into an insert failure on a live turn.
--
-- model and usage stay nullable: upstream occasionally omits them, and the row
-- is still a true record of a billable call.
--
-- No user_id. engine.* takes no new columns pointing at an external identity
-- system; session_id → chat_sessions.user_id answers attribution for the rows
-- that have a session, and the rest are deployment-level work with no user.
--
-- Foreign keys FROM the nine child columns are deliberately NOT here. They
-- cannot ship in the same release as the write path that populates this table:
-- fly.toml runs `migrate` before traffic swaps, so the machines still serving
-- would be writing child rows without parents. See the spec, §8.
--
-- Spec: docs/superpowers/specs/2026-08-24-llm-generations-audit-design.md

CREATE TABLE engine.llm_generations (
    generation_id TEXT PRIMARY KEY,
    session_id    UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    task          TEXT NOT NULL,
    model         TEXT,
    usage         JSONB,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Without this, every chat_sessions delete seq-scans this table for the
-- ON DELETE SET NULL above.
CREATE INDEX idx_llm_generations_session
    ON engine.llm_generations (session_id);
-- "spend per task over a window" — the query this table exists for.
CREATE INDEX idx_llm_generations_task_created
    ON engine.llm_generations (task, created_at DESC);

-- Supabase lockdown, mirroring 0045. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.llm_generations FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.llm_generations FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.llm_generations ENABLE ROW LEVEL SECURITY;
