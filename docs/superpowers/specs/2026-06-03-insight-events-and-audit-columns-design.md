# eros-engine — Insight extraction events + OpenRouter audit columns (Spec B1)

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track. **One migration (`0025`).**
**Scope**: a new append-only `companion_insights_events` audit table — **one row per
OpenRouter call** of `insight_extraction` — plus `model` / `usage` / `generation_id`
audit columns on both `companion_insights_events` (new) and `companion_affinity_events`
(existing), wired from the affinity + insight call sites.

This is **Spec B1**, the first of two specs splitting the original "Spec B" extraction
overhaul. **Spec B2** (extraction prompt precision, config-driven extraction prompts,
and the `location` / `nationality` / `hometown` geo-schema fields) is designed separately.

---

## 0. Background

### What item 1 + item 2 ask for

1. **Item 1** — add a `companion_insights_events` table that records each
   `insight_extraction` run, mirroring how `affinity_evaluation` writes
   `companion_affinity_events`.
2. **Item 2** — give both `companion_insights_events` (new) and
   `companion_affinity_events` (existing) the OpenRouter `generation_id`, `model`, and
   `usage` of the call that produced the row.

### The affinity precedent (the pattern item 1 mirrors)

`affinity_evaluation` runs once per qualifying reply turn, makes **one** OpenRouter call,
and `persist_with_event` (`crates/eros-engine-store/src/affinity.rs` @ ~L190-273) writes
**one** `companion_affinity_events` row (`event_type`, `deltas`, `effective_deltas`,
`context`, `created_at`; created in `0002_affinity.sql`, widened by
`0014_affinity_effective_deltas.sql`). The OpenRouter `generation_id` / `model` / `usage`
are currently **only logged** (`log_openrouter_usage`, `crates/eros-engine-server/src/pipeline/mod.rs`
@ ~L25-54) — never persisted.

### Why `insight_extraction` is not a 1:1 mirror

`insight_extraction` runs per produced assistant message (`extract_insights`,
`crates/eros-engine-server/src/pipeline/post_process.rs` @ ~L518) and makes **two**
OpenRouter calls:

1. **`facts`** — `extract_facts` → `{"facts":[...]}` (stage-1 fact list).
2. **`structured`** — `extract_structured_insights` → a JSON object matching
   `COMPANION_INSIGHTS_SCHEMA`, which is then `InsightRepo::merge`d into
   `companion_insights.insights` and projected into `human_insights`.

Each call has its own `generation_id` / `model` / `usage`. **Decision: one row per
OpenRouter call**, discriminated by a `stage` column, so each row carries exactly one
clean audit trio. A `run_id` (generated once per run) ties the two rows of one run
together.

### Two conventions this reuses

- **The audit-column trio already exists.** `chat_messages` carries
  `model TEXT` / `usage JSONB` / `generation_id TEXT` (migration `0012_chat_streaming.sql`).
  The new columns mirror those names and types exactly.
- **The Supabase lockdown boilerplate.** New sensitive tables (`0013`+, e.g.
  `0021_companion_insights_snapshot.sql`) `REVOKE` from `anon` / `authenticated` (wrapped
  in `pg_roles` existence guards so non-Supabase / sqlx-test Postgres skips silently) and
  `ENABLE ROW LEVEL SECURITY`. `companion_insights_events` follows the same pattern.

### Not redundant with `companion_insights_snapshot`

`companion_insights_snapshot` (`0021`) is a periodic **state** history (the full
`insights` JSONB + `training_level`, one row per user per sweeper fire).
`companion_insights_events` is a per-**call** audit log (what each extraction call cost
and produced). Same family, different grain — both append-only, both retained.

---

## 1. Goals / non-goals

**Goals**
- Persist a durable, cost-faithful audit row for **every OpenRouter call** that
  `insight_extraction` makes that **returns a response** — including calls whose output
  was empty (no new facts) or failed to parse (the call still spent tokens).
- Carry `generation_id` / `model` / `usage` on both `companion_insights_events` and
  `companion_affinity_events`.
- Group the two calls of one run via a shared `run_id`.

**Non-goals (out of scope)**
- Any read path — no API / BFF / dashboard surface for these events. Write-only audit
  for now.
- No backfill of existing `companion_affinity_events` rows (the new columns stay `NULL`
  on pre-migration rows).
