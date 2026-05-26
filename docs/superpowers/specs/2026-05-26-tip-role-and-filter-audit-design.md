# eros-engine — Tip role (`gift_user`) + chat-reply filter audit columns (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.4.x` dev track (`0.4.3-dev`); one additive `engine.chat_messages` migration
**Audience**: anyone implementing issue #51 (distinct tip role + structured `tips_amount_usd` in BFF history) and persisting filter-layer audit fields previously declared in-memory-only by `2026-05-25-chat-output-filter-design.md` §2.6.

---

## 0. Background

Two engine-side gaps land in the same migration because they both extend
`engine.chat_messages` with optional metadata columns that are NULL on legacy
rows and on rows that never touch the relevant path.

**Gap A — issue #51 (tip user row).** The streaming chat path persists a
user-side tip marker via `upsert_user_message_idempotent` (`crates/eros-engine-store/src/chat.rs` ~L297) with `role='user'` and `content="(打赏 $X)"`. In BFF
history (`/bff/v1/comp/chat/{sid}/history`) the row is **indistinguishable from
a normal user message**: role + content look like a regular turn, and parsing
`(打赏 $X)` out of `content` is brittle and locale-dependent. The BFF DTO
already documents `gift_user` as a role value, and the table's `role` `CHECK`
constraint already accepts `gift_user` (`0001_chat.sql:18`), but the tip path
never writes it. `2026-05-26-tips-stream-reply-design.md` §2.3 explicitly
deferred the role flip ("Acceptable for v1; a distinct role is deferred to the
later gift-route keep/remove decision"). This spec performs that flip.

**Gap B — filter audit (supersedes 2026-05-25 §2.6).** The chat-reply output
filter (`2026-05-25-chat-output-filter-design.md`) ships filtered text to the
client and persists `content = filtered`. The original pre-filter text is
intentionally **in-memory only** and **not recoverable** — fed to
`post_process::run` when `timing = after_extract`, then dropped. Once the
feature ran on production traffic, operators want to inspect what the filter
rewrote, which model served the rewrite, and which trigger predicates fired.
The original-as-in-memory-only choice was tagged as a user-approved tradeoff in
that spec; this spec withdraws it.

The two gaps are independent in semantics (one for `role='gift_user'` user
rows, one for filtered `role='assistant'` rows), but additive on the same
table with disjoint columns. Bundling avoids a second migration round-trip
through release.

---

## 1. Goal / Non-goals

**Goal — Gap A:**
- Persist tip user rows with `role='gift_user'` (reusing the existing CHECK enum value, not introducing a new one).
- Persist `tips_amount_usd` structurally on the row (no string parsing).
- Expose `tips_amount_usd` in BFF history DTO so a client renders the tip as a system-style notice at the right point in the timeline.

**Goal — Gap B:**
- On filtered-success assistant rows, persist five audit columns: `pre_filter_content`, `filter_model`, `filter_triggers`, `f_client_msg_id`, `f_generation_id`.
- Make the original recoverable for ad-hoc inspection (DB query) without exposing it through BFF history.

**Non-goals:**
- No new `role` value (`user_send_tips` is **rejected**; `gift_user` is reused — see §3.1).
- No DTO surface for the filter audit columns (no BFF / no public API). A future admin/debug endpoint is **out of scope**; if it lands, it gets its own spec.
- No retroactive backfill. Legacy rows stay NULL; downstream readers MUST tolerate NULL.
- No change to filter timing semantics (`after_extract` / `before_extract`) or to `extracted_facts`.
- No widening of the `assistant_action_type` CHECK; no change to its column type.
- No change to chat-reply `content` semantics — the assistant row still stores the **filtered** text in `content` exactly as today.
- No change to client `(打赏 $X)` marker content (kept for FE backward-compat fallback rendering).

---

## 2. Schema migration

New file `crates/eros-engine-store/migrations/0019_chat_tip_marker_and_filter_audit.sql`:

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
--
-- chat_messages gains:
--   metadata           — open marker bag for user-side rows (today: tip amount)
--   pre_filter_content, filter_model, filter_triggers, f_client_msg_id,
--   f_generation_id   — assistant-side filter audit, written only when the
--                       chat-reply output filter actually rewrote the reply.
--
-- All six columns are nullable and default-safe; existing inserts that do not
-- mention them keep working. No backfill.

ALTER TABLE engine.chat_messages
    ADD COLUMN metadata             JSONB NULL,
    ADD COLUMN pre_filter_content   TEXT  NULL,
    ADD COLUMN filter_model         TEXT  NULL,
    ADD COLUMN filter_triggers      JSONB NULL,
    ADD COLUMN f_client_msg_id      TEXT  NULL,
    ADD COLUMN f_generation_id      TEXT  NULL;

-- Audit index for tip-amount aggregation. Partial: only rows that carry a tip.
CREATE INDEX chat_messages_tips_amount_idx
    ON engine.chat_messages ((metadata->>'tips_amount_usd'))
    WHERE metadata ? 'tips_amount_usd';

-- f_client_msg_id is engine-generated per filter LLM call (§4); enforce that
-- a single logical filter call writes at most one row per session.
CREATE UNIQUE INDEX chat_messages_f_client_msg_id_uidx
    ON engine.chat_messages (session_id, f_client_msg_id)
    WHERE f_client_msg_id IS NOT NULL;
```

The `role` `CHECK` is **not modified** — `gift_user` is already in the allowed
set. `assistant_action_type` is **not modified** — its CHECK and TEXT type stay
as-is.

---

## 3. Gap A — tip role + tips_amount_usd

### 3.1 Why reuse `gift_user`, not introduce `user_send_tips`

- The `role` `CHECK` constraint at `0001_chat.sql:18` already includes `gift_user`.
- The BFF DTO comment at `crates/eros-engine-server/src/routes/bff/companion.rs:35` already documents `gift_user` as a returnable role.
- Adding `user_send_tips` would require a second `CHECK` migration plus a DTO enum widening for no semantic gain over the existing slot.
- Naming objection ("gift suggests an item, tips don't") is overridden by the cost of a parallel role-token taxonomy; downstream renders the role + the structured `tips_amount_usd`, and the visual presentation is FE-driven.

### 3.2 `upsert_user_message_idempotent` signature change

Current (`chat.rs:297`):

```rust
pub async fn upsert_user_message_idempotent(
    &self,
    session_id: Uuid,
    content: &str,
    client_msg_id: &str,
) -> Result<UpsertUserOutcome, sqlx::Error>
```

New:

```rust
pub async fn upsert_user_message_idempotent(
    &self,
    session_id: Uuid,
    content: &str,
    client_msg_id: &str,
    role: &str,                            // "user" (default) | "gift_user"
    metadata: Option<&serde_json::Value>,  // tip path: Some({"tips_amount_usd": X})
) -> Result<UpsertUserOutcome, sqlx::Error>
```

Insert statement updates to bind `role` and `metadata`:

```sql
INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id, metadata)
VALUES ($1, $4, $2, $3, $5)
RETURNING id
```

The replay-lookup `SELECT` keeps the existing `role = 'user'` filter? **No**:
widen to `role IN ('user','gift_user')`. The idempotency key is `(session_id,
client_msg_id)` regardless of which role the row was inserted under, and the
streaming protocol treats the row as the canonical user turn for the
fallback/replay chain.

All non-tip call sites pass `"user", None`. The single tip call site (in
`crates/eros-engine-server/src/pipeline/stream.rs`, the
`PersistedUserMessage`-with-`tips_amount_usd` branch) passes `"gift_user",
Some(json!({"tips_amount_usd": amount}))`.

### 3.3 Stream wiring

The previously-shipped `2026-05-26-tips-stream-reply-design.md` already
threads `tips_amount_usd: Option<f64>` through `PersistedUserMessage` and
`Event::UserMessage`. This spec only changes the **persist call**:

```rust
let role = if user_msg.tips_amount_usd.is_some() { "gift_user" } else { "user" };
let meta = user_msg
    .tips_amount_usd
    .map(|a| serde_json::json!({ "tips_amount_usd": a }));
