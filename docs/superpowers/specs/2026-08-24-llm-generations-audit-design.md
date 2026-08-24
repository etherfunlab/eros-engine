# `engine.llm_generations` — one row per LLM generation — Design

- **Date:** 2026-08-24
- **Status:** Release A **deployed** — 1.6.1 reached production 2026-08-24
  18:08 CST, and §7.0's precondition now passes. 1.6.2 is released (§4.5, the
  timeout unification) and not yet deployed. B1 and B2 not started.
  "Released" and "deployed" are not interchangeable anywhere in this document:
  §7.3's argument for validated foreign keys rests on the code *serving
  traffic* already writing parent rows, and a git tag does not establish that.
- **Type:** New parent table + write-path change at every LLM call site;
  then foreign keys, a backfill, and column drops on eight live tables
- **Owner:** enriquephl (sole dev)
- **Target:** **Three releases, not two.** Release A shipped in 1.6.1;
  B1 and B2 follow, each after its predecessor's rollout completes. §8 is a
  hard constraint in both directions, not a preference — the split between
  B1 and B2 exists because the original two-release plan would have broken
  production (§7 preamble).
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
- **A generation may own no child row at all.** Two sources: the five sweeper
  tasks, which write nowhere else, and candidates the fallback chain answered
  and then passed over (§4.5). Both were billed, which is the whole test. An
  abandoned candidate is in fact the most valuable row in the table — it is
  spend that appears in no other engine record.
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

## 4. Write path

§4.1–§4.4 shipped in Release A. §4.5 is a gap Release A left; it is pure
application code, touches no schema, and is therefore not bound by §8 — it
rides in Release B's PR only because that is the next one open.

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

### 4.5 Abandoned candidates leave no row (Release B)

Three arms walk a fallback chain **while streaming**, and all three record
once, after the chain loop ends, with whatever the served attempt left
behind. Each keeps a working trio (`last_gen_id` / `last_usage` /
`served_model`) that only ever holds one candidate's metadata, because an
abandoned attempt must not leak its model or usage onto a later fallback's
reply. That is correct for the reply and is exactly what erases the abandoned
attempt's audit row: a candidate that streamed a response, was billed for it,
and was then passed over leaves no trace in `llm_generations`.

The two main chat arms do not have this gap: they call `record_generation`
inside the per-candidate loop, at the point that attempt's stream ends.

The evidence is lost two different ways, and each needs its own fix.

**Loss 1 — a superseded candidate is overwritten.** Line numbers are as of
`b8e03af`; verify before editing.

| file | where the previous candidate's evidence dies |
|---|---|
| `pipeline/voice.rs:714` | loop-top reset of the trio |
| `pipeline/stream.rs:4380` | loop-top reset of the trio |
| `routes/persona.rs:554` | next iteration overwrites `last_generation_id` |

`persona.rs` looks like four reset points (576, 595, 645, 681) but is not:
its per-candidate evidence lives in a `let` inside the loop, and the
chain-level `last_generation_id` those branches assign is simply overwritten
by the following iteration. One drain at the top of `for model_id in chain`
dominates all four.

**Fix — drain at the top of the loop.** Record whatever the previous
candidate left, then let the existing reset run. The return value is
discarded: no child row will ever point at an abandoned generation, and
`llm_generations` already holds rows with no children (the five sweeper
tasks). The first iteration finds `None` and does nothing.

```rust
// first statement of the candidate loop, before the existing resets
if let Some(id) = last_gen_id.take() {
    let usage_full = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
    let _ = record_generation(&state.pool, GenerationRecord {
        task: TASK,
        session_id: Some(session_id),   // `None` in persona.rs — standalone endpoint
        generation_id: Some(&id),
        model: served_model.as_deref(),
        usage: usage_full.as_ref(),
    })
    .await;
}
```

**Loss 2 — the *last* candidate's evidence dies on an early return.** Two
arms give up after the loop without reaching their `record_generation`, and
both are reachable holding a real `generation_id`:

| file | branch |
|---|---|
| `pipeline/voice.rs:839-850` | `if acc.is_empty()` — "a stream that sent only metadata (id/model) and then errored or ended without content" — emits an Error frame and returns |
| `pipeline/stream.rs:4536-4555` | chain exhausted **and** no canned phrase configured — emits an Error frame and returns |

