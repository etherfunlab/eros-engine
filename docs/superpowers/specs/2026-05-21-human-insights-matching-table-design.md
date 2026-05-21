# Human Insights Matching Table — Design

**Status:** Draft for review
**Date:** 2026-05-21
**Owner:** @enriquephl

## Problem

The structured user profile mined from conversations lives only in
`engine.companion_insights` as a single JSONB blob (`insights`) keyed by
`user_id`, written each turn by the `insight_extraction` pipeline
(`post_process.rs:496` → `InsightRepo::merge`). That shape is perfect for
the prompt-rendering read path (bullet list back into the system prompt) but
useless for **user↔user matching**: you cannot index a JSONB blob's nested
fields for array-overlap or range queries without bolting on expression
indexes, and the blob mixes free text, scalars, arrays, and a nested
`matching_preferences` object.

We want a second table, `engine.human_insights`, that is a **flat, typed
projection** of the soft (conversation-derived) signal, shaped for matching
queries. It is read for matching, **not** rendered to frontend users.

## Non-Goals

- **Don't touch `companion_insights`.** It runs smoothly and stays the
  source of truth. The LLM merge hot path is not modified beyond capturing
  its already-returned row (see Write-Through).
- **No hard-filter attributes.** Gender / age / geography hard filters are
  the job of the **user-self-filled profile table** (owned elsewhere, not in
  `engine`), which is authoritative and structured. `human_insights` carries
  **only** conversation-derived soft signal. We deliberately do **not** add
  `own_gender` / `own_age` columns, and "later change the update mechanism"
  will never add hard-attribute extraction here.
- **No matching algorithm.** This spec ships the table + projection + sync,
  not a ranking/recommendation query. Matching will JOIN the profile table
  (hard `WHERE` filter) against `human_insights` (GIN overlap / soft score).
- **No new LLM extraction.** Phase 1 mirrors the existing
  `companion_insights` JSONB. A dedicated human-matching extractor is future
  work, deferred to "later change the update mechanism".
- **No API / OpenAPI surface.** No HTTP route reads or writes this table in
  v1. It is an internal store primitive.

## High-Level Design

```
insight_extraction (per turn, post_process.rs)
   │
   ▼
InsightRepo::merge(user_id, new_insights)  ── writes companion_insights JSONB
   │   returns CompanionInsightsRow { insights, .. }   (today: discarded)
   ▼
HumanInsightRepo::project_from_insights(user_id, &row.insights)   ── NEW write-through
   │   parse JSONB → typed columns
   ▼
engine.human_insights  (flat, indexed for matching)
```

`companion_insights` stays canonical. `human_insights` is a derived mirror
in Phase 1. The projection logic is isolated in one function
(`project_from_insights`) so the trigger and the source can be swapped later
without touching callers.

## Data Model

