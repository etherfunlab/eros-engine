-- Index an instance's `period` and `period_scare` rows, newest first, so the
-- read of the newest of each (the cycle's anchor and its scare, spec
-- 2026-11-24-period-cycle-design.md §5, §11) finds them by index however old
-- they are, instead of walking the instance's whole event history.

CREATE INDEX IF NOT EXISTS persona_events_cycle_idx
    ON engine.persona_events (instance_id, kind, started_at DESC)
    WHERE kind IN ('period', 'period_scare');