chat_repo
    .upsert_user_message_idempotent(
        session_id,
        &content,                       // "(打赏 $X)" marker stays as-is for FE fallback
        &client_msg_id,
        role,
        meta.as_ref(),
    )
    .await?;
```

`content` is **not** changed — keeping the `(打赏 $X)` marker preserves
backward-compat for any client that has not yet read this spec and still
renders by content-string parsing.

### 3.4 BFF history DTO

`crates/eros-engine-server/src/routes/bff/companion.rs` — `HistoryMessage`
gains:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub tips_amount_usd: Option<f64>,
```

The SELECT for BFF history `(metadata->>'tips_amount_usd')::float8` extracts
the value when present. Returned on rows with `role='gift_user'` that have the
field; omitted otherwise via `skip_serializing_if`.

`role` continues to serialise as the raw DB string, so frontends now see
`"gift_user"` for tip rows where they previously saw `"user"`. This is the
behaviour change clients act on.

### 3.5 `prompt_traits` on assistant rows' `metadata`

In addition to the user-side tip case, `engine.chat_messages.metadata` is also written
on **every** assistant row persisted by `drive_chat_burst` — both live mode and
filtered mode, regardless of whether the filter fired. Shape:

```json
{ "prompt_traits": ["nsfw_boost", "tsundere"] }
```

