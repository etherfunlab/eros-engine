# `engine.llm_generations` — one row per LLM generation — Design

- **Date:** 2026-08-24
- **Status:** Approved, not yet implemented
- **Type:** New parent table + write-path change at every LLM call site;
  then foreign keys, a backfill, and column drops on eight live tables
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.7.x, **two PRs and two separate releases**
  (§8 is a hard constraint, not a preference)
- **Depends on:** migrations through `0058`. Production is on `0054`, so
  `0055`–`0058` ship alongside Release A.

## 1. Problem

Every LLM call the engine makes gets billed, and the only handle on that bill
is the provider's `generation_id`. Today that handle is scattered across nine
columns on eight tables, each shaped by whatever the feature that wrote it
happened to need:

| column | table |
|---|---|
| `generation_id` | `chat_messages` |
| `f_generation_id` | `chat_messages` |
| `generation_id` | `companion_affinity_events` |
| `generation_id` | `companion_insights_events` |
| `generation_id` | `companion_decision_events` |
| `generation_id` | `chat_images_events` |
| `generation_id` | `chat_vision_events` |
| `generation_id` | `character_insights_events` |
| `generation_id` | `user_insights_events` |

Five call sites write no `generation_id` at all — `world_director`,
`world_stories_director`, `world_comment`, `world_reply`, `memory_extraction`.
Their spend leaves no trace in the database whatsoever.

So there is no answer to "what did this deployment spend, and on what" that
does not begin with exporting the provider's dashboard. There is no table to
`GROUP BY task`, no table to join a cost onto a session, and no way to notice
that a task is being called more often than it should.

**One table, one row per generation, every call site writing to it.**

## 2. What counts as a generation

A generation is **one billable provider response that carried a
`generation_id`**. That is the unit the provider bills and the unit its log
exposes, so it is the unit this table keys on.

Consequences, each deliberate:

- **Failed attempts are not rows.** A hop that timed out, was refused, or died
  in transport has no `generation_id` (`OpenRouterClient::execute` returns
  `generation_id: None` on those paths). `llm_attempts` / `gateway_errors`
  (migration 0050) already hold that story and keep it.
- **A generation may own several child rows.** One streamed reply can persist
  as multiple `chat_messages` rows via `continues_from_message_id`. The table
  is the parent of a one-to-many, and the write is idempotent (§4).
- **Filter calls are rows.** `chat_input_filter` and `chat_output_filter` cost
  money, so they get rows like anything else. `chat_messages.f_generation_id`
  is the one pointer that does **not** get a foreign key (§7) — a column
  constraint is a separate question from whether the row exists.
- **Voyage embeddings are not rows.** Different provider, no OpenRouter
  `generation_id`, different billing line.
- **Image generation is not a row.** Since v0.7.1 the engine emits
  `image_request` and the downstream draws. What `chat_images_events` records
  is the *composer* call — a text completion — and that is a row.

## 3. Schema — migration `0059_llm_generations.sql` (Release A)

```sql
CREATE TABLE engine.llm_generations (
    generation_id TEXT PRIMARY KEY,
    session_id    UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    task          TEXT NOT NULL,
    model         TEXT,
    usage         JSONB,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_llm_generations_session
    ON engine.llm_generations (session_id);
CREATE INDEX idx_llm_generations_task_created
    ON engine.llm_generations (task, created_at DESC);
```

Plus the Supabase lockdown block (`REVOKE` guarded by `pg_roles` existence,
then `ENABLE ROW LEVEL SECURITY`), copied from `0045`.

**`generation_id TEXT PRIMARY KEY`.** The provider's own opaque handle, stored
verbatim. It is the join key to the provider's log, so a surrogate id would add
a column and remove nothing.

**`session_id` nullable ⇒ `ON DELETE SET NULL`.** Several tasks legitimately
have no session: the standalone compose endpoint, the world and story sweepers,
`world_comment` / `world_reply`. And an audit row must outlive what it points
at — the cost is still reconcilable after the conversation is deleted.

**`task NOT NULL`, no `CHECK`.** A row that cannot say which task it belongs to
answers none of the questions this table exists for. But the vocabulary is
`[tasks.*]` config, which a deployer may extend: a `CHECK` would turn adding a
config section into a runtime insert failure — a live turn dying because the
audit table did not recognise a legal task name.

**`model` and `usage` nullable.** Upstream occasionally omits them. The row is
still a true record of a billable call, which is the test for `NOT NULL` — not
"does the writer usually have it".

