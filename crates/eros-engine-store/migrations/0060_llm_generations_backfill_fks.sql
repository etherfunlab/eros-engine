-- SPDX-License-Identifier: AGPL-3.0-only
-- Release B1 of three. 0059 created engine.llm_generations and Release A
-- (1.6.1) made every call site write to it. This migration gives the table its
-- history and its constraints: one parent row per generation_id already sitting
-- in a child table, an index on each of the eight child columns, and eight
-- validated foreign keys.
--
-- Spec: docs/superpowers/specs/2026-08-24-llm-generations-audit-design.md §7
--
--
-- WHY THE DROP IS NOT HERE.
--
-- The obvious companion to this migration — dropping model and usage from the
-- eight child tables, now that the parent holds them — cannot ship in the same
-- release. Ten INSERT statements in this crate name both columns explicitly
-- (chat.rs three times; affinity, insight, decision, character_insight,
-- user_insight once each; image_events twice). fly.toml runs `migrate` as
-- release_command, which completes BEFORE traffic moves, so for the length of
-- every rollout the PREVIOUS build is serving against the NEW schema. Drop the
-- columns here and every machine still taking traffic issues
-- INSERT ... (model, usage, ...) against columns that no longer exist — which
-- for chat_messages is a user's reply failing to persist. 0061 drops them once
-- B1 is deployed and nothing writes them any more.
--
-- This is 0059's own §8 note read in the other direction. A change that ADDS a
-- requirement ships after the code that satisfies it; a change that REMOVES
-- something ships after the code that stopped using it. Both halves are hard.
--
--
-- WHY THIS FILE SETS lock_timeout, AND NO EARLIER ONE DOES.
--
-- Seventeen lock-taking statements against tables a live turn writes on every
-- message. The reasoning that makes a timeout right here is specific: a failed
-- release_command is the SAFE failure. Fly runs it before traffic moves, so an
-- abort leaves the old machines serving on the old schema with nothing to roll
-- back. A migration that instead waits behind someone's long-running
-- transaction holds chat_messages write-blocked for as long as that wait
-- lasts, and users see that. Failing fast and re-running the deploy is
-- strictly better than succeeding slowly.
--
--
-- STATEMENT ORDER. sqlx wraps this whole file in ONE transaction, so every
-- lock is held until the last statement commits — ordering changes when a lock
-- is ACQUIRED, never when it is released. CREATE INDEX CONCURRENTLY cannot run
-- inside a transaction and is not available. So, carrying 0058's discipline:
--
--   1. Backfill first. It only takes AccessShare on the child tables, which
--      blocks nothing, and everything downstream depends on its output.
--   2. Orphan preflight second — earlier than the spec's "immediately before
--      the first ADD CONSTRAINT", because it needs nothing but AccessShare and
--      failing before ANY index has taken its ShareLock is strictly better.
--   3. Indexes, coldest table first, chat_messages last.
--   4. Foreign keys, same order.
--
-- Row counts as of 2026-08-25: chat_messages 22,573, companion_insights_events
-- 18,918, companion_decision_events 10,533, companion_affinity_events 9,941,
-- character_insights_events 7,316, chat_images_events 1,297, chat_vision_events
-- 14, user_insights_events 0.
--
--
-- WHY VALIDATED, REVERSING 0058'S BLANKET NOT VALID RULE.
--
-- 0058 constrained columns whose parents (chat_sessions, chat_messages,
-- persona_instances) are deleted by forces outside the migration; one user
-- deleted between the orphan check and the deploy is enough to fail the scan.
-- Here the parent rows are produced by this same transaction, out of the very
-- child tables being constrained. ADD FOREIGN KEY takes SHARE ROW EXCLUSIVE on
-- both sides, so nothing writes a new child row between the backfill and the
-- scan, and the deploy order above means the code running during this migration
-- is Release A or later — all of which write the parent before handing the id
-- to a child.
--
-- That argument covers concurrency, not history. It says nothing about an
-- orphan already sitting in a table, from a build older than Release A or from
-- any path that ever stored a raw resp.generation_id instead of
-- record_generation's return value. Production had zero such rows across all
-- eight columns on 2026-08-25 — an observation with a timestamp on it, not a
-- proof. The preflight below asserts it instead of assuming it.
--
--
-- BACKFILL VOLUME, measured 2026-08-25 with 1.6.2 serving traffic: 59,112
-- child references, ALL DISTINCT, of which 56,636 need a parent. The 2,476
-- difference is what 1.6.1 and 1.6.2 have already written, and it is what
-- ON CONFLICT DO NOTHING absorbs. No generation_id appears in two source
-- tables, so the conflict tiebreak never actually fires here — it is a guard,
-- not a policy anyone has to reason about.
--
-- Every DISTINCT ON carries an explicit ORDER BY anyway. Without one Postgres
-- may return any row of a duplicate group, and a migration that produces
-- different task or session_id values on two runs is untestable. That no
-- duplicates exist is what makes the ORDER BY free — the reason to write it,
-- not a reason to omit it.
--
--
-- companion_affinity_events HAS NO session_id COLUMN. It carries affinity_id
-- and a nullable user_message_id, and nothing else that reaches a session, so
-- its only route is user_message_id -> chat_messages.session_id. Coverage
-- splits by era: 146 of 8,054 rows (1.8%) before 1.6.1 was deployed, 65 of 65
-- (100%) since. The historical NULLs are a backlog that stopped growing, not a
-- property of the table. They are also not a defect to fix here: session_id is
-- nullable precisely because some generations have no conversation to attribute
-- to, and inventing one — by joining on timestamps, or on the affinity row's
-- own owner — would put a guess in the one column whose whole value is that it
-- is not one.
--
--
-- EVERY session_id GOES THROUGH LEFT JOIN engine.chat_sessions. 0059 gave
-- llm_generations.session_id a VALIDATED foreign key, and 0058 deliberately
-- left dangling session ids in place downstream — 26 distinct ones in
-- companion_insights_events, 24 in companion_decision_events, 2 in
-- chat_images_events, measured 2026-08-25. Copying one straight across would
-- violate this table's own constraint and abort the migration, taking the
-- engine's boot with it. The LEFT JOIN is load-bearing, not defensive.