New migration `crates/eros-engine-store/migrations/0015_human_insights.sql`.

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- Flat, typed projection of the soft (conversation-derived) user profile,
-- shaped for user<->user matching. companion_insights stays the source of
-- truth (JSONB); this table is a derived mirror in Phase 1.
--
-- Hard-filter attributes (own gender/age/geo) are NOT here by design — they
-- live in the user-self-filled profile table owned outside engine.* and are
-- joined at match time.
CREATE TABLE engine.human_insights (
    user_id            UUID PRIMARY KEY,

    -- soft scalar signal (free text; carried for context / future embedding,
    -- not used for hard filtering)
    city               TEXT,            -- conversational mention only; geo
                                        -- hard-filter lives in profile table
    occupation         TEXT,
    mbti_guess         TEXT,
    love_values        TEXT,
    emotional_needs    TEXT,
    life_rhythm        TEXT,

    -- array signal — the core matching dimensions (set overlap via &&)
    interests          TEXT[] NOT NULL DEFAULT '{}',
    personality_traits TEXT[] NOT NULL DEFAULT '{}',

    -- flattened matching_preferences: "what the user wants" (complements the
    -- self-filled "what the user is" in the profile table)
    preferred_gender   TEXT,
    age_min            INT,
    age_max            INT,
    deal_breakers      TEXT[] NOT NULL DEFAULT '{}',

    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Only the array-overlap dimensions get indexes. No city index — geo
-- filtering is the profile table's job.
CREATE INDEX idx_human_insights_interests ON engine.human_insights USING GIN(interests);
CREATE INDEX idx_human_insights_traits    ON engine.human_insights USING GIN(personality_traits);
```

### Supabase lockdown (mandatory)

Migration `0013_supabase_lockdown.sql` REVOKEs grants + enables RLS on every
`engine.*` table. A new table that skips this would be a hole, so the same
migration must append, guarded by the same `pg_roles` existence checks as
0013:

```sql
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.human_insights FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.human_insights FROM authenticated;
    END IF;
END
$$;
ALTER TABLE engine.human_insights ENABLE ROW LEVEL SECURITY;
```

(Schema USAGE for anon/authenticated is already revoked by 0013 and is not
re-granted, so no USAGE handling is needed here.)

### Field mapping (companion_insights JSONB → columns)

| JSONB path                            | Column              | Type      | Rule |
|---------------------------------------|---------------------|-----------|------|
| `city`                                | `city`              | TEXT      | string or NULL |
| `occupation`                          | `occupation`        | TEXT      | string or NULL |
| `mbti_guess`                          | `mbti_guess`        | TEXT      | string or NULL |
| `love_values`                         | `love_values`       | TEXT      | string or NULL |
| `emotional_needs`                     | `emotional_needs`   | TEXT      | string or NULL |
| `life_rhythm`                         | `life_rhythm`       | TEXT      | string or NULL |
| `interests` (array)                   | `interests`         | TEXT[]    | string items only; missing → `{}` |
| `personality_traits` (array)          | `personality_traits`| TEXT[]    | string items only; missing → `{}` |
| `matching_preferences.preferred_gender` | `preferred_gender`| TEXT      | string or NULL |
| `matching_preferences.age_range[0]`   | `age_min`           | INT       | int or NULL; malformed → NULL |
| `matching_preferences.age_range[1]`   | `age_max`           | INT       | int or NULL; malformed → NULL |
| `matching_preferences.deal_breakers`  | `deal_breakers`     | TEXT[]    | string items only; missing → `{}` |

`age_range` is `[min_int, max_int]` per `COMPANION_INSIGHTS_SCHEMA`
(`prompt.rs:404`). A non-array, wrong-length, or non-integer `age_range`
yields `age_min = age_max = NULL` rather than failing the projection.

## Store Layer

New file `crates/eros-engine-store/src/human_insight.rs`; register
`pub mod human_insight;` in `crates/eros-engine-store/src/lib.rs`.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HumanInsightsRow {
    pub user_id: Uuid,
    pub city: Option<String>,
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub preferred_gender: Option<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub deal_breakers: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

pub struct HumanInsightRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> HumanInsightRepo<'a> {
    /// Project a companion_insights JSONB blob into the flat matching row and
    /// UPSERT. This is the ONLY place that knows the JSONB→columns mapping, so
    /// the source/trigger can be repointed later without touching callers.
    pub async fn project_from_insights(
        &self,
        user_id: Uuid,
        insights: &serde_json::Value,
    ) -> Result<(), sqlx::Error> { /* parse → INSERT ... ON CONFLICT DO UPDATE */ }

    pub async fn load(&self, user_id: Uuid) -> Result<Option<HumanInsightsRow>, sqlx::Error> { /* ... */ }
}
```

Parsing is a pure helper (`fn project_columns(insights: &Value) -> ProjectedColumns`)
unit-tested without a DB. The UPSERT writes all columns + `updated_at = now()`:

```sql
INSERT INTO engine.human_insights
    (user_id, city, occupation, mbti_guess, love_values, emotional_needs,
     life_rhythm, interests, personality_traits, preferred_gender,
     age_min, age_max, deal_breakers)
VALUES ($1, $2, ..., $13)
ON CONFLICT (user_id) DO UPDATE SET
    city = EXCLUDED.city, ..., deal_breakers = EXCLUDED.deal_breakers,
    updated_at = now()
```

Full-overwrite semantics (not field-merge): `companion_insights` already
holds the cumulatively-merged blob, so each projection writes the complete
current state. No merge logic is duplicated here.

## Write-Through

In `post_process.rs::extract_insights` (around line 536). Today:

```rust
if let Err(e) = insights_repo.merge(user_id, new_insights).await {
    tracing::warn!("companion_insights merge failed: {e}");
}
```

`InsightRepo::merge` already returns the merged `CompanionInsightsRow`
(`insight.rs:84`), currently discarded. Capture it and project — **no extra
DB read**:

```rust
match insights_repo.merge(user_id, new_insights).await {
    Ok(row) => {
        let human_repo = HumanInsightRepo { pool: &state.pool };
        if let Err(e) = human_repo.project_from_insights(user_id, &row.insights).await {
            tracing::warn!("human_insights projection failed: {e}");
        }
    }
    Err(e) => tracing::warn!("companion_insights merge failed: {e}"),
}
```

Projection failure only warns — it never breaks the turn, matching the
existing fire-and-forget post-process style. Projection runs only when there
were new insights to merge (the `extract_insights` early-returns already
gate that).

## Backfill (manual command)

A `eros-engine backfill-human-insights` subcommand in `main.rs`, mirroring
the `seed-personas` pattern (`main.rs:52`). Following the project rule that
data-mutating maintenance stays manual (never wired into `release_command`),
this is operator-run, not automatic:

```rust
Some("backfill-human-insights") => run_backfill_human_insights().await,
```

It streams `companion_insights` and projects each row:

```sql
-- Conceptually; implemented by looping rows through project_from_insights so
-- the JSONB→columns mapping has exactly one definition.
SELECT user_id, insights FROM engine.companion_insights;
```

Each row → `HumanInsightRepo::project_from_insights`. Idempotent (UPSERT), so
re-running is safe. Logs `backfilled`/`skipped` counts like seed-personas.

Update the usage string at `main.rs:62` and the subcommand doc comment block
at `main.rs:40`.

## Tests

### `human_insight.rs` (unit — no DB)

- `project_columns_full_blob` — every field populated → all columns set,
  arrays preserved in order.
- `project_columns_missing_fields_are_null_and_empty` — `{}` → all scalars
  NULL, all arrays `[]`.
- `project_columns_age_range_parsed` — `matching_preferences.age_range:[18,30]`
  → `age_min=18, age_max=30`.
- `project_columns_malformed_age_range_is_null` — `age_range:"18-30"`,
  `[18]`, `[18,30,40]`, `["a","b"]` each → both NULL.
- `project_columns_array_drops_non_strings` — `interests:["coffee",1,null]`
  → `["coffee"]`.

### `human_insight.rs` (`#[sqlx::test(migrations = "./migrations")]`)

- `project_creates_then_overwrites` — first project creates the row; a second
  project with changed fields overwrites (full state, not merge), `updated_at`
  advances.
- `arrays_roundtrip` — `interests` / `personality_traits` / `deal_breakers`
  written and read back identical.
- `gin_overlap_query_matches` — insert two users with overlapping
  `interests`, assert `WHERE interests && ARRAY['coffee']` returns the right
  set (exercises the GIN index path).
- `load_returns_none_for_unknown_user`.

### `migrations` lockdown (extend `lib.rs` migration_tests)

- `human_insights_has_rls_enabled` — assert `relrowsecurity` is true for
  `engine.human_insights` (mirrors the 0013 guarantee for the new table).

## Risks / Open Questions

1. **Phase-1 mirror is throwaway scaffolding.** When the dedicated
   human-matching extractor lands ("later change the update mechanism"), the
   write-through from `merge` is removed and `project_from_insights` is
   repointed at the new source. Because the JSONB→columns mapping is isolated
   in one function, this is a localized change. Accepted.
2. **No referential integrity to a users table.** `companion_insights` itself
   has no FK on `user_id` (it's a bare PK), so `human_insights` follows the
   same convention — `user_id` is an `auth.users` id from the shared realm,
   not FK-enforced in `engine`.
3. **Preference vs. attribute split must stay clean.** `preferred_gender` /
   `age_range` here are the *querying* user's wants, matched against
   *candidate* users' self-filled profile attributes. Keeping "wants" in
   `human_insights` and "is" in the profile table is the whole design;
   blurring it (e.g. someone later adds `own_gender` here) re-creates the
   ambiguity this split removes. Documented to prevent drift.
4. **City column with no index.** Carried as soft conversational context; if a
   future query wants to weakly boost same-city matches it can still read the
   column. Geo hard-filtering remains the profile table's job. If it proves
   genuinely unused, drop the column in a later migration (YAGNI either way —
   keeping it costs ~nothing).

## Acceptance Criteria

- [ ] `cargo test -p eros-engine-store -p eros-engine-server` green
- [ ] `0015_human_insights.sql` creates the table + 2 GIN indexes and applies
      the lockdown REVOKE/RLS for the new table
- [ ] `human_insights_has_rls_enabled` passes
- [ ] Per-turn chat flow projects into `human_insights` with no extra DB read
      (verified by the write-through using `merge`'s returned row)
- [ ] `eros-engine backfill-human-insights` populates existing
      `companion_insights` users idempotently
- [ ] `companion_insights` write path behaviour is byte-identical except for
      capturing the previously-discarded `merge` return value
