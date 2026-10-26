-- SPDX-License-Identifier: AGPL-3.0-only
--
-- `event` joins the composer audit's source vocabulary: a life event whose
-- message carries her picture (POST /v2/comp/session/{session_id}/events,
-- `taking_selfie`). Its `inputs` are the chat composer's slots.

ALTER TABLE engine.chat_images_events
    DROP CONSTRAINT chat_images_events_source_check;

ALTER TABLE engine.chat_images_events
    ADD CONSTRAINT chat_images_events_source_check
    CHECK (source IN (
        'chat_reply_text_image', 'chat_reply_image',
        'compose_endpoint', 'compose_endpoint_stream',
        'image_edit', 'event'
    ));