SET LOCAL lock_timeout = '3s';

-- ─── 1. Backfill: one parent row per existing child generation_id ───
--
-- Timestamps come from sent_at (chat_messages) or created_at (all others), so
-- backfilled rows keep their real position in time. 0059 declares created_at
-- as a plain DEFAULT now(), so an explicit value is accepted; stamping 56,636
-- historical generations with deploy-day would make the column useless for
-- exactly the reconciliation this table exists for.

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (m.generation_id)
       m.generation_id,
       s.id,
       CASE m.channel
           WHEN 'voice'      THEN 'chat_voice'
           WHEN 'product_qa' THEN 'chat_product_qa'
           ELSE 'chat_companion'
       END,
       m.model, m.usage, m.sent_at
  FROM engine.chat_messages m
  LEFT JOIN engine.chat_sessions s ON s.id = m.session_id
 WHERE m.generation_id IS NOT NULL
 ORDER BY m.generation_id, m.sent_at
ON CONFLICT (generation_id) DO NOTHING;

-- The output filter's own generation. filter_model is its model — but ONLY on
-- the rows that actually made a call: the regex arm writes the sentinel
-- '<regex>' with no f_generation_id at all, and those rows are not selected
-- here because they have no generation to record. That is also why 0061 does
-- NOT drop filter_model: 2,024 rows hold '<regex>' against 1 holding a real
-- slug, and the sentinel has no parent row it could ever be read from.
INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (m.f_generation_id)
       m.f_generation_id, s.id, 'chat_output_filter', m.filter_model, NULL, m.sent_at
  FROM engine.chat_messages m
  LEFT JOIN engine.chat_sessions s ON s.id = m.session_id
 WHERE m.f_generation_id IS NOT NULL
 ORDER BY m.f_generation_id, m.sent_at
ON CONFLICT (generation_id) DO NOTHING;

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id, 'affinity_evaluation', e.model, e.usage, e.created_at
  FROM engine.companion_affinity_events e
  LEFT JOIN engine.chat_messages m ON m.id = e.user_message_id
  LEFT JOIN engine.chat_sessions s ON s.id = m.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id,
       CASE e.stage WHEN 'facts' THEN 'insight_extraction'
                    ELSE 'insight_structuring' END,
       e.model, e.usage, e.created_at
  FROM engine.companion_insights_events e
  LEFT JOIN engine.chat_sessions s ON s.id = e.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id, 'pde_decision', e.model, e.usage, e.created_at
  FROM engine.companion_decision_events e
  LEFT JOIN engine.chat_sessions s ON s.id = e.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

