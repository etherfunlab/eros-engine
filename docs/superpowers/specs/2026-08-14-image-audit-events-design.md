# Image-composer and vision audit events + voice `relationship_scope` removal

**Date:** 2026-08-14
**Status:** approved
**Target release:** v1.2.1
**Scope:** `eros-engine-store` (new `image_events` module, migrations 0045/0046),
`eros-engine-server` (pipeline/stream, pipeline/voice, routes/voice,
routes/persona), `eros-engine-core` (scope), docs, openapi

Two independent changes ship together because both land in the same release and
neither is large enough to justify its own branch.

---

## Part 1 — Voice `relationship_scope` removal

The deprecation was announced twice: spec `2026-08-12-voice-affinity-scope-design.md`
§"Next release", and the v1.2.0 release note ("Voice endpoint `relationship_scope`
split — deferred to v1.2.1"). This is that removal. No new design decisions.

### Request contract

`VoiceTurnRequest.relationship_scope` is deleted. `eros_engine_core::scope::RelationshipScope`
is deleted with it — the voice route was its only consumer.

Resolution drops to two levels:

1. `affinity_scope` present → `AffinityScopeDto::resolve()`.
2. else → `AffinityScope::bond()`.

An unknown-value `relationship_scope` no longer 422s: with the field gone, serde
ignores it like any other unknown key. This is the intended end state — a
deployer still sending the old field silently gets the `bond` default, which is
what the field's most common value (`both` → now `bond`) already migrated to in
v1.2.0.

### Audit metadata

The assistant row's `metadata.relationship_scope` (the legacy vocabulary
`both / bond / chemistry / none`) is replaced by resolved **`affinity_scope`** —
the 6-bool object, byte-identical in shape to what the chat stream writes at
`pipeline/stream.rs`. `legacy_scope_label()` is deleted.

`metadata.memory_scope` on the assistant row is unchanged. The user row is
untouched: `affinity_scope_raw` / `memory_scope_raw` are already correct.

### Tests

Deleted (the field they exercise no longer exists):
`voice_422_when_relationship_scope_invalid`,
`voice_422_when_relationship_scope_uses_new_value`,
`voice_422_when_overridden_relationship_scope_is_garbage`,
`affinity_scope_wins_over_relationship_scope`,
`legacy_relationship_scope_alone_is_byte_compatible`.

Adapted: every assertion reading `metadata->>'relationship_scope'` reads
`metadata->'affinity_scope'` and checks the 6-bool object instead of a label.

New: an unknown `relationship_scope` in the body is ignored (200, `bond`
default) — locks in that the removal did not leave a 422 behind.

---

## Part 2 — `engine.chat_images_events`

### Problem

The image composer's inputs are not recorded anywhere. `metadata.image` on the
assistant row stores the composer's *subject* plus the audit trio
`compose_variant` / `compose_model` / `compose_generation_id`, and deliberately
omits the composed wire prompt. So today:

- **A failed compose leaves no trace in the database.** The absence of the
  `compose_*` keys conflates three causes — task not configured, chain
  exhausted, or an aborted turn — and only a `warn!` line separates them.
- **The context the composer saw is gone.** `compose_user_payload()`'s five
  slots (`[人物外观] [最近场景] [对方最新消息] [风格] [画幅]`) are assembled,
  sent, and discarded. Nothing can answer "why did it draw *that*".
