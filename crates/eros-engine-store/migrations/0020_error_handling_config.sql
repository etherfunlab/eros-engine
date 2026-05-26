-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Generic key-value config bag for error-handling parameters.
-- First case: chat-stream complete failure → pseudo-ghost a casual phrase
-- instead of dumping an Error frame to the client.
--
-- Spec: docs/superpowers/specs/2026-05-26-error-fallback-config-design.md

CREATE TABLE engine.error_handling_config (
    kind        TEXT PRIMARY KEY,
    payload     JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Seed: 10 short pseudo-ghost phrases. Codex-generated 2026-05-26 to be
-- universal (simple English + emoji, no i18n needed). Downstream operators
-- can UPDATE / INSERT to adjust per deployment.
INSERT INTO engine.error_handling_config (kind, payload) VALUES
  ('chat_stream_failure_fallback_phrases',
   '["huh?","hm?","...","oh?","mhm","ok","👀","😅","say again?","wait what?"]'::jsonb);