A third site looks like the same bug and is not: `stream.rs:4528` clears the
trio on purpose so a canned fallback phrase is not attributed to a real
generation. That clearing must survive the fix.

**Fix — move the existing call above the exhausted block, do not add one.**
In both arms, hoist `usage_full` and the post-loop `record_generation` to
just above the `acc.is_empty()` / chain-exhausted handling. One move covers
the early return *and* the normal path, and it makes `4528` correct for free:
the generation is recorded because it was billed, and the subsequent clear
still keeps it off the canned-phrase row.

`persona.rs` needs no move — its exhausted arm already records at 726.

**The hoist is the part that gets missed.** `usage_full` is computed from
`last_usage` immediately before the record; moving one without the other
compiles and silently writes `usage: None`. This exact mistake was caught in
Release A's filtered arm and would not have been caught by the compiler.

Three drains and two moves, not one edit repeated at every reset. The two
fixes cannot double-record — the drain fires for the *previous* candidate, the
move for the last one — and `ON CONFLICT (generation_id) DO NOTHING` (§4.1)
makes it harmless if a future edit makes them overlap.

**Cost.** One extra write per abandoned candidate, on the fallback path
only. A turn whose first candidate answers pays nothing.

**Not "accumulate into a `Vec`, flush after the loop."** Both arms in Loss 2
`return` out of the chain, and a pending vector goes out of scope with the
stack — the vector reintroduces exactly the bug being fixed.

**Not backfillable.** Abandoned candidates from before this change left no
`generation_id` in any table, so §7.1 cannot recover them. The gap closes
forward only.

## 5. Reading it

Cost per task for a window. **Never sum `cost` without reporting how many
rows carried one** — the reason is below the query, and it is not a detail:

```sql
SELECT task,
       count(*)                                                       AS calls,
       count(*) FILTER (WHERE jsonb_typeof(usage->'cost') = 'number') AS priced,
       sum((usage->>'cost')::numeric)
         FILTER (WHERE jsonb_typeof(usage->'cost') = 'number')        AS cost_of_priced
FROM engine.llm_generations
WHERE created_at >= now() - interval '7 days'
GROUP BY task ORDER BY cost_of_priced DESC NULLS LAST;
```

**The filter tests the value's type, not the key's presence, and both halves
need it.** `usage ? 'cost'` is the obvious spelling and is wrong twice over:
a `"cost": null` counts as priced while contributing nothing to the sum, so
`priced` overstates coverage; and a non-numeric value — `"cost": "unknown"`,
or an object — makes `::numeric` **raise** and abort the whole query rather
than yielding `NULL`. `jsonb_typeof(usage->'cost') = 'number'` is false for
both, and for a missing key and a `NULL` `usage`, so no unparseable value ever
reaches the cast. `usage` is written verbatim from a provider's response; the
engine does not validate its shape and should not have to.

`usage` is stored **unfiltered** — whatever the provider sent, `cost` included
when the provider sent one. `OPENROUTER_USAGE_HIDDEN_KEYS` strips keys on the
way out to clients only; the database has always kept the whole object and
continues to.

**`cost` is a provider-dependent key, not a guaranteed one.** OpenRouter
returns it; Venice direct does not put it in `usage` at all. Measured on
production 22 minutes after 1.6.1 went live: 44 of 204 rows carried a `cost`
(21.6%), with 160 rows served by Venice direct.

`priced` is what makes the number readable: it is the count of rows carrying a
**numeric** cost, which is exactly the set the sum is over. A task served
entirely by Venice sums to `NULL`, which is honest. **The dangerous case is a
mixed task**: at
that measurement `chat_companion` ran on both `deepseek/deepseek-v4-flash-0731`
(OpenRouter, priced) and `gemma-4-uncensored@venice` (unpriced), so a bare
`sum(cost)` returned a plausible figure covering 18 of its 27 calls. A number
that looks like a total and is two thirds of one is worse than a `NULL`.

Deriving the missing cost from tokens and a price table is deliberately **not**
done: it would make the engine a second authority on what a call cost, and
that table drifts against the provider's real pricing with no signal. The
provider's own dashboard remains the authority for unpriced rows; this table's
job is to tell you which rows those are.

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

