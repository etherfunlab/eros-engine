-- SPDX-License-Identifier: AGPL-3.0-only
-- One-time cleanup: remove legacy in-app Gift Event rows from chat_messages.
-- The event_gift endpoint (removed in #76) wrote role='gift_user' rows with a
-- bare label (e.g. "rose") and NO tips_amount_usd metadata. With gift_user now
-- tip-only, those rows would be miscounted as user turns by assemble_chat_request
-- and compute_signals_for_session. This deletes exactly the non-tip gift_user
-- rows; tips (gift_user rows carrying metadata.tips_amount_usd) are preserved.
-- Idempotent; a no-op for fresh/OSS deployments (eros-app, the only event_gift
-- caller, is the sole source of such rows). companion_affinity_events
-- event_type='gift' audit rows are intentionally left (append-only, already
-- EMA-applied, still listed by the affinity BFF).
--
-- Spec: docs/superpowers/specs/2026-06-03-cleanup-legacy-gift-user-rows-design.md

DELETE FROM engine.chat_messages
WHERE role = 'gift_user'
  AND (metadata IS NULL OR NOT (metadata ? 'tips_amount_usd'));
