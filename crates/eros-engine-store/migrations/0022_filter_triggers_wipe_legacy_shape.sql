-- SPDX-License-Identifier: AGPL-3.0-only
-- v0.5.1 ships a new shape for engine.chat_messages.filter_triggers
-- (config-as-declared). The legacy shape (random as {p,draw}, models as a
-- single id, traits as an observed tag array) cannot be reconstructed back
-- to source TOML because observed values do not carry the `when` mode. Null
-- the legacy audit so readers never see mixed shapes. The "filter ran"
-- signal is preserved on the row via filter_model NOT NULL; only the
-- per-predicate detail is lost.
--
-- This runs at the v0.5.1 upgrade, before any new-shape row can exist, so
-- every non-null filter_triggers is legacy — wipe on that condition alone.
--
-- Spec: docs/superpowers/specs/2026-05-29-engine-cleanup-and-snapshot-design.md §2

UPDATE engine.chat_messages
   SET filter_triggers = NULL
 WHERE filter_triggers IS NOT NULL;
