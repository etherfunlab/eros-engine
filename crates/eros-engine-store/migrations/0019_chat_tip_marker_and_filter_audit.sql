-- SPDX-License-Identifier: AGPL-3.0-only
--
-- chat_messages gains:
--   metadata           — open marker bag for user-side rows (today: tip amount)
--   pre_filter_content, filter_model, filter_triggers, f_client_msg_id,
--   f_generation_id   — assistant-side filter audit, written only when the
--                       chat-reply output filter actually rewrote the reply.
--
-- All six columns are nullable and default-safe; existing inserts that do not
-- mention them keep working. No backfill.
--
-- Spec: docs/superpowers/specs/2026-05-26-tip-role-and-filter-audit-design.md

ALTER TABLE engine.chat_messages
    ADD COLUMN metadata             JSONB NULL,
    ADD COLUMN pre_filter_content   TEXT  NULL,
    ADD COLUMN filter_model         TEXT  NULL,
    ADD COLUMN filter_triggers      JSONB NULL,
    ADD COLUMN f_client_msg_id      TEXT  NULL,
    ADD COLUMN f_generation_id      TEXT  NULL;

-- Audit index for tip-amount aggregation. Partial: only rows that carry a tip.
CREATE INDEX chat_messages_tips_amount_idx
    ON engine.chat_messages ((metadata->>'tips_amount_usd'))
    WHERE metadata ? 'tips_amount_usd';

-- f_client_msg_id is engine-generated per filter LLM call; enforce that a
-- single logical filter call writes at most one row per session.
CREATE UNIQUE INDEX chat_messages_f_client_msg_id_uidx
    ON engine.chat_messages (session_id, f_client_msg_id)
    WHERE f_client_msg_id IS NOT NULL;
