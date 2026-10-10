-- SPDX-License-Identifier: AGPL-3.0-only
--
-- A tip is a user action (docs/superpowers/specs/2026-10-11-tip-as-action-design.md).
--
-- Every gift_user row becomes a user row whose metadata.action is
-- {type: "tip", amount_usd, response: "accept"}: each such turn was accepted,
-- because the removed tip path never let the persona refuse. Only the
-- synthesized placeholder "(打赏 $X)" is cleared; a tip that carried the
-- user's own words keeps them. read_at stays NULL on these rows (no receipt;
-- see 0053). Every gift_user row carries tips_amount_usd (0027 removed the
-- rest).

UPDATE engine.chat_messages
SET role     = 'user',
    content  = CASE WHEN content ~ '^\(打赏 \$[0-9]+(\.[0-9]+)?\)$'
                    THEN '' ELSE content END,
    metadata = (metadata - 'tips_amount_usd')
               || jsonb_build_object('action', jsonb_build_object(
                      'type',       'tip',
                      'amount_usd', metadata -> 'tips_amount_usd',
                      'response',   'accept'))
WHERE role = 'gift_user';

ALTER TABLE engine.chat_messages DROP CONSTRAINT chat_messages_role_check;
ALTER TABLE engine.chat_messages ADD CONSTRAINT chat_messages_role_check
    CHECK (role IN ('user', 'assistant', 'system_error'));

DROP INDEX engine.chat_messages_tips_amount_idx;