### 6.1 The exposure is the timeout budget, not the write count (Release B)

Release A put serial database writes on the chat turn's critical path, which
reads like a latency risk. The arithmetic says it is not:

- At most **four** audit writes are on the critical path — `chat_input_filter`,
  `pde_decision`, `chat_companion`, `chat_output_filter`. The post-process
  batch (`affinity_evaluation`, the insight stages, `memory_extraction`) runs
  in a task spawned after the reply has streamed, off the path entirely.
- A single-row `INSERT` against same-region Supabase costs 1–3 ms. Four of
  them is ~12 ms against a turn measured in seconds.
- The pool is 20 connections with a 5 s `acquire_timeout`
  (`eros-engine-store/src/pool.rs`). Production writes ~3,000 generations a
  day — 0.035/s. Saturating that pool takes four orders of magnitude of
  growth, not one.

What is actually mis-sized is `AUDIT_WRITE_TIMEOUT`, today 2 s
(`pipeline/mod.rs:41`). It bounds one write; it does not bound a turn. With
the database degraded rather than down, a turn can block **4 × 2 s = 8 s**
serially, all of it outside the LLM timeout budget, and what the user sees is
a reply that hangs.

**Set it to 500 ms.** A single-row insert's p99 is milliseconds, so 500 ms is
already a hundredfold margin, and the worst case falls to 2 s per turn. One
constant — no per-turn budget object, no shared deadline threaded through the
call sites.

**"One constant" is literal.** The repo carried two, same name, sibling
modules: `pipeline/mod.rs` at 2 s for `llm_generations`, and
`pipeline/stream.rs` at 3 s for `chat_images_events` / `chat_vision_events`,
the latter predating this work. Changing only one would have widened the
split to 0.5 s against 3 s. All three are the same operation — a single-row
fail-open INSERT into the same database — so `stream.rs`'s copy is deleted
and its two call sites read `super::AUDIT_WRITE_TIMEOUT`. The image and
vision writes come down from 3 s with the same argument, and the same
consequence when they trip: one telemetry row lost, never a turn.

The timed-out `warn!` also switches from `timeout_secs` to `timeout_ms` —
`Duration::as_secs()` on 500 ms logs `0`.

**Land it before Release A reaches production, if the ordering allows.** The
8 s exposure was introduced by 1.6.1, which is tagged but not deployed, so it
has never run against real traffic. Shipping the constant in whatever build
deploys next means it never does. This is a preference about sequencing, not
a constraint like §8 — the change is one line and correct in either order.

Two numbers to watch once Release A is deployed, both already emitted:

- the count of `llm_generations: audit write timed out` warnings — non-zero
  means the pool or the database is degrading, and it is the earliest signal
  either produces;
- pool `acquire_timeout` errors.

## 7. Release B — and why it is two releases, not one

Release B was specified as one migration doing four things: backfill, index,
constrain, and drop the now-redundant child columns. **The drop cannot ship
with the other three.** It is the same deploy-order hazard §8 describes,
pointing the other way, and the original §8 only covered one direction.

Every child INSERT in `eros-engine-store` names `model` and `usage`
explicitly — `character_insight.rs`, `affinity.rs`, `decision.rs`,
`image_events.rs` (twice), `insight.rs` (twice), `user_insight.rs` (twice).
`release_command = "migrate"` runs **before** traffic moves. Drop those columns
and, for the length of the rollout, every machine still serving traffic issues
`INSERT ... (model, usage, ...)` against columns that no longer exist. For
`chat_messages` that is a user's reply failing to persist — the exact outcome
§8 exists to prevent.

So the work splits by which side of the rollout each half is safe on:

| | contents | why its migration is safe when it runs |
|---|---|---|
| **B1** | code stops writing `model` / `usage` to the eight child tables (reads move to a join); migration `0060`: backfill, eight indexes, eight validated foreign keys | the build serving traffic is Release A, which already writes parent rows (§7.0) |
| **B2** | migration `0061`: `DROP` the redundant columns | the build serving traffic is B1, which no longer writes them |

The foreign keys ride in B1 rather than B2 because they are already safe there
— gating them behind the drop would delay the guarantee for no reason.