**No `user_id`.** `engine.*` does not take new columns pointing at an external
identity system. `session_id → chat_sessions.user_id` answers the attribution
question for rows that have a session, and rows that do not have one are
deployment-level work with no user behind them.

**Two indexes.** `(session_id)` because without it every
`chat_sessions` delete seq-scans this table for the `ON DELETE SET NULL`.
`(task, created_at DESC)` because "spend per task over a window" is the query
this table is for. No bare `(created_at DESC)`: the composite serves the
task-scoped form, and a global time scan over an audit table is not a hot path.

## 4. Write path (Release A)

### 4.1 The helper already exists

`pipeline::log_openrouter_usage(task, session_id, &resp)` is already called
from **19 of the 23** LLM call sites, and it already carries exactly the five
facts this table stores. The change is to make that function persist as well as
trace:

```rust
// crates/eros-engine-server/src/pipeline/mod.rs
pub(super) struct GenerationRecord<'a> {
    pub task: &'a str,
    pub session_id: Option<Uuid>,
    pub generation_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub usage: Option<&'a serde_json::Value>,
}

pub(super) async fn record_generation(
    pool: &PgPool,
    rec: GenerationRecord<'_>,
) -> Option<String>
```

Renamed, because it now records a fact to two sinks rather than emitting a log
line. The existing `tracing::info!` stays inside it verbatim — the structured
log line is what operators grep today and nothing about it changes.

**Five loose fields, not `&ChatResponse`.** Four streaming arms never own a
`ChatResponse` — they hold `last_gen_id` / `model_id` / `usage_full` as
separate locals — and two of them (`stream.rs` product-QA, `persona.rs`
compose) today synthesise a fake `ChatResponse` with an empty `reply` purely to
satisfy the logger's parameter. Taking the fields directly deletes those
synthetic values instead of adding two more. A named-field struct rather than
five positional `Option`s, matching `DecisionEventInsert` and its siblings.

Backed by `LlmGenerationRepo::record` in a new
`crates/eros-engine-store/src/generation.rs`, issuing:

```sql
INSERT INTO engine.llm_generations
    (generation_id, session_id, task, model, usage)
VALUES ($1, $2, $3, $4, $5)
ON CONFLICT (generation_id) DO NOTHING
```

`ON CONFLICT DO NOTHING` is required, not defensive: a continued reply calls
the child write once per `chat_messages` row for a single generation.

`created_at` is the row default rather than a parameter. It is the moment the
engine recorded the generation, within milliseconds of the response; taking it
from the caller would invite each call site to pass a subtly different clock.

### 4.2 The return value is the contract

Call sites stop reading `resp.generation_id` and use what `record_generation`
returns:

```rust
let gen = record_generation(&state.pool, GenerationRecord {
    task: TASK,
    session_id: Some(session_id),
    generation_id: resp.generation_id.as_deref(),
    model: resp.model.as_deref(),
    usage: resp.usage.as_ref(),
}).await;
// ... write the child row with `gen.as_deref()`, not `resp.generation_id`
```

`Some(id)` — the parent row is committed, the child may reference it.
`None` — either upstream gave no id, or the parent write failed (§6). Either
way the child must store `NULL`.

In the four streaming arms the returned value is **rebound onto `last_gen_id`**
at the point the stream loop ends, so every persist site downstream in that arm
picks up the degraded value with no further edit:

```rust
last_gen_id = record_generation(&state.pool, GenerationRecord { .. }).await;
```

This is what makes the foreign keys in Release B safe by construction: there is
no path that yields a `generation_id` to a child writer without the parent row
already existing.

### 4.3 Call sites

Nineteen sites need only the new parameter and `.await`. Four need a call that
does not exist yet, one needs its existing call moved, and five need a real
`session_id`:

| task | site | change |
|---|---|---|
| `chat_companion` | `stream.rs` live arm + filtered arm | **new call** — rebind `last_gen_id` where the stream loop ends, covering all of that arm's persist sites at once |
| `chat_voice` | `voice.rs` stream arm | **new call**, same rebind |
| `chat_image_edit_compose` | `image_edit.rs`, both terminal paths | **new call** — the exhausted path was billed too |
| `chat_product_qa` | `stream.rs` product-QA arm | existing call **moves earlier**: today it runs after the persist, which would leave the child pointing at an uncommitted parent |
| `chat_output_filter`, `chat_input_filter`, `pde_decision`, `chat_vision`, `chat_image_prompt_compose` (chat path) | `stream.rs` | pass the **real `session_id`** instead of today's `None` |

That last row is a fix, not a rename. Those sites pass `None` because a log
line did not need the session; a table that cannot attribute a chat-turn cost
to its conversation is worth much less.