- **The composed wire prompt is not in the engine at all.** It was left out
  back when the seed lived on the PDE event row and the composer was optional;
  since composition became mandatory (#212) nothing replaced that slot. Our own
  downstream stores it, but no other consumer is obliged to — and what
  downstream stores is only the *result*: the composer call itself (which model
  wrote it, under which variant, after how many failed candidates) is lost.

With an audit table in place, the evidence should be complete.

### Schema (migration 0045)

```sql
CREATE TABLE engine.chat_images_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source          TEXT NOT NULL CHECK (source IN (
                        'chat_reply_text_image','chat_reply_image',
                        'compose_endpoint','compose_endpoint_stream')),
    user_id         UUID NOT NULL,
    instance_id     UUID,
    session_id      UUID,
    status          TEXT NOT NULL CHECK (status IN ('ok','exhausted','not_configured')),
    inputs          JSONB NOT NULL,
    subject         TEXT,
    caption         TEXT,
    composed_prompt TEXT,
    variant         TEXT,
    model           TEXT,
    usage           JSONB,
    generation_id   TEXT,
    attempts        SMALLINT NOT NULL,
    last_failure    TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_chat_images_events_user_time
    ON engine.chat_images_events (user_id, created_at DESC);
CREATE INDEX idx_chat_images_events_session_time
    ON engine.chat_images_events (session_id, created_at DESC);
```

Plus the Supabase lockdown block copied from migration 0028: `pg_roles`-guarded
`REVOKE ALL FROM anon / authenticated`, then unconditional
`ENABLE ROW LEVEL SECURITY` with no policy.

**No foreign keys.** Append-only telemetry; a row may outlive or precede
anything it refers to (same rule as `companion_decision_events`).

Column notes:

- **`inputs`** — the five composer slots, structured, never concatenated:
  `{appearance, recent_scene, latest_user_msg, style, aspect_ratio}`. Same
  principle as `companion_decision_events.inputs` (migration 0044): engine-supplied
  input, kept in its own column so it can never be confused with model output.
  Empty slots are stored as the empty string, not as the `（无）` placeholder the
  prompt renders — the placeholder is a rendering detail.
- **`subject`** — the composer's own `prompt` field. NULL unless `status = 'ok'`.
- **`caption`** — NULL when the composer produced none (including the
  non-JSON migration fallback, where the whole reply becomes `subject`).
- **`composed_prompt`** — the assembled wire string from
  `compose_image_prompt(style, persona, subject)`: style preset + persona
  appearance + subject, i.e. exactly what the downstream consumer is handed.
  **Stored on every row that produced one, including `exhausted` and
  `not_configured`** — those chat turns still ship a picture, drawn from the
  portrait fallback, and this column is then the only record anywhere of what
  was drawn. NULL only on the standalone endpoint's `exhausted` row, which
  502s without assembling anything.

  The column earns its place on the table's own terms, not on the scarcity of
  the value elsewhere: **one row must be auditable on its own**, without
  joining `chat_messages` to reconstruct what was drawn, and the table must
  stay **independently extensible** — a future composer retry, or any caller
  with no message to join to, records a complete row without a schema change.
  (`chat_messages.metadata.image` is not that record: its `prompt` key holds
  the composer's short *subject*, never the assembled wire string.)
- **`variant`** — the resolved `prompt_variant` key. `"raw"` is an ordinary key,
  not a skip.
- **`usage`** — the **full unfiltered** usage block, `serde_json::to_value`'d,
  matching what the chat and voice assistant rows persist.
  `openrouter_usage_hidden_keys` filters the *wire* copy only and must not be
  applied here.
- **`attempts`** — how many models off `[primary, ...fallback]` were actually
  called. `0` for `not_configured`.
- **`last_failure`** — why the last attempt failed; NULL when `status = 'ok'`.
  Values: `model_error` / `timeout` / `empty` / `empty_prompt` /
  `stream_open_failed` / `stream_died_midway`. A free TEXT column, not a CHECK:
  this is a diagnostic label whose vocabulary will grow.

`status` is deliberately three values. `exhausted` means "no usable compose
result from this call" for every reason including a mid-stream death on the
standalone endpoint; `last_failure` carries the distinction. Keeping the CHECK
small keeps the reverse-lookup query trivial.

**No `message_id` column.** See linkage below.

### Linkage — assistant row points at the event, not the reverse

The composer runs *before* the assistant row exists: `build_delegated_image_prompt`
is `tokio::spawn`ed ahead of the chat call so its latency hides underneath, and
the assistant `message_id` only materialises at the join point from
`produced.last()`.

Rather than defer the audit write to the join point, the composer **writes its
event row first and returns the row id**, which is then stamped onto the
assistant row as `metadata.image.compose_event_id`. Reverse lookup goes
assistant row → `compose_event_id` → audit table.

This direction is the right one independent of the ordering problem: it keeps
the composer a self-contained auditable unit. Any future caller — the existing
standalone endpoint, a composer retry, a batch re-compose — writes rows without
needing a message to attach to, and the table never grows a column that half its
rows leave NULL.

It also widens coverage: because the row lands the moment the compose returns, a
turn whose image is never emitted (client disconnect or ghost fallback firing
`AbortOnDrop` after the call completed) still leaves the event behind. "Model
call paid for, no picture shipped" becomes visible. A compose aborted *mid-call*
writes no row, which is correct — that call never completed.

### Write points

One writer, called from inside the composer's own scope:

1. **`pipeline/stream.rs::build_delegated_image_prompt`** — after
   `compose_image_prompt()` has produced `composed_prompt`, before constructing
   `DelegatedImagePrompt`. `source` is `chat_reply_text_image` or
   `chat_reply_image` from `plan.action_type`. Covers all four call sites of
   this function (the image-only path, the speculative spawn, and the two
   join-point fallback re-invocations) with no per-site code. The function
   gains `user_id` / `session_id` params — both known at spawn time, unlike
   the assistant `message_id`; `instance_id` comes off the persona it already
   holds.
2. **`routes/persona.rs::compose_image`** (non-stream) — `source =
   'compose_endpoint'`, `session_id` NULL. `status` is only ever `ok` or
   `exhausted` here: a missing task 501s before any work.
3. **`routes/persona.rs::compose_stream`** — `source =
   'compose_endpoint_stream'`. Written when the terminal `done` frame is
   produced, or on chain exhaustion / mid-stream death. This mode's `done`
   frame already carries `composed_prompt` / `subject` / `caption` / `model` /
   `generation_id`, so the row is filled identically to the non-stream mode.

`DelegatedImagePrompt` gains `compose_event_id: Option<Uuid>`;
`build_delegated_image_marker` gains the matching parameter and writes the key
only when present.

To supply `attempts` / `last_failure` / `usage`, `run_image_prompt_compose`
returns a wrapper — outcome plus the chain-walk facts — instead of a bare
`Option<ComposeOutcome>`; `ComposeOutcome` gains `usage`.

### Fail-open

The INSERT is awaited and its error is `warn!`ed and dropped. A failed audit
write costs the event row and leaves `compose_event_id` absent; it never fails,
delays, or alters the turn. Same discipline as `companion_decision_events`.

---

## Part 3 — `engine.chat_vision_events`

### Problem

When a user sends the companion an image, `run_vision` walks the `chat_vision`
model chain and merges a successful describe into the user row's
`metadata.vision` / `vision_model` / `vision_generation_id`. Every failure mode —
transport error, timeout, empty reply, unparseable JSON, `content_filter`,
blank description, refusal-shaped description — produces only a `warn!` and an
absent metadata key. There is no way to ask "how often does the describe
succeed, and on which model".

### Schema (migration 0046)

```sql
CREATE TABLE engine.chat_vision_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       UUID NOT NULL,
    session_id    UUID NOT NULL,
    message_id    UUID NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('ok','exhausted','not_configured')),
    image_url     TEXT NOT NULL,
    vision        JSONB,
    model         TEXT,
    usage         JSONB,
    generation_id TEXT,
    attempts      SMALLINT NOT NULL,
    last_failure  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_chat_vision_events_user_time
    ON engine.chat_vision_events (user_id, created_at DESC);
CREATE INDEX idx_chat_vision_events_message
    ON engine.chat_vision_events (message_id);
```

Same lockdown block and same no-FK rule as 0045.

**This table keeps `message_id`, deliberately breaking symmetry with
`chat_images_events`.** The two are not structurally alike: vision runs *after*
the `role='user'` row exists, so `user_message_id` is already in hand, and there
is exactly one call site with no prospect of a standalone vision entry point.
Dropping a free, reliable join key to match a sibling table would force every
query to read the user row's metadata first — the wrong direction.

`vision` duplicates `metadata.vision` on success. That redundancy is the
accepted price of the table answering "how many describes ran, on what, at what
success rate" without joining `chat_messages` to establish a denominator.

`last_failure` values: `model_error` / `timeout` / `empty` / `unparseable` /
`content_filter` / `blank_description` / `refusal_pattern` — the last three are
`image_vision_invalidity`'s existing reason strings, reused verbatim.

The user's accompanying text is **not** stored: `message_id` points at
`chat_messages.content`, and real user text is not duplicated across tables.

### Write point

`pipeline/stream.rs`, in the existing `if user_msg.tips_amount_usd.is_none()`
image-describe block, which sits inside the `ActionType::ReplyText |
ReplyImage | ReplyTextImage` match arm, after the image-only reply's early
`return`. **One row per image-carrying, non-tipped turn that reaches this
arm** — including the `resolve_vision() == None` case, written as
`not_configured` with `attempts = 0`. That case is one of the three reasons
`metadata.vision` can be missing, so it has to be recordable.

Turns with no image write nothing: "carries an image" is the denominator, and a
text turn is not a missed describe. So do turns that never reach this arm at
all — `ActionType::Ghost`, `ActionType::ProductQa`, or an image-only reply
(`ReplyImage`, which returns before this block). The describe never ran on
those paths before this table existed either, and running one on a ghost turn
would waste a paid call, so this is not a behaviour change — it means an
operator computing "how often does the describe succeed" from this table must
read its denominator as **image-carrying turns that reached the text-reply
path**, not every image-carrying turn. (Deviation from the original plan: an
earlier draft of this section said "whenever the turn carries an image and is
not a tip" with no further qualification; the write site was never moved to
cover the other three paths, so this section now describes what shipped.)

`run_vision` returns a wrapper carrying `attempts` / `last_failure` alongside
the optional `VisionOutcome`; `VisionOutcome` gains `usage`. Fail-open exactly
as in Part 2.

---

## Common implementation notes

- New store module `crates/eros-engine-store/src/image_events.rs`, holding both
  repos and both insert structs. Shape copied from `decision.rs`: a plain
  `Insert` struct, a `Repo { pool }`, one `record()` returning the new row's
  `Uuid` (images) or `()` (vision).
- `crates/eros-engine-store/src/lib.rs` re-exports the two repos alongside the
  existing ones.
- `chat_messages.metadata.image` and `metadata.vision` are **unchanged** except
  for the single additive `compose_event_id` key. Both engine and downstream
  logic depend on their current shape.

## Tests

- `image_events.rs`: one `#[sqlx::test]` per repo, round-tripping an `ok` row
  and a failure row (`exhausted` with `attempts > 0` and a `last_failure`,
  NULL result columns), asserting the returned id matches the stored row.
- `pipeline/stream.rs`: a compose whose chain is exhausted still writes a row
  with `status = 'exhausted'`; the assistant row's `metadata.image` carries a
  `compose_event_id` matching a real row on the success path.
- `pipeline/stream.rs`: an image turn with no `[tasks.chat_vision]` writes one
  `not_configured` vision row; a describe that returns unparseable JSON on
  every chain model writes one `exhausted` row with
  `last_failure = 'unparseable'`.
- `routes/persona.rs`: the non-stream compose endpoint writes one
  `compose_endpoint` row carrying `composed_prompt` and the five `inputs` slots.

## Documentation

- `docs/llm-audit.md` / `.zh.md`: document both tables, their columns, the
  `metadata.image.compose_event_id` linkage, and the fail-open/no-FK nature of
  the writes.
- `docs/api-reference.md` / `.zh.md`: voice section drops `relationship_scope`
  entirely (request field, deprecation note, and the race-table row that names
  it) and documents the assistant row's resolved `affinity_scope`; compose
  endpoint gains a line noting its calls are audited.
- `openapi.json` regenerated.

## Not in scope

- Removing the now-partially-redundant `compose_variant` / `compose_model` /
  `compose_generation_id` keys from `metadata.image`. Downstream reads them.
- Per-attempt event rows. `attempts` + `last_failure` answer the chain
  questions at one row per call; per-attempt granularity can be added later
  without changing this schema's meaning.
- Any retention or pruning policy for either table.
- A `vision_event_id` stamp on the user row. The `message_id` column already
  gives that direction.