The array is the **kept** trait tags actually injected into the system prompt this
turn — the same set as the final frame's `prompt_injected`. Empty array when no
traits were injected (distinct from legacy rows with NULL metadata).

**Not** exposed via BFF history — audit-only. A future admin/debug endpoint can read
it if needed.

---

## 4. Gap B — filter audit columns

### 4.1 Where exactly the write happens

In `crates/eros-engine-server/src/pipeline/stream.rs`, in `drive_chat_burst`'s
**filtered-mode, per-attempt `models`-predicate passes, filter LLM call
succeeds** branch (the success arm of §2.5 step 3 first sub-branch in
`2026-05-25-chat-output-filter-design.md`). **Nowhere else.** Specifically:

| Code path | Writes audit columns? |
|---|---|
| live mode (filter disabled or turn-level predicate failed) | No — NULL |
| filtered mode, per-attempt `models` predicate failed | No — NULL |
| filtered mode, filter LLM **failure / timeout** (fail-open) | No — NULL |
| filtered mode, filter LLM **success**, this is what the client received | **Yes — all 5 columns set** |
| filtered mode, chat-attempt truncated and discarded silently | No row persisted at all |

The five columns travel together — they are either all set (filtered success)
or all NULL. Code MUST enforce this as a single `Option<FilterAudit>` write,
not five independent `Option`s.

### 4.2 Column contents

