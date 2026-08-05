-- SPDX-License-Identifier: AGPL-3.0-only
--
-- The three original instance-scoped tables — chat_sessions (0001),
-- companion_affinity (0002) and companion_memories (0003) — reference
-- engine.persona_instances(id) by convention only. They predate the
-- world/story tables (0035-0039), which all declare a real foreign key, so
-- nothing at the DB level stopped a bogus instance_id from being written and
-- no cascade fired when an instance row was removed out of band.
--
-- The practical cost beyond integrity: without a declared FK the Supabase
-- table editor renders instance_id as a plain UUID column, so there is no way
-- to navigate from a session to its persona instance — you copy the UUID out
-- and search the other table by hand, which is easy to get wrong when a row
-- carries two similar-looking UUIDs (its own id and its instance_id).
--
-- ON DELETE CASCADE matches all six existing FKs into persona_instances. The
-- engine itself never DELETEs an instance — it flips status to 'archived' and
-- PersonaStore::ensure_active_instance reactivates it — so this only governs
-- deliberate out-of-band deletion, where wiping the instance's data is the
-- point. chat_messages already cascades from chat_sessions, so dropping an
-- instance now takes its whole conversation history with it in one step.
--
-- companion_memories.instance_id stays nullable (NULL = profile layer, see
-- 0003); a foreign key ignores NULL, so profile-layer rows are unaffected.
--
-- All three columns were verified orphan-free beforehand and the tables are
-- small, so the constraints go in validated rather than NOT VALID + VALIDATE.

ALTER TABLE engine.chat_sessions
    ADD CONSTRAINT chat_sessions_instance_id_fkey
    FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id)
    ON DELETE CASCADE;

ALTER TABLE engine.companion_affinity
    ADD CONSTRAINT companion_affinity_instance_id_fkey
    FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id)
    ON DELETE CASCADE;

ALTER TABLE engine.companion_memories
    ADD CONSTRAINT companion_memories_instance_id_fkey
    FOREIGN KEY (instance_id) REFERENCES engine.persona_instances(id)
    ON DELETE CASCADE;