**B2 has its own precondition, symmetric to §7.0:** do not run `0061` until B1
is deployed. There is no equivalent one-query check, because the evidence is an
absence — the running code no longer writing two columns. Check
`fly image show -a eros-engine` shows B1's version on **every** machine, and
that no INSERT in `eros-engine-store` still names `model` or `usage` on those
tables.

### 7.0 Precondition — Release A must be *deployed*, and this is how you check

Do not run `0060` until this returns true:

```sql
SELECT count(*) > 0
FROM engine.llm_generations
WHERE created_at > now() - interval '1 hour';
```

A recent row is the only evidence that the build **currently serving traffic**
writes parent rows. Checking the tag proves nothing — 1.6.1 was tagged and
published while production still ran 1.6.0. Checking `fly image show` is
closer but still not enough: the image can be correct on machines that have
not taken traffic yet, and `release_command = "migrate"` runs before traffic
moves (§8).

If the query returns false, every foreign key in this migration is a live
failure mode: the running code writes `generation_id` into child rows with no
parent, and for `chat_messages` that is a user's reply failing to persist.

### 7.1 Backfill (B1)

**56,675** child references need a parent row, out of 57,142 total (production,
measured 2026-08-24 19:30 CST, shortly after 1.6.1 was deployed; the 467
difference is what 1.6.1 has already written). **A moving figure** — the same
tables take ~2,700 new generations a day, so the backfill grows by that much
for every day B1 waits. Re-measure before writing the migration; nothing in the
design depends on the number, but the runtime estimate in §7.3 does.

**All 57,142 references are distinct.** No `generation_id` appears in two source
tables, so the `ON CONFLICT` tiebreak below never actually fires — it stays as a
guard, not as a policy anyone has to reason about.

Each source contributes `DISTINCT ON (generation_id)` with a literal `task`:

| source | `task` |
|---|---|
| `chat_messages` `channel IS NULL` | `chat_companion` |
| `chat_messages` `channel = 'voice'` | `chat_voice` |
| `chat_messages` `channel = 'product_qa'` | `chat_product_qa` |
| `chat_messages.f_generation_id` | `chat_output_filter` |
| `companion_affinity_events` (see below — no `session_id`) | `affinity_evaluation` |
| `companion_insights_events` `stage = 'facts'` / `'structured'` | `insight_extraction` / `insight_structuring` |
| `companion_decision_events` | `pde_decision` |
| `chat_images_events` `source = 'image_edit'` | `chat_image_edit_compose` |
| `chat_images_events` other sources | `chat_image_prompt_compose` |
| `chat_vision_events` | `chat_vision` |
| `character_insights_events` `stage` | `character_insight_extraction` / `_structuring` |
| `user_insights_events` `stage` | `user_insight_extraction` / `_structuring` |

Timestamps come from `sent_at` (`chat_messages`) or `created_at` (all others),
so backfilled rows keep their real position in time.

**`companion_affinity_events` has no `session_id` column.** It carries
`affinity_id` and a nullable `user_message_id`, and nothing else that reaches a
session. Its only route is `user_message_id → chat_messages.session_id`, and
that route is mostly empty: of its 8,119 rows carrying a `generation_id`, **211
have a `user_message_id`** (2.6%), all of which resolve. So this source
contributes 211 rows with a session and 7,908 with `NULL`.

That is not a defect to fix in the backfill. `session_id` is nullable precisely
because some generations have no conversation to attribute to, and an affinity
evaluation predating the `user_message_id` column has no surviving link to one.
Inventing it — by joining on timestamps, or on the affinity row's own owner —
would put a guess in a column whose whole value is that it is not one.

**Every `session_id` goes through `LEFT JOIN engine.chat_sessions` and lands as
`NULL` when it does not resolve.** Migration 0058 recorded that
`chat_images_events`, `companion_insights_events` and `companion_decision_events`
carry dangling session ids and deliberately left them dangling. Copying one into
`llm_generations.session_id` would violate this table's own — validated —
foreign key and abort the migration, taking the engine's boot with it.

Measured 2026-08-24, this is load-bearing rather than defensive: **26** distinct
dangling session ids in `companion_insights_events`, **24** in
`companion_decision_events`, **2** in `chat_images_events`. Drop the `LEFT JOIN`
and the migration aborts.

Every insert carries `ON CONFLICT (generation_id) DO NOTHING`.