| Column | Source | Notes |
|---|---|---|
| `pre_filter_content` | `acc` (the accumulated original reply text before the filter rewrite) | The text previously dropped per `2026-05-25-chat-output-filter-design.md` §2.6. |
| `filter_model` | `ChatResponse.model` from the filter LLM `execute()` call (the model **actually served**, accounting for the filter's depth-capped fallback) | Distinct from the assistant row's `model` column, which is the **chat** model. |
| `filter_triggers` | JSONB capturing each predicate that fired, with the matched value | Format below. |
| `f_client_msg_id` | Engine-generated ULID, prefix `f_`, created **once per logical filter call** before invoking `execute()`; reused across the filter's internal fallback retries | Used as an idempotency / trace key by ops; unique per `(session_id, f_client_msg_id)`. |
| `f_generation_id` | `ChatResponse.generation_id` from the filter LLM call | OpenRouter generation id for the filter call. |

`filter_triggers` JSON shape — **only fired predicates appear**:

```json
{
  "random":  { "p": 0.30, "draw": 0.18 },
  "models":  "deepseek/deepseek-v4-flash",
  "traits":  ["nsfw_boost"]
}
```

- `random` present iff `trigger.random` configured **and** drew under `p`. `p` echoes the configured probability; `draw` echoes the per-turn draw.
- `models` present iff `trigger.models` configured **and** the chat model that produced the persisted reply is in the list; value = that model id.
- `traits` present iff `trigger.traits` configured **and** the predicate passed; value = the **subset of `any` actually present in `kept_traits`** for `when = present`, or `[]` for a passing `when = absent` predicate.
- The shape is deliberately additive — future predicates land as new top-level keys.

A turn that is filtered with no `trigger` block configured at all (empty trigger ⇒ always-filter, per `2026-05-25 §2.4`) writes `filter_triggers = {}`.

### 4.3 `should_filter` signature change

`OutputFilterTrigger::should_filter` currently returns `bool`. Change to:

```rust
pub fn should_filter(
    &self,
    model_id: &str,
    traits: &[PromptTrait],
    random_draw: Option<f64>,           // None when trigger.random is absent
) -> Option<TriggerHits>
```

Where:

```rust
pub struct TriggerHits {
    pub random: Option<RandomHit>,
    pub models: Option<String>,
    pub traits: Option<Vec<String>>,
}
pub struct RandomHit { pub p: f64, pub draw: f64 }
```

`None` means the filter does not fire (per-attempt). `Some(hits)` means it
fires; `hits` serialises to the `filter_triggers` JSONB above (drop fields that
are `None`).

Callers update from `if should_filter(...)` to `if let Some(hits) =
should_filter(...)`. `random_draw` is the per-turn draw (`rng.gen::<f64>()`),
sampled **once per turn** per `2026-05-25 §2.4`; the burst threads it into the
per-attempt call so the random outcome is stable across attempts of one turn.

### 4.4 Filter write site

Inside the filtered-success branch:

```rust
let filter_audit = FilterAudit {
    pre_filter_content: acc.clone(),         // original
    filter_model:       filter_resp.model.clone(),
    filter_triggers:    serde_json::to_value(&hits)?,
    f_client_msg_id:    filter_call_id.clone(),
    f_generation_id:    filter_resp.generation_id.clone(),
};
// passed into insert_assistant_batch / persist_assistant_row as an Option<FilterAudit>;
// None at every other call site.
```

`AssistantInsert` (or the equivalent row struct in `eros-engine-store`) gains a
single `filter_audit: Option<FilterAudit>` field that fans out into the five
bound parameters in the `INSERT`. The store layer is responsible for binding
NULL across all five when `None`.

### 4.5 Implications for `2026-05-25-chat-output-filter-design.md`

This spec **supersedes** the in-memory-only / not-recoverable wording in §2.6
of that document. Specifically:

- "The original on a filtered success is in-memory only" → original is now persisted on the same row as `pre_filter_content`.
- "Not stored, not recoverable" → reverse.
- `timing = after_extract` vs `before_extract` semantics for what `produced.full_text` feeds the extract pipeline are **unchanged** (still controlled by `timing`). Persisting the original to `pre_filter_content` is additive and orthogonal.

Add a banner note at the top of `2026-05-25-chat-output-filter-design.md` (do
not delete the existing wording; readers need the historical context):

> **Update (2026-05-26):** §2.6 "in-memory only / not recoverable" superseded by
> `2026-05-26-tip-role-and-filter-audit-design.md` §4. The original pre-filter
> text is now persisted on the assistant row as `pre_filter_content` when the
> filter rewrites the reply.

---

## 5. BFF history exposure

- `tips_amount_usd`: **exposed** on `HistoryMessage`, via `metadata->>` extract, with `skip_serializing_if = "Option::is_none"`. Tip rows include it; other rows omit the field entirely from JSON.
- `pre_filter_content`, `filter_model`, `filter_triggers`, `f_client_msg_id`, `f_generation_id`: **NOT exposed**. No DTO field is added. No SELECT pulls them. They are operator-side audit data, queryable via DB tooling. A future internal/admin endpoint can read them; that endpoint is out of scope for this spec.

`role` continues to serialise raw; tip rows now report `"gift_user"`.

---

## 6. Persistence, replay & extract

- `chat_messages.content` semantics unchanged. Assistant row holds filtered text exactly as today (or the chat reply on fail-open); user row holds `(打赏 $X)` marker on tip path.
- Replay path is unchanged for assistant rows — it never read the new five audit columns. For user rows, the `role` filter widens to `IN ('user','gift_user')` at the dedup lookup (§3.2); replay decisions key on `client_msg_id` regardless of role.
- `after_extract` / `before_extract` continue to control which text feeds `post_process::run`. The change is that the original is now also on disk; nothing in the extract pipeline reads `pre_filter_content`.

---

## 7. Testing

**Migration:**
- Apply on a DB with existing rows from 0001 + 0012: existing inserts still succeed (six columns NULL).
- Insert one row with `metadata = '{"tips_amount_usd": 20.0}'`: round-trips; `(metadata->>'tips_amount_usd')::float8` returns `20.0`.
- Insert two filtered rows with the same `(session_id, f_client_msg_id)`: second fails on unique index. Insert with `f_client_msg_id = NULL` on two rows: both succeed (partial index ignores NULL).

**`upsert_user_message_idempotent`:**
- `role="user", metadata=None` (existing behaviour) → row has `role='user'`, `metadata IS NULL`. All existing tests at `chat.rs:547+` keep passing with the updated call sites.
- `role="gift_user", metadata=Some({"tips_amount_usd": 20.0})` → row has `role='gift_user'`, metadata round-trips.
- Replay lookup: second call with same `(session_id, client_msg_id)` regardless of role → `Replay` outcome (the widened role filter sees both `user` and `gift_user` matches).

**`should_filter` (pure):**
- Empty trigger ⇒ returns `Some(TriggerHits { all None })` ⇒ serialises to `{}`.
- Each predicate alone, hit case ⇒ corresponding field populated.
- `traits` `when = absent` passing ⇒ `traits: Some(vec![])`.
- AND-of-many: any specified predicate failing ⇒ returns `None`.
- `random_draw = None` when `trigger.random` is absent ⇒ random gate is "no gate" (passes); when present, draw compared to `p`.

**Stream (wiremock for chat + filter models):**
- Filtered-success: client receives filtered delta; row persists `content = filtered`, all 5 audit columns set, `pre_filter_content == acc (original)`, `filter_model == ChatResponse.model`, `filter_triggers` matches the fired predicate set.
- Filtered + fail-open: client receives original delta; row persists `content = acc`, all 5 audit columns NULL.
- Live mode (filter disabled or turn-level predicate failed): all 5 audit columns NULL; byte-identical to today.
- Filtered-mode `models`-miss: row persists `content = acc`, all 5 audit columns NULL.
- Filter LLM's internal fallback served the second model: `filter_model` reflects the served model id; `f_generation_id` reflects that call's generation id; `f_client_msg_id` is the same id used across the chain.
- Two consecutive filtered turns in the same session yield distinct `f_client_msg_id` values.

**BFF history:**
- Tip row (`role='gift_user'`, `metadata.tips_amount_usd = 20.0`): response includes `"role": "gift_user"`, `"tips_amount_usd": 20.0`.
- Normal user row: response has `"role": "user"` and **no** `tips_amount_usd` field.
- Filtered assistant row: response has no `pre_filter_content` / `filter_*` / `f_*` fields (DTO never declared them).

---

## 8. Rollout

- Single PR into `dev`. The dev-track cut itself takes no version bump; the next release cut to `main` will bump the four `Cargo.toml`s + `openapi.json` + the README docker version as usual.
- The migration runs automatically on the next deploy; no manual step required.
- Frontend coordination: BFF response shape for tip rows changes (`role: "gift_user"` instead of `"user"`, new optional `tips_amount_usd`). Existing FE that filters on `role === "user"` will silently stop showing tip rows in the regular bubble lane — desired behaviour per issue #51 but flag it in the PR description so the FE team flips the rendering path in the same release window.
- `2026-05-25-chat-output-filter-design.md` gets the §2.6 banner update committed in the same PR.
- No OSS config file changes (no `model_config.toml` / `examples/` deltas).

---

## 9. Open questions / follow-ups

- Admin/debug endpoint to surface filter audit columns (out of scope; track in a separate issue if/when ops asks).
- Janitor for `metadata` JSONB shape evolution beyond `tips_amount_usd` (premature; revisit when the second user-marker type appears).
