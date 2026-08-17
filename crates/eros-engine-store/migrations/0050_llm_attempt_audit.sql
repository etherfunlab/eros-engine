-- 0050: per-attempt LLM failure audit.
--
-- Two columns, five tables, one shape. The split is by who authored the fact:
--
--   llm_attempts   what the provider said. Every element has an http_status
--                  (200 for a mid-stream error envelope). Raw u16, never an
--                  enum — OpenRouter reports overload as 529 while Venice uses
--                  429/503, and the next provider will differ again.
--   gateway_errors where the engine's path to the provider broke: timeouts,
--                  transport drops, decode failures, and the chain-scoped
--                  chain_exhausted. Named for the gateway role, not the
--                  process: a bug in the affinity math does not belong here.
--
-- Each table's existing coarse marker keeps its own business verdict. NULL
-- means nothing to record; an empty array is never written.
--
-- Elements are told apart by `task`, the existing [tasks.*] key, so
-- engine.chat_messages can host three call sites (chat_companion / chat_voice /
-- chat_product_qa, chat_output_filter, chat_input_filter) in one column.

ALTER TABLE engine.chat_messages
    ADD COLUMN llm_attempts JSONB,
    ADD COLUMN gateway_errors JSONB;

ALTER TABLE engine.chat_vision_events
    ADD COLUMN llm_attempts JSONB,
    ADD COLUMN gateway_errors JSONB;

ALTER TABLE engine.chat_images_events
    ADD COLUMN llm_attempts JSONB,
    ADD COLUMN gateway_errors JSONB;

ALTER TABLE engine.companion_decision_events
    ADD COLUMN llm_attempts JSONB,
    ADD COLUMN gateway_errors JSONB;

ALTER TABLE engine.companion_affinity_events
    ADD COLUMN llm_attempts JSONB,
    ADD COLUMN gateway_errors JSONB;

-- companion_decision_events.status is NOT NULL, so a judge call that failed on
-- transport needs a legal value. `timeout` and `error` stay accepted so rows
-- written before this migration remain valid; the engine stops writing them in
-- favour of the two pointer values, which say only "an attempt failed, the
-- detail is in that column". No backfill: rewriting production audit history to
-- gain vocabulary uniformity is not worth it.
ALTER TABLE engine.companion_decision_events
    DROP CONSTRAINT companion_decision_events_status_check;

ALTER TABLE engine.companion_decision_events
    ADD CONSTRAINT companion_decision_events_status_check
    CHECK (status IN ('ok', 'empty', 'parse_error', 'timeout', 'error',
                      'upstream_error', 'gateway_error'));