### 7.2 Indexes, then constraints (B1)

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

### 7.3 Validated, not `NOT VALID` (B1)

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

57,000 rows scan in milliseconds. Taking the retrospective guarantee here is
free; skipping it would leave a permanent "someday" on the table.

### 7.4 Drop the redundant columns — migration `0061` (B2)

`model` and `usage` come off all eight child tables. The same generation's
model and usage now live in exactly one place, reached by a join on
`generation_id`.

`chat_messages.filter_model` goes too: it is the model of the
`f_generation_id` generation, which is in the parent table.

Verified before writing this spec: `eros-engine-web` reads none of these
columns (`SELECT` on `engine.chat_messages` was revoked from `service_role` in
its migration `018`, and no web RPC references `model` or `usage` on any
`engine.*` table), and no engine read path outside test assertions selects
them.

**The code change belongs to B1; only the `DROP` is B2.** Every child INSERT
in `eros-engine-store` names both columns explicitly —
`character_insight.rs`, `affinity.rs`, `decision.rs`, `image_events.rs`
(twice), `insight.rs` (twice), `user_insight.rs` (twice). B1 removes those
column names and their `.bind()` calls and rewrites the tests that assert on
them; B2's migration then drops columns nothing writes. Doing both in one
release means the machines serving traffic during `release_command = "migrate"`
INSERT into columns that no longer exist (§7 preamble, §8).

Between the two releases the columns exist and hold `NULL` for anything B1
wrote. That is the intended intermediate state, not a defect to paper over:
the same generation's model and usage are already in the parent table, reached
by a join.

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

Release A already rewrote `docs/llm-audit.md` / `.zh.md` around the new table
and added it to `docs/architecture.md`, and 1.6.2 deleted the "Two known
limits" entry that §4.5 closed. What is left is column-specific and splits the
same way the code does:

- **B1** — those two docs describe the eight child `model` / `usage` columns as
  a later-release item. B1 makes them stop being written, so the wording moves
  from "will be dropped" to "no longer written; dropped in the next release,
  read through the join". Both languages.
- **B2** — the columns are gone; every reference to them becomes the join, and
  the "later-release item" framing disappears entirely.

Scan `docs/api-reference.md` in B1 for any response field sourced from one of
the dropped columns. Nothing is expected — §7.4 verified no read path outside
test assertions selects them — but the scan is cheap and the failure mode is a
documented API field that silently starts returning `NULL`.

### 7.6 Retention — none, and the numbers that would change that

**No retention policy.** The table grows without bound and that is the
decision, not an omission.

Baseline, measured in production on 2026-08-24 over the trailing 14 days:

| source | generations / day |
|---|---|
| `companion_insights_events` | 818 |
| `character_insights_events` | 498 |
| `chat_messages` | 478 |
| `companion_decision_events` | 461 |
| `companion_affinity_events` | 365 |
| `chat_images_events` | 89 |
| `chat_vision_events` | 1 |
| **eight child tables** | **~2,710** |

Release A also records the two filter tasks and the five sweepers, which
write no child row — call it **3,500–4,000 rows/day**. At ~500 bytes a row
including all three indexes, that is **~0.7 GB/year**. For comparison,
`chat_messages` is 22,179 rows in 29 MB. This table passes `chat_messages`
in **row count** within two months and will never come close to it in bytes
per row: it stores a 45-byte id and a ~110-byte usage object, not a
conversation.

0.7 GB/year is not a cost line on Supabase, and pruning now would throw away
the only reconciliation history the engine has ever had — the reading this
table was built to produce does not exist yet.

**Trigger — revisit when either holds:**

- the table exceeds **10 GB**, or
- the `GROUP BY task` window query in §5 exceeds **2 s**.

**If it fires, in this order:** roll up into a daily summary
(`day, task, calls, cost, prompt_tokens, completion_tokens`) *first*, then
delete detail rows behind the rollup. The rollup is what makes the deletion
acceptable, because deletion is not free:

> Every foreign key in §7.2 is `ON DELETE SET NULL`. Deleting a parent row
> **nulls `generation_id` on its child rows** — a `chat_messages` row that
> has carried its provider handle since it was written silently loses it.
> That is the real price of retention here, and it is why the rollup comes
> first: the cost curve survives, the per-generation handle does not.