- `memory_extraction` (`dreaming.rs`) gets **no** events table and **no** audit columns —
  items 1 + 2 name only `insight_extraction` (insights) and `affinity_evaluation`
  (affinity).
- No change to `companion_insights_snapshot`, `companion_insights`, or `human_insights`
  schema (geo-schema work is Spec B2).
- Transport errors (an OpenRouter call that returns **no** response — network / 4xx) are
  still only logged, not rowed (there is no `generation_id` / `usage` to record).

---

## 2. Schema — migration `0025`

One migration file, `crates/eros-engine-store/migrations/0025_insight_events_and_audit_cols.sql`:

### 2a. New table `companion_insights_events`

```sql
CREATE TABLE engine.companion_insights_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id        UUID NOT NULL,             -- shared by both calls of one insight_extraction run
    user_id       UUID NOT NULL,             -- plain UUID, NO foreign key (see note)
    session_id    UUID,                      -- tracing: the session the turn came from
    message_id    UUID,                      -- tracing: the assistant turn that triggered extraction
    stage         TEXT NOT NULL CHECK (stage IN ('facts','structured')),
    status        TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error')),
    payload       JSONB,                     -- facts[] (facts) | insight delta (structured) | NULL (parse_error)
    model         TEXT,                      -- ┐ mirror chat_messages.{model, usage,
    usage         JSONB,                     -- ┤ generation_id} (migration 0012)
    generation_id TEXT,                      -- ┘
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_companion_insights_events_user_time
    ON engine.companion_insights_events (user_id, created_at DESC);
CREATE INDEX idx_companion_insights_events_run
    ON engine.companion_insights_events (run_id);

-- Supabase lockdown, mirroring 0021_companion_insights_snapshot.sql.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.companion_insights_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.companion_insights_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.companion_insights_events ENABLE ROW LEVEL SECURITY;
```

Notes:
- **No FK on `user_id`** (deliberate): a run whose `facts` call returns `{"facts":[]}`
  writes a `stage='facts', status='empty'` row even though no `InsightRepo::merge`
  happened, so a `companion_insights` row may not exist yet — a FK to
  `companion_insights(user_id)` would violate. This table is a decoupled audit log.
  (Contrast `companion_affinity_events`, which FKs to `companion_affinity` because that
  parent row always exists before an event.) `session_id` / `message_id` are likewise
  plain UUIDs for tracing, no FK.
- `payload` for `stage='structured'` is the **returned schema-fill delta** (the object
  `extract_structured_insights` parsed), not the post-merge `companion_insights` state.
- `status`: `ok` (non-empty parsed output), `empty` (parsed but no content — `{"facts":[]}`
  / `{}`), `parse_error` (response did not parse; `payload` is `NULL`).

### 2b. `companion_affinity_events` ALTER (item 2 on the existing table)

```sql
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN model         TEXT,
    ADD COLUMN usage         JSONB,
    ADD COLUMN generation_id TEXT;
```

All nullable; no backfill. Pre-migration rows keep `NULL` in the three new columns.

---

## 3. Call-site wiring

### 3a. `run_id`

Generate `let run_id = Uuid::new_v4();` once at the top of `extract_insights`
(`post_process.rs` @ ~L518) and thread it into both the `facts` and `structured` event
writes so both rows of the run share it.

### 3b. Affinity (`companion_affinity_events`)

`evaluate_affinity` (`post_process.rs` @ ~L464-513) already holds the `ChatResponse`
(`resp`, @ ~L492) whose `generation_id` / `model` / `usage` fields exist on the struct
(`crates/eros-engine-llm/src/openrouter.rs` @ ~L100-114). Thread the trio into
`persist_with_event` / `persist_affinity` (`affinity.rs` @ ~L190-273) and bind them into
the **existing** `INSERT INTO engine.companion_affinity_events (...)` (@ ~L255-266). The
trio rides the insert affinity already performs — no new statement, no new failure point.

