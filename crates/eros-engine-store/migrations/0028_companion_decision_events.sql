-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.companion_decision_events — append-only, best-effort telemetry: ≈ one
-- row per PDE judge run (action + cost audit). Modelled on
-- companion_insights_events (migration 0025). NOT a guaranteed ledger — the
-- write is fire-and-forget, so a row may be dropped under shutdown/backpressure.
--
-- Spec: docs/superpowers/specs/2026-06-04-llm-based-pde-design.md

CREATE TABLE engine.companion_decision_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id          UUID NOT NULL,
    user_id         UUID NOT NULL,
    session_id      UUID,
    message_id      UUID,
    status          TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error','timeout','error')),
    action          TEXT,
    proposed_action TEXT,
    payload         JSONB,
    model           TEXT,
    usage           JSONB,
    generation_id   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_companion_decision_events_user_time
    ON engine.companion_decision_events (user_id, created_at DESC);
CREATE INDEX idx_companion_decision_events_run
    ON engine.companion_decision_events (run_id);

-- Supabase lockdown, mirroring 0025. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.companion_decision_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.companion_decision_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.companion_decision_events ENABLE ROW LEVEL SECURITY;
