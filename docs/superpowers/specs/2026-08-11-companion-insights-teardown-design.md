# Companion Insights Teardown & Direct-Write Design

**Date:** 2026-08-11
**Status:** Approved (brainstorm complete)
**Scope:** one PR, branch off `dev`

## Motivation

`engine.companion_insights` is a legacy of the eros-gateway era. The system has
grown a read/write asymmetry around it:

- **Write:** the insight extractor merges into `companion_insights` (one JSONB
  blob per user), then mirrors into `engine.human_insights` (typed columns) via
  a separate, non-transactional projection.
- **Read:** prompt building reads only `human_insights`. Nothing in prompt
  assembly reads `companion_insights` anymore (`insights_to_bullets` is
  `#[cfg(test)]`).

The JSONB blob was a design mistake we are now correcting: each extraction run
only updates a few fields, yet the write path does load → shallow-merge in Rust
→ full-blob upsert (two round-trips, last-write-wins race), and the mirror then
full-overwrites all 21 columns. JSONB's supposed benefits do not hold up —
"field flexibility" is illusory (the field set is fixed by the stage-2 prompt
schema), and "return JSON directly" is trivially replicated by composing JSON
from columns. Meanwhile the blob cannot be queried with plain SQL and silently
accumulates off-schema keys emitted by the LLM.

`companion_insights.training_level` is an even earlier relic: computed and
written on every merge but never consumed by anything except the lead/CTA
chain, which itself is gateway-era downstream logic. Per OSS discipline,
downstream scoring belongs downstream.

## Decisions

- **D1 — Teardown.** Drop `engine.companion_insights` and the `training_level`
  concept. The extractor writes `human_insights` directly with per-column
  incremental updates.
- **D2 — Lead/CTA chain goes with it.** `training_level` is the chain's only
  input (`training_level` → `lead_score = level×10` on `chat_sessions` →
  `should_show_cta` → SSE final frame). Remove `refresh_lead_score`,
  `fut_lead`, `should_show_cta`, and the `agent_training_level` field from
  both the SSE final frame and the profile route. The reader verification
  (done at planning) found the remaining `lead_score` readers are all
  exporters of the same dead chain — the SSE final frame's normalised
  `lead_score` field and `SessionListEntry.lead_score` on
  `GET /comp/chat/{user_id}/sessions` — so they are removed with it and
  `chat_sessions.lead_score` is dropped.
- **D3 — Snapshots re-pointed.** Freeze `companion_insights_snapshot` (keep
  table and history, including historical `training_level`; stop writing).
  Create `human_insights_snapshot`; `snapshot_all_users` snapshots
  `to_jsonb(human_insights row)` — snapshots are an append-only archive, and
  `to_jsonb` means future column additions need no snapshot migration.
- **D4 — Matching columns stay.** The four matching columns and the
  `matching_preferences` branch of the stage-2 extraction schema are kept as
  user-profile data. Recorded here: they currently have **no consumer** (no
  prompt renders them — `handlers.rs` comment — and no API exposes them; the
  two GIN indexes on `interests`/`personality_traits` also have no production
  query). GIN indexes untouched.
- **D5 — Profile route becomes a typed DTO.** `GET /comp/user/{user_id}/profile`
  returns flat typed `human_insights` fields; openapi gets a full typed schema
  (no more `value_type = Object` black box).
- **D6 — `companion_insights_events` is preserved** — table, name, write
  behavior (`facts`/`structured` stages), payload shape, and purpose all
  unchanged. Two audit-semantics caveats from the new data flow:
  1. The `existing_insights` context conditioning a recorded structured run
     now comes from the `human_insights` reverse projection, not the old
     JSONB blob — an event is now "delta relative to human_insights".
  2. Off-schema keys emitted by the LLM will appear in event payloads but
     never persist anywhere (the old JSONB used to accept them). Any future
     events↔store reconciliation must compare on payload keys ∩ column set.
     `docs/llm-audit.md` gets one sentence stating this.
- **D7 — Out of scope.** `companion_memories.category` needs no work here: it
  is implemented end-to-end (dreaming-lite writes it; `search_profile_grouped`
  partitions recall by it). The NULL-majority is the raw-turn writer's designed
  behavior; perceived granularity issues are a dreaming-lite model/prompt
  concern, not schema work. Also out of scope: any matching feature, any
  historical-session backfill mechanism.

## §1 New write path

`extract_insights` keeps its two LLM stages and event auditing unchanged.
Changes are at the ends of the pipeline:

- **Stage-2 existing context:** `HumanInsightRepo::load`, then reverse-project
  the row into the prompt-schema JSON shape: only populated columns are
  emitted; arrays only if non-empty; the four matching columns re-nest into a
  `matching_preferences` object emitted only if any of the four is set. The
  stage-2 prompt schema itself is unchanged.
- **Write:** new `HumanInsightRepo::apply_extraction(user_id, parsed)` — a
  single `INSERT … ON CONFLICT (user_id) DO UPDATE` statement.
  - Scalar columns: extracted value present → overwrite; absent → keep
    (COALESCE semantics).
  - Array columns (`interests`, `personality_traits`, `deal_breakers`):
    non-empty extracted array → overwrite; empty or absent → keep.
  - `updated_at = now()` on every successful apply.
  - **No erase path** (non-goal): an explicit `null` from the LLM cannot clear
    a column. The old blob-merge technically could; no prompt ever instructed
    it.
  - One round-trip; the old load→merge→upsert whole-blob last-write-wins race
    narrows to column-level last-write-wins — strictly better.