The `ON DELETE` clause stays `SET NULL` regardless. `NO ACTION` would protect
the child handles, but it would also mean a retention job can only delete
parents that happen to have no children, and it contradicts the repo's own
rule that a nullable reference column takes `SET NULL`. A future retention
decision can `ALTER` the constraint; nothing here forecloses it.

## 8. Deploy order is a hard constraint, in both directions

`infra/engine/fly.toml` sets `release_command = "migrate"`, which Fly runs
**before** traffic moves to the new machines. For the length of every rollout,
therefore, **the previous build's code runs against the new build's schema.**
That single fact generates two separate constraints, and this design tripped
over one of each.

**A schema change that adds a requirement must ship AFTER the code that
satisfies it.** Release B's foreign keys require every child `generation_id` to
have a parent row. If they shipped with Release A's write path in one release,
the machines still serving traffic during the migration would be the build
*before* Release A — writing `generation_id` into child rows with no parent.
Every such insert violates the constraint, and for `chat_messages` that is a
user's reply failing to persist. Hence A, then B.

**A schema change that removes something must ship AFTER the code that stopped
using it.** B2 drops `model` and `usage` from the eight child tables, but every
child INSERT in `eros-engine-store` names those columns. If the drop shipped
with the code that stops naming them, the machines still serving traffic during
the migration would issue `INSERT ... (model, usage, ...)` against columns that
no longer exist — the same user-visible failure, from the opposite cause. Hence
B1, then B2.

The general rule, which is what to check against any future migration here:
**during a rollout the old code must be correct against the new schema.**
Additive-and-permissive changes satisfy it for free. Anything that constrains
or removes needs a release boundary, and which side it goes on depends on
whether the code has to start doing something or stop doing it.

Three releases, each with a completed rollout in between. No PR opens until its
predecessor is live — verified for B1 by §7.0's query, and for B2 by §7's
preamble.

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

**Abandoned candidates (§4.5)** — the two losses fail independently, so each
needs its own test. Assertions count rows in `llm_generations`; a test that
only checks the served path passes today and would keep passing with the fix
reverted.

*Loss 1, one test per arm* (voice, product-QA, compose): candidate 1 streams
a response carrying a `generation_id` and then fails; candidate 2 succeeds.
Assert **two** rows, and that the persisted child row points at candidate 2.

*Loss 2, one test per arm* (voice, product-QA): a single candidate streams
metadata — `generation_id`, `model`, `usage` — and then ends with no content,
so the arm takes its early-return branch. Assert **one** row carrying that id
**with a non-null `usage`**. The usage assertion is what catches a move that
left `usage_full` behind.

*The canned-phrase branch* (`stream.rs:4528`) gets its own test: chain
exhausted after a candidate streamed metadata, with a phrase configured.
Assert the abandoned generation has a row while the persisted message's
`generation_id` is `NULL`. The branch clears the trio precisely so the canned
phrase is not attributed to a real generation, and the move must not undo
that.

**Migration 0060 (B1)** — a sqlx test that migrates to `0059`, inserts child
rows across several source tables including one with a dangling `session_id`
and one `companion_affinity_events` row with a `NULL` `user_message_id`, runs
`0060`, and asserts: parent rows exist with the right `task`, the dangling
session landed as `NULL`, the affinity row without a `user_message_id` landed
as `NULL` while one with a resolvable id kept its session, and the constraints
report `convalidated`.

**Migration 0061 (B2)** — migrates through `0060`, runs `0061`, asserts the
dropped columns are gone. Kept separate from the `0060` test because they ship
in different releases and a combined test would pass on a tree that cannot be
deployed.

**The B1 write path** — for each of the eight child tables, insert through the
repo and assert the row's `model` and `usage` columns are `NULL` while the
parent row carries both. This is the assertion that makes B2's `DROP` safe, and
it is the one a reviewer is most likely to consider redundant: the columns
still exist in B1, so nothing visibly breaks without it.

## 10. Out of scope

- Failed attempts without a `generation_id` — `llm_attempts` / `gateway_errors`
  already own that.
- Voyage embedding calls.
- Image-provider generations (the engine emits `image_request`; the downstream
  draws and bills).
- Any endpoint exposing this table. It is an operator table read through
  `supabase db query`; an API for it is a separate proposal.
