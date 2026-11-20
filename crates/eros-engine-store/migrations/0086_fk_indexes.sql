-- Index the foreign keys that no index serves (none leads with the column, or
-- the one that does is partial on something else), so a delete on the
-- referenced row (a persona instance, a story event, a chat session) finds its
-- dependents by index instead of scanning the table.

CREATE INDEX IF NOT EXISTS idx_companion_affinity_instance_id
    ON engine.companion_affinity (instance_id);
CREATE INDEX IF NOT EXISTS idx_companion_memories_instance_id
    ON engine.companion_memories (instance_id);
CREATE INDEX IF NOT EXISTS idx_companion_memories_session_id
    ON engine.companion_memories (session_id);
CREATE INDEX IF NOT EXISTS idx_chat_turn_queue_session_id
    ON engine.chat_turn_queue (session_id);
CREATE INDEX IF NOT EXISTS idx_persona_story_memories_event_id
    ON engine.persona_story_memories (event_id);
CREATE INDEX IF NOT EXISTS idx_persona_story_memories_instance_id
    ON engine.persona_story_memories (instance_id);
CREATE INDEX IF NOT EXISTS idx_world_memories_instance_id
    ON engine.world_memories (instance_id);
CREATE INDEX IF NOT EXISTS idx_world_post_comments_author_instance_id
    ON engine.world_post_comments (author_instance_id);
CREATE INDEX IF NOT EXISTS idx_world_posts_instance_id
    ON engine.world_posts (instance_id);
