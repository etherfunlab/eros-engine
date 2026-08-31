-- SPDX-License-Identifier: AGPL-3.0-only
-- Release B2 of three. 0059 created engine.llm_generations, Release A made
-- every call site write to it, and 0060 (B1) gave it its history and its
-- constraints while the code stopped writing model and usage to the child
-- tables. This migration drops what nothing writes any more: the same
-- generation's model and usage now live in exactly one place, reached by the
-- join on generation_id.
--
-- Spec: docs/superpowers/specs/2026-08-24-llm-generations-audit-design.md §7.4
--
--
-- PRECONDITION, symmetric to §7.0 but with no one-query check. Do not run
-- this until the build serving traffic on EVERY machine is B1 (v1.7.0) or
-- later. The evidence is an absence — the running code no longer naming these
-- columns in any INSERT — so it cannot be read out of the database: check
-- `fly image show` (or your deployment's equivalent) on every machine, and
-- that no INSERT in eros-engine-store still names model or usage on these
-- tables. Run it over a build that still writes them and every rollout has a
-- window where the old machines INSERT into columns that no longer exist —
-- which for chat_messages is a user's reply failing to persist.
--
-- ROLLBACK FLOOR. Once this is deployed, the oldest build that can be rolled
-- back to is B1. Release A and 1.6.2 name model and usage in every child
-- INSERT, and those columns no longer exist.
--
-- chat_messages.filter_model SURVIVES on purpose. It is not the model of the
-- f_generation_id generation: 2,024 rows hold the regex arm's '<regex>'
-- sentinel with no f_generation_id at all, against 1 holding a real slug. It
-- is a discriminator that happens to be spelled like a model name, not a
-- redundant copy (spec §7.4).
--
-- DROP COLUMN is a catalog-only change — no table rewrite — but it takes
-- ACCESS EXCLUSIVE, so 0060's lock_timeout reasoning carries over verbatim: a
-- failed release_command is the SAFE failure, waiting behind a long
-- transaction holds chat_messages blocked where users can see it. Same
-- ordering discipline too: coldest table first, chat_messages last, one ALTER
-- per table so each takes its lock exactly once.

SET LOCAL lock_timeout = '3s';

ALTER TABLE engine.user_insights_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.chat_vision_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.chat_images_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.character_insights_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.companion_affinity_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.companion_decision_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.companion_insights_events
    DROP COLUMN model,
    DROP COLUMN usage;

ALTER TABLE engine.chat_messages
    DROP COLUMN model,
    DROP COLUMN usage;
