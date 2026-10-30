-- SPDX-License-Identifier: AGPL-3.0-only
-- Romantic life choices (spec 2026-10-30 §4.8): `tier_gap` belongs to
-- `ask_for_space` alone, so the other kinds carry none.
ALTER TABLE engine.life_choices ALTER COLUMN tier_gap DROP NOT NULL;
ALTER TABLE engine.life_choices
    ADD CONSTRAINT life_choices_tier_gap_kind
    CHECK ((kind = 'ask_for_space') = (tier_gap IS NOT NULL));