> **Follow-up (2026-06-23, PR #91).** The trio is populated only from a *successful*
> `affinity_evaluation` call, which runs only on a substantive text reply (`action ==
> ReplyText`, user message ≥ `AFFINITY_EVAL_MIN_CHARS`, non-empty assistant reply). Every
> other turn still writes a `message` event with the trio left `NULL` — semantically
> correct (no eval call was made, so there is no `generation_id` to record), but in the
> audit indistinguishable from data loss. To keep every `NULL` join key explainable at zero
> extra LLM cost, the affinity event `context` now carries an `eval_skip_reason` marker
> whenever the trio is `NULL`: `short_user_msg`, `empty_assistant`, `image_reply` (forward-
> looking — image variants degrade to `ReplyText` today), `proactive`,
> `no_persona_or_affinity`, `eval_error`, `eval_timeout`, or `eval_no_generation_id` (a
> salvaged-garble response that returned `Ok` but carried no `generation_id`). No backfill;
> `context` is free-form jsonb, so no migration or API change. `eval_timeout` /
> `eval_no_generation_id` are the only "possibly billed but unrecorded" cases and are now
> greppable.

### 3c. Insights (`companion_insights_events`)

After each `execute` in `extract_facts` (@ ~L578-618) and `extract_structured_insights`
(@ ~L643-693), capture the `ChatResponse` trio, compute `status`, and write one
`companion_insights_events` row via a new store method (e.g.
`InsightEventRepo::record(&self, ev: InsightEventInsert)` in
`crates/eros-engine-store/src/`, or a method on the existing `InsightRepo`):

- **`facts` row:** `stage='facts'`, `payload = <the facts JSON array>`,
  `status = ok | empty | parse_error`.
- **`structured` row:** `stage='structured'`, `payload = <the returned insight delta
  object>`, `status` likewise.

`run_id` / `user_id` / `session_id` / `message_id` (the produced assistant message) come
from the `extract_insights` call-site context. The `structured` call only runs when the
`facts` stage produced facts; a run can therefore legitimately write **one** row (facts,
empty) or **two** rows (facts + structured).

### 3d. Status mapping

| Call outcome | `status` | `payload` |
|---|---|---|
| parsed, non-empty | `ok` | the parsed facts[] / delta object |
| parsed, empty (`{"facts":[]}` / `{}`) | `empty` | the empty array / object |
| response did not parse | `parse_error` | `NULL` |
| no response (network / 4xx) | — (logged only, no row) | — |

---

## 4. Error handling — fail-open

Writing a `companion_insights_events` row is **best-effort**: an insert failure must
**never** break the reply or the extraction path — log a `tracing::warn!` and continue,
matching the existing post-process fail-open posture (extraction is already wrapped so a
failure degrades silently). The affinity audit columns ride the existing affinity insert,
so they add no new failure point.

---

## 5. Testing / verification

- **Migration** applies cleanly; `companion_insights_events` exists with all columns +
  indexes; `companion_affinity_events` has the three new columns.
- **Store unit tests** (`#[sqlx::test]`): insert + read back a `companion_insights_events`
  row for each `(stage, status)` combination, asserting `run_id` ties two rows; an
  affinity persist test asserts the trio is stored on the affinity event row (extend an
  existing affinity test or add one).
- **Pipeline test** (mocked OpenRouter via wiremock): a normal `extract_insights` run
  writes **2** rows (`facts` + `structured`) sharing one `run_id`, each with the served
  `model` / `usage` / `generation_id`; an empty-facts run writes **1** row
  (`stage='facts', status='empty'`) and no `structured` row.
- **Gate:** `cargo fmt` / `clippy --workspace -D warnings` / `test --workspace`
  (DB tests via `.test-env`). `openapi.json` unchanged (no DTO/route change — write-only
  audit, no read surface).

---

## 6. Files touched

- `crates/eros-engine-store/migrations/0025_insight_events_and_audit_cols.sql` — new
  table + the affinity ALTER.
- `crates/eros-engine-store/src/affinity.rs` — thread + bind the audit trio in
  `persist_with_event`.
- `crates/eros-engine-store/src/` (insights repo module) — new `InsightEventRepo` /
  insert method + its types.
- `crates/eros-engine-server/src/pipeline/post_process.rs` — generate `run_id`; capture
  the `ChatResponse` trio in `extract_facts` / `extract_structured_insights` / affinity;
  write the insight event rows; thread context (`user_id` / `session_id` / `message_id`).
- Tests alongside the above.

No change to `companion_insights`, `human_insights`, the extraction prompts, or
`model_config` — all of that is Spec B2.
