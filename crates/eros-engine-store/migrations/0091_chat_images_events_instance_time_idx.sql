-- Index an instance's picture rows, newest first, so the read of her last
-- picture inside the window before the turn's moment (spec
-- 2026-11-28-wardrobe-steward-design.md §5.3) finds it by index. The index
-- leads with `instance_id`, so it also serves that foreign key, and takes the
-- place of the single-column index on it.

CREATE INDEX IF NOT EXISTS idx_chat_images_events_instance_time
    ON engine.chat_images_events (instance_id, created_at DESC);

DROP INDEX IF EXISTS engine.idx_chat_images_events_instance;
