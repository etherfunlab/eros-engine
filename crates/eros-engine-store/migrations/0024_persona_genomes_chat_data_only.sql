-- SPDX-License-Identifier: AGPL-3.0-only
-- DESTRUCTIVE. BREAKING. Strips persona_genomes to chat-relevant fields.
-- engine no longer judges persona availability or stores display data;
-- catalog / availability / avatar are downstream concerns keyed by genome_id.
--
-- Spec: docs/superpowers/specs/2026-06-01-persona-genomes-chat-data-only-design.md

ALTER TABLE engine.persona_genomes DROP COLUMN IF EXISTS is_active;
ALTER TABLE engine.persona_genomes DROP COLUMN IF EXISTS avatar_url;