Full task vocabulary after this PR: `chat_companion`, `chat_voice`,
`chat_product_qa`, `chat_input_filter`, `chat_output_filter`, `chat_vision`,
`chat_image_prompt_compose`, `chat_image_edit_compose`, `pde_decision`,
`affinity_evaluation`, `insight_extraction`, `insight_structuring`,
`character_insight_extraction`, `character_insight_structuring`,
`user_insight_extraction`, `user_insight_structuring`, `memory_extraction`,
`world_director`, `world_stories_director`, `world_comment`, `world_reply`.

**Coverage is complete in Release A**, including the five tasks that write no
child row today. Because the helper is shared, they cost nothing extra — for
them, `llm_generations` becomes their only record in the database.

### 4.4 Why not push the write into the store repos

`AffinityEventRepo::record` and its siblings do not know the task name, and
`chat_messages` reaches the database through several insert / upsert / update
paths that would each have to write the parent separately. The call site is
the only place that holds `(task, session_id, response)` together exactly once.

No trait, no middleware, no wrapper type: one function, twenty-three call
sites, three lines each.

## 5. Reading it

Cost per task for a window:

```sql
SELECT task,
       count(*)                                  AS calls,
       sum((usage->>'cost')::numeric)            AS cost
FROM engine.llm_generations
WHERE created_at >= now() - interval '7 days'
GROUP BY task ORDER BY cost DESC;
```

`usage` is stored **unfiltered**, including `cost`. `OPENROUTER_USAGE_HIDDEN_KEYS`
strips keys on the way out to clients only; the database has always kept the
whole object and continues to.

## 6. Failure semantics — fail-open, and what it costs

If the parent insert fails, the helper logs at `warn!` and returns `None`. The
child row is written with `generation_id = NULL` and the turn proceeds. A chat
reply is never lost to an audit write.

**The cost of that choice, stated plainly.** After Release B drops the
redundant `model` / `usage` columns (§7), a degraded write means that call's
model, token counts, and cost are gone from the database entirely. The tracing
line still names them, and the provider's log still holds them, but nothing in
Postgres does. This is the accepted trade: a dropped audit row is recoverable
from two other places, a dropped user reply is not.

## 7. Release B — migration `0060_llm_generation_fks.sql`

One file, one transaction, in this order.

### 7.1 Backfill

Roughly 55,000 historical rows across the eight tables (production, measured).
Each source contributes `DISTINCT ON (generation_id)` with a literal `task`:

| source | `task` |
|---|---|
| `chat_messages` `channel IS NULL` | `chat_companion` |
| `chat_messages` `channel = 'voice'` | `chat_voice` |
| `chat_messages` `channel = 'product_qa'` | `chat_product_qa` |
| `chat_messages.f_generation_id` | `chat_output_filter` |
| `companion_affinity_events` | `affinity_evaluation` |
| `companion_insights_events` `stage = 'facts'` / `'structured'` | `insight_extraction` / `insight_structuring` |
| `companion_decision_events` | `pde_decision` |
| `chat_images_events` `source = 'image_edit'` | `chat_image_edit_compose` |
| `chat_images_events` other sources | `chat_image_prompt_compose` |
| `chat_vision_events` | `chat_vision` |
| `character_insights_events` `stage` | `character_insight_extraction` / `_structuring` |
| `user_insights_events` `stage` | `user_insight_extraction` / `_structuring` |

Timestamps come from `sent_at` (`chat_messages`) or `created_at` (all others),
so backfilled rows keep their real position in time.

**Every `session_id` goes through `LEFT JOIN engine.chat_sessions` and lands as
`NULL` when it does not resolve.** Migration 0058 recorded that
`chat_images_events`, `companion_insights_events` and `companion_decision_events`
carry dangling session ids and deliberately left them dangling. Copying one into
`llm_generations.session_id` would violate this table's own — validated —
foreign key and abort the migration, taking the engine's boot with it.

Every insert carries `ON CONFLICT (generation_id) DO NOTHING`. Where the same
id appears in two source tables the first writer wins; the ordering above is
the tiebreak and no such collision is expected.

### 7.2 Indexes, then constraints

An index on each of the eight `generation_id` columns — none has one today, and
an unindexed child column makes the parent's `ON DELETE` seq-scan it.

Then eight foreign keys to `engine.llm_generations(generation_id)`, each
`ON DELETE SET NULL` (nullable reference column, audit trail outlives its
parent).