- **Deleted:** `project_from_insights` and the mirror call in `post_process`;
  the `backfill-human-insights` CLI subcommand (with direct writes there is no
  mirror left to repair). `project_columns` is NOT deleted — `apply_extraction`
  still uses it.

## §2 Teardown inventory

| Object | Disposition |
|---|---|
| `engine.companion_insights` table | dropped (§3) |
| `InsightRepo` (`load`/`merge`), `merge_objects`, `compute_training_level`, `WEIGHTS` | deleted; `insight.rs` keeps `InsightEventRepo` and the snapshot repo |
| `insights_to_bullets` (`#[cfg(test)]`) | deleted |
| `refresh_lead_score`, `fut_lead`, `should_show_cta` | deleted |
| `chat_sessions.lead_score` | dropped in §3 migration (reader grep done: only same-chain exporters remained, removed below) |
| SSE final frame fields `lead_score`, `agent_training_level`, `should_show_cta` | removed (wire break; docs updated) |
| `SessionListEntry.lead_score` on `GET /comp/chat/{user_id}/sessions` | removed (wire break; docs updated) |
| `GET /comp/user/{user_id}/profile` | `ProfileResponse` rewritten as flat typed DTO: `user_id`, all 21 `human_insights` data columns (matching four included), `updated_at`; openapi regenerated |
| `companion_insights_snapshot` + `snapshot_all_users` | table frozen in place (history kept); writer re-pointed to new `human_insights_snapshot` |
| `backfill-human-insights` CLI | removed (`main.rs` dispatch + `docs/deploying.md`) |
| `companion_insights_events` | untouched (D6 caveats are documentation-only) |

## §3 Migration `0042_drop_companion_insights.sql`

1. **Final reconciliation backfill** (belt-and-braces — the live mirror should
   already be in sync): per-column projection `FROM engine.companion_insights` with
   `ON CONFLICT (user_id) DO UPDATE` — existing `human_insights` values win;
   only gaps are filled. Scalars: `COALESCE(hi.col, projected)`. Arrays are
   `NOT NULL DEFAULT '{}'` so COALESCE never fires; use
   `CASE WHEN hi.col = '{}' THEN projected ELSE hi.col END`. Off-schema keys
   the JSONB may have accumulated are not projected and are discarded with the
   table.
2. `DROP TABLE engine.companion_insights;`
3. `CREATE TABLE engine.human_insights_snapshot (id UUID PK DEFAULT
   gen_random_uuid(), user_id UUID NOT NULL, snapshot JSONB NOT NULL,
   captured_at TIMESTAMPTZ NOT NULL)` + index `(user_id, captured_at DESC)` +
   house-style REVOKE/RLS lockdown.
4. `ALTER TABLE engine.chat_sessions DROP COLUMN lead_score;` (gated on the §2
   verification step).

**Rollout note:** in the window between `migrate` running and the new binary
taking over, the OLD binary fails on every session-touching endpoint — its
`ChatSession` derives `FromRow` including `lead_score` and decodes
`SELECT *`/`RETURNING *` rows, so after step 4 drops the column every decode
hits `ColumnNotFound` (start_chat, stream ownership check, history, voice,
list_sessions → 5xx). Only post_process extraction and the final-frame lead
read were fail-open. Stop old instances before/at migrate, or accept a
seconds-level 5xx window — a normal deploy restart already implies a brief one.

## §4 Testing

- Rewrite the full-overwrite semantics locks in `human_insight.rs` as
  incremental-semantics locks (present→overwrite / absent→keep / arrays
  non-empty→overwrite). The semantics change is the purpose of this PR —
  within regression-lock policy for deliberate behavior changes.
- `post_process` integration tests: assert direct write + unchanged event
  rows; delete mirror assertions.
- Reverse-projection unit tests (populated-only emission, matching re-nest).
- Column-order pin test for `human_insights_snapshot` (old snapshot table's
  pin test stays — the frozen table still exists).
- Profile DTO tests; regenerate `openapi.json`.
- CI gates as usual: fmt / clippy / test / openapi.

## §5 Documentation sweep

Sweep by **concept**, not identifier: training level, companion insights,
lead score, CTA, mirror/projection. Known touchpoints:

- `docs/api-reference.md` + `.zh.md` — SSE final frame fields, profile
  response shape
- `docs/deploying.md` — remove `backfill-human-insights`
- `docs/world-system.md:230` — "superset of `companion_insights`" wording →
  `human_insights`
- `docs/llm-audit.md` — D6 reconciliation-scope sentence; confirm no stale
  table references
- `examples/*.toml` comments, `.env.example`, `README`
- Release notes: breaking changes below

## Breaking changes (release notes)

1. SSE `final` frame: `lead_score`, `agent_training_level`, and
   `should_show_cta` removed.
2. `GET /comp/user/{user_id}/profile`: response is now a flat typed
   `human_insights` DTO; `companion_insights` (raw JSONB) and
   `agent_training_level` fields are gone.
   `GET /comp/chat/{user_id}/sessions`: `lead_score` removed from entries.
3. CLI subcommand `backfill-human-insights` removed.
4. Tables: `engine.companion_insights` dropped; `chat_sessions.lead_score`
   dropped; `companion_insights_snapshot` frozen (no longer written);
   `engine.human_insights_snapshot` added.
