-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Foreign keys for every reference column in engine.* that still had none.
--
-- These grew by drift, not by decision. 0040's own comment records it: the
-- original instance-scoped tables "reference engine.persona_instances(id) by
-- convention only. They predate the world/story tables (0035-0039), which all
-- declare a real foreign key" — and the tables written in between copied the
-- earlier style. 0047 then shipped character_insights_events with the cost
-- stated out loud and still no FK ("rows whose instance has since been deleted
-- are no longer attributable, because the join has nothing to hit"). The bill
-- lands on whoever queries the database: an unlinked UUID is unnavigable in the
-- Supabase table editor, so working out which turn a row belongs to means
-- guessing by timestamp — which is what 0056 said it was replacing.
--
-- This closes the whole set except the attribution columns (below). user_id has
-- a second reason to stay unconstrained: it would be this schema's only
-- reference into auth.users.
--
-- ON DELETE, per column:
--
--   SET NULL where the pointer is one of several and the row stays meaningful
--   without it. An audit trail outlives what it points at: the cost and
--   generation_id a row records stays reconcilable against the provider's own
--   log after the conversation is gone.
--
-- No FK at all on an ATTRIBUTION column — the one column a row has that leads
-- back to a person. CASCADE destroys the evidence; SET NULL leaves a row nobody
-- can ever find again; RESTRICT / NO ACTION does preserve both, but at the cost
-- of letting an append-only audit table veto the deletion of a core row, which
-- audit must never do. All three are worse than no constraint. That is why no
-- user_id / owner_uid here carries one, and why instance_id on
-- character_insights_events, user_insights_events and their two snapshot tables
-- does not either: those four tables have no user_id and no owner_uid, so
-- instance_id is the only attribution they have, and
-- `deleting_the_instance_cascades_the_profile_but_keeps_the_audit` (0047) and
-- its user-chain twin (0055) already pin that the trail survives the instance.
-- 0047's own comment names the real fix: give those tables an owner_uid. That
-- is a column addition with a backfill and a write-path change, so it is its
-- own migration, not a rider on this one.
--
-- WHY THE FOUR user_id COLUMNS STAY, given that storing one fact twice is
-- exactly what this codebase does not do. They are not a second copy of
-- chat_sessions.user_id, because on three of the four tables session_id is
-- NULLABLE by design and user_id is the row's only attribution when it is null:
--
--   chat_images_events        — the standalone compose endpoint has no session
--   companion_insights_events — a run may be recorded without one
--   companion_decision_events — same
--
-- chat_vision_events is the one where session_id used to be NOT NULL, so user_id
-- did duplicate a derivable fact. It stops duplicating as of this migration:
-- SET NULL means a deleted session blanks session_id and user_id becomes the
-- only thing left pointing at a person. Dropping the column would trade a
-- reconcilable cost record for an anonymous one.
--
-- Each is also the leading column of that table's (user_id, created_at DESC)
-- index — the "this user's audit trail" access path, which a join through
-- chat_sessions cannot serve once session_id is null anyway.
--
-- chat_vision_events.session_id and .message_id drop NOT NULL so SET NULL is
-- expressible. ChatVisionEventRepo::record supplies both on every insert, but
-- note the database no longer enforces that: a hand-written INSERT may now
-- leave either pointer NULL where before it could not. The application is the
-- only thing keeping them populated at write time.
--
-- EVERY CONSTRAINT IS NOT VALID, including the columns production came back
-- clean on. A validated ADD CONSTRAINT scans the table as it is at deploy time,
-- and a failed scan rolls back the migration — which here means the engine does
-- not boot, with no rollback path. Between the orphan check that informed this
-- file and the moment it actually runs, one row pointing at a since-deleted
-- parent is enough to trigger that. NOT VALID removes the entire class: no scan,
-- so nothing to fail on, and enforcement on every future INSERT and on any
-- UPDATE of the key is identical either way. What is given up is only the
-- retrospective guarantee about rows that already exist.
--
-- Four columns genuinely do carry dangling ids today — chat_images_events
-- .session_id, and both pointers on companion_insights_events and
-- companion_decision_events. They are deliberately NOT backfilled to NULL,
-- because two audit rows sharing one dead message_id are still recognizably the
-- same turn, and that correlation is the only thing those ids have left. A
-- later migration may clean up and VALIDATE; nothing here depends on it.
--
-- STATEMENT ORDER. sqlx runs this whole file in ONE transaction, so every lock
-- any statement takes is held until the last one commits — ordering changes
-- when a lock is ACQUIRED, never when it is released. CREATE INDEX takes SHARE,
-- which already blocks writes to the table it indexes; ADD FOREIGN KEY takes
-- SHARE ROW EXCLUSIVE on the child AND on the parent. So the goal is narrow but
-- real: every index build on an audit table finishes before the first statement
-- that touches a hot table, and the statements touching chat_messages —
-- its own index and its two self-referential FKs — sit as late as their
-- dependencies allow. chat_messages still becomes write-blocked at the first FK
-- naming it as parent, which is unavoidable in a single transaction.
-- CREATE INDEX CONCURRENTLY is not an option: it cannot run inside one.

-- ─── Audit-table indexes first: no hot table is touched yet ────────
--
-- An unindexed FK makes the parent's ON DELETE action seq-scan the child.
-- Columns already covered as the leading column of an existing composite index
-- are skipped: every user_id, chat_images_events.session_id,
-- chat_vision_events.message_id, and the four insights tables' instance_id.
-- chat_messages.user_message_id already has its own partial index.

CREATE INDEX idx_chat_images_events_instance
    ON engine.chat_images_events (instance_id);
CREATE INDEX idx_chat_vision_events_session
    ON engine.chat_vision_events (session_id);
CREATE INDEX idx_companion_insights_events_session
    ON engine.companion_insights_events (session_id);
CREATE INDEX idx_companion_insights_events_message
    ON engine.companion_insights_events (message_id);
CREATE INDEX idx_companion_decision_events_session
    ON engine.companion_decision_events (session_id);
CREATE INDEX idx_companion_decision_events_message
    ON engine.companion_decision_events (message_id);
CREATE INDEX idx_character_insights_events_session
    ON engine.character_insights_events (session_id);
CREATE INDEX idx_character_insights_events_message
    ON engine.character_insights_events (message_id);
CREATE INDEX idx_user_insights_events_session
    ON engine.user_insights_events (session_id);
CREATE INDEX idx_user_insights_events_message
    ON engine.user_insights_events (message_id);

-- ─── Nullability, so SET NULL is expressible ────────────────────────

ALTER TABLE engine.chat_vision_events
    ALTER COLUMN session_id DROP NOT NULL,
    ALTER COLUMN message_id DROP NOT NULL;

-- ─── The constraints ───────────────────────────────────────────────

ALTER TABLE engine.chat_images_events
    ADD CONSTRAINT chat_images_events_instance_id_fkey
    FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.chat_vision_events
    ADD CONSTRAINT chat_vision_events_message_id_fkey
    FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.character_insights_events
    ADD CONSTRAINT character_insights_events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.character_insights_events
    ADD CONSTRAINT character_insights_events_message_id_fkey
    FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.user_insights_events
    ADD CONSTRAINT user_insights_events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.user_insights_events
    ADD CONSTRAINT user_insights_events_message_id_fkey
    FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;

-- The two self-referential pointers on chat_messages. `user_message_id` is the
-- "which turn is this" pointer downstream routes replies by; 0056 chose SET
-- NULL for the identical pointer on companion_affinity_events, and this matches
-- it. A session delete cascades every message away regardless.

CREATE INDEX idx_chat_messages_continues_from
    ON engine.chat_messages (continues_from_message_id)
    WHERE continues_from_message_id IS NOT NULL;

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_user_message_id_fkey
    FOREIGN KEY (user_message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_continues_from_message_id_fkey
    FOREIGN KEY (continues_from_message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.chat_images_events
    ADD CONSTRAINT chat_images_events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.companion_insights_events
    ADD CONSTRAINT companion_insights_events_message_id_fkey
    FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES engine.chat_sessions(id)
    ON DELETE SET NULL NOT VALID;

ALTER TABLE engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_message_id_fkey
    FOREIGN KEY (message_id) REFERENCES engine.chat_messages(id)
    ON DELETE SET NULL NOT VALID;