`chat_messages.f_generation_id` gets **no** foreign key. The filter generation
is in the parent table like any other, but the column stays unconstrained: it
is written by a separate `UPDATE` path (`mark_filtered`) that would need its own
degrade branch, and production has exactly **one** non-null value in it. The
constraint would buy nothing and add a second failure mode to the filter path.

### 7.3 Validated, not `NOT VALID`

These constraints are added **validated**, reversing migration 0058's blanket
rule. The difference is where the orphans could come from:

- 0058 constrained columns whose parents (`chat_sessions`, `chat_messages`,
  `persona_instances`) are deleted by forces outside the migration — a user
  deleted between the orphan check and the deploy is enough to fail the scan,
  and a failed release command means the engine does not boot with no rollback
  path.
- Here the parent rows are produced by the same transaction, from the very
  child tables being constrained. `ADD FOREIGN KEY` takes `SHARE ROW EXCLUSIVE`
  on both sides, so nothing writes a new child row between the backfill and the
  scan. And the deploy order (§8) guarantees the code running during the
  migration is Release A, which already writes parents. There is no source of
  an orphan.

55,000 rows scan in milliseconds. Taking the retrospective guarantee here is
free; skipping it would leave a permanent "someday" on the table.

### 7.4 Drop the redundant columns

`model` and `usage` come off all eight child tables. The same generation's
model and usage now live in exactly one place, reached by a join on
`generation_id`.

`chat_messages.filter_model` goes too: it is the model of the
`f_generation_id` generation, which is in the parent table.

Verified before writing this spec: `eros-engine-web` reads none of these
columns (`SELECT` on `engine.chat_messages` was revoked from `service_role` in
its migration `018`, and no web RPC references `model` or `usage` on any
`engine.*` table), and no engine read path outside test assertions selects
them. The work is deleting `.bind()` calls and rewriting the tests that assert
on them.

**Known property, not a Release-B surprise: `llm_generations.model` is
heterogeneous across tasks.** The two `chat_companion` streaming arms
(`stream.rs`'s live and filtered arms) record `model: Some(model_id.as_str())`
— the *requested* config slug, which may carry an `@provider` suffix — while
every non-streaming call site records `resp.model`, the model the provider
actually served. This already matches what `AssistantInsert.model` stores
today, so nothing regresses, but once the child `model` columns are dropped,
`llm_generations.model` is the only copy left and a reader joining across
tasks needs to know the two are not the same kind of value.

### 7.5 Documentation

`docs/llm-audit.md` and `docs/llm-audit.zh.md` describe the current split
("background paths emit usage only as tracing fields") and must be rewritten
around the new table. `docs/architecture.md` gains the table. Scan
`docs/api-reference.md` for any response field sourced from a dropped column.

## 8. Deploy order is a hard constraint

**Release A must be fully rolled out before Release B's migration runs.**

`infra/engine/fly.toml` sets `release_command = "migrate"`, which Fly runs
**before** traffic moves to the new machines. The moment Release B's foreign
keys exist, the machines still serving traffic are running Release A — or, if
the two were merged into one release, the *previous* build, which writes
`generation_id` into child rows without ever writing a parent. Every such
insert would violate the constraint, and for `chat_messages` that is a user's
reply failing to persist.

Merging the two PRs into one release breaks production. They are separate
releases with a completed rollout in between, and PR 2 does not open until
Release A is live.

## 9. Testing

**Store (`generation.rs`)**
- Insert, then insert the same `generation_id` again — one row, first write wins.
- `session_id = NULL` accepted.
- Deleting the session sets `session_id` to `NULL` and keeps the row.

**Helper**
- Returns the id on success; returns `None` and leaves the child column `NULL`
  when the insert fails.
- Returns `None` when `resp.generation_id` is `None`, without touching the DB.
- The `tracing` line is unchanged (existing log assertions keep passing).

**Call sites** — one test per task asserting a `llm_generations` row with the
right `task` and a `session_id` where the site has one. The five previously
unaudited tasks get their first database-level assertion here.

**Migration 0060** — a sqlx test that migrates to `0059`, inserts child rows
including one with a dangling `session_id`, runs `0060`, and asserts: parent
rows exist with the right `task`, the dangling one landed as `NULL`, the
constraints are `convalidated`, and the dropped columns are gone.

## 10. Out of scope

- Failed attempts without a `generation_id` — `llm_attempts` / `gateway_errors`
  already own that.
- Voyage embedding calls.
- Image-provider generations (the engine emits `image_request`; the downstream
  draws and bills).
- Any endpoint exposing this table. It is an operator table read through
  `supabase db query`; an API for it is a separate proposal.