-- source = 'image_edit' has zero rows carrying a generation_id today. The arm
-- stays: a CASE branch that matches nothing costs nothing, and removing it
-- would silently mislabel the first image-edit compose that ever lands here.
INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id,
       CASE e.source WHEN 'image_edit' THEN 'chat_image_edit_compose'
                     ELSE 'chat_image_prompt_compose' END,
       e.model, e.usage, e.created_at
  FROM engine.chat_images_events e
  LEFT JOIN engine.chat_sessions s ON s.id = e.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id, 'chat_vision', e.model, e.usage, e.created_at
  FROM engine.chat_vision_events e
  LEFT JOIN engine.chat_sessions s ON s.id = e.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id,
       CASE e.stage WHEN 'extraction' THEN 'character_insight_extraction'
                    ELSE 'character_insight_structuring' END,
       e.model, e.usage, e.created_at
  FROM engine.character_insights_events e
  LEFT JOIN engine.chat_sessions s ON s.id = e.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

INSERT INTO engine.llm_generations (generation_id, session_id, task, model, usage, created_at)
SELECT DISTINCT ON (e.generation_id)
       e.generation_id, s.id,
       CASE e.stage WHEN 'extraction' THEN 'user_insight_extraction'
                    ELSE 'user_insight_structuring' END,
       e.model, e.usage, e.created_at
  FROM engine.user_insights_events e
  LEFT JOIN engine.chat_sessions s ON s.id = e.session_id
 WHERE e.generation_id IS NOT NULL
 ORDER BY e.generation_id, e.created_at
ON CONFLICT (generation_id) DO NOTHING;

-- ─── 2. Orphan preflight ────────────────────────────────────────────
--
-- This fails the same deploy ADD CONSTRAINT would have failed, but it names
-- the table and the count — which turns a generic constraint violation into a
-- five-minute diagnosis — and it fails before any statement below has taken a
-- lock that blocks a live write. 57,000 rows scan in milliseconds.

DO $$
DECLARE
  t text;
  n bigint;
BEGIN
  FOREACH t IN ARRAY ARRAY[
      'chat_messages', 'companion_affinity_events', 'companion_insights_events',
      'companion_decision_events', 'chat_images_events', 'chat_vision_events',
      'character_insights_events', 'user_insights_events'
  ] LOOP
    EXECUTE format(
      'SELECT count(*) FROM engine.%I c
        WHERE c.generation_id IS NOT NULL
          AND NOT EXISTS (SELECT 1 FROM engine.llm_generations g
                           WHERE g.generation_id = c.generation_id)', t)
    INTO n;
    IF n > 0 THEN
      RAISE EXCEPTION 'orphaned generation_id rows in engine.%: % (backfill missed a discriminator value)', t, n;
    END IF;
  END LOOP;
END $$;

-- ─── 3. Indexes, coldest table first ────────────────────────────────
--
-- None of these eight columns has an index today, and an unindexed child
-- column makes the parent's ON DELETE SET NULL seq-scan the whole table.

CREATE INDEX idx_user_insights_events_generation
    ON engine.user_insights_events (generation_id);
CREATE INDEX idx_chat_vision_events_generation
    ON engine.chat_vision_events (generation_id);
CREATE INDEX idx_chat_images_events_generation
    ON engine.chat_images_events (generation_id);
CREATE INDEX idx_character_insights_events_generation
    ON engine.character_insights_events (generation_id);
CREATE INDEX idx_companion_affinity_events_generation
    ON engine.companion_affinity_events (generation_id);
CREATE INDEX idx_companion_decision_events_generation
    ON engine.companion_decision_events (generation_id);
CREATE INDEX idx_companion_insights_events_generation
    ON engine.companion_insights_events (generation_id);
CREATE INDEX idx_chat_messages_generation
    ON engine.chat_messages (generation_id);

-- ─── 4. The constraints, same order ─────────────────────────────────
--
-- ON DELETE SET NULL throughout: the reference column is nullable, and an
-- audit trail outlives its parent. A generation's cost stays reconcilable
-- against the provider's own log after the parent row is gone.

ALTER TABLE engine.user_insights_events
    ADD CONSTRAINT user_insights_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

ALTER TABLE engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

ALTER TABLE engine.chat_images_events
    ADD CONSTRAINT chat_images_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

ALTER TABLE engine.character_insights_events
    ADD CONSTRAINT character_insights_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

ALTER TABLE engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

ALTER TABLE engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;

-- chat_messages last. It is the hottest table here and the only one where a
-- lock held too long is visible to a user mid-turn.
--
-- NOTE the asymmetry: f_generation_id gets NO constraint. It is written by a
-- separate UPDATE path (mark_filtered) that would need its own degrade branch,
-- and production holds exactly one non-null value in it. The constraint would
-- buy nothing and add a second failure mode to the filter path. This is a
-- decision, not an oversight — see spec §7.2 before "fixing" it.

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_generation_id_fkey
    FOREIGN KEY (generation_id) REFERENCES engine.llm_generations(generation_id)
    ON DELETE SET NULL;
