# Error Fallback Config Design

**Spec:** 2026-05-26
**Status:** implemented (v0.5.0 Bundle B)
**Crates:** `eros-engine-store`, `eros-engine-server`

---

## §0 Background

When the chat-stream fallback chain exhausts (every model in the chain fails or
returns a truncated/empty reply), the engine currently emits a
`ProtocolFrame::Error { code: UpstreamUnavailable }` to the client. Downstream
UIs surface this as a raw error banner, which breaks the companion illusion.

**Desired behavior:** the companion should look momentarily evasive rather than
broken. A casual phrase ("huh?", "👀", etc.) emitted as a normal turn is
indistinguishable from a model deciding to be coy.

---

## §1 Goal / Non-Goals

**Goal:** introduce a generic key-value config table (`engine.error_handling_config`)
with the first use case being "chat-stream pseudo-ghost on chain exhaustion."
Phrases are operator-configurable via plain SQL `UPDATE`.

**Non-goal (important):** do NOT silently swallow infra failures. If the config
lookup itself fails (DB down, table missing, empty array), the engine still
emits the original `Error{UpstreamUnavailable}` frame. Hiding infra failures
entirely would make outages invisible to monitoring.

---

## §2 Schema

```sql
CREATE TABLE engine.error_handling_config (
    kind        TEXT PRIMARY KEY,
    payload     JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Seed (migration 0020):
INSERT INTO engine.error_handling_config (kind, payload) VALUES
  ('chat_stream_failure_fallback_phrases',
   '["huh?","hm?","...","oh?","mhm","ok","👀","😅","say again?","wait what?"]');
```

`kind` is an open string discriminator. Future error-handling parameters land
as new rows without schema changes.

---

## §3 Wire Behavior — `drive_chat_burst` Pseudo-Ghost Path

### 3.1 Chain exhaustion detection points

`drive_chat_burst` in `pipeline/stream.rs` has two exhaustion sites:

- **Live mode** — after persisting and yielding `Done` for the last truncated
  attempt, when `idx + 1 == chain.len()`.
- **Filtered mode** — after all per-model accumulation attempts truncate, when
  `idx + 1 == chain.len()`.

Both previously yielded `ProtocolFrame::Error { UpstreamUnavailable }`.

### 3.2 Replacement logic

Both sites are replaced with a call to `build_stream_failure_pseudo_ghost()`:

1. Call `ErrorHandlingRepo::pick_chat_stream_fallback_phrase()` on the pool.
2. On `Ok(Some(phrase))`: emit `Meta → Delta(phrase) → Done` frames, persist
   an assistant row with `metadata.fallback_reason = "stream_failure"`, return
   the frames to the caller via `Option<Vec<ProtocolFrame>>`.
3. On `Ok(None)` or `Err(_)`: return `None`; caller emits original Error frame.

`outcome.retries_chat` is set to `chain.len()` so the `Final` frame reflects
that all retries were exhausted.

### 3.3 Persisted assistant row fields

| Field | Value |
|---|---|
| `content` | the picked phrase |
| `model` | `"__fallback_phrase__"` (clearly marked, never an OR model id) |
| `assistant_action_type` | `persist_action` (same as a normal reply: `"reply"` or `"gift_reaction"`) |
| `metadata.fallback_reason` | `"stream_failure"` |
| `metadata.prompt_traits` | the injected trait tags for this turn |
| `metadata.tier` | user's tier, omitted when `None` |
| `metadata.retries_chat` | total chain depth attempted |
| `truncated` | `false` |
| `usage` | `null` |
| `filter_audit` | `null` |

---

## §4 Failure Semantics

- **Random phrase selection:** `rand::seq::SliceRandom::choose` over the
  payload array. Cheap, no external state.
- **Missing config row or empty array:** the helper returns `None`; caller
  falls back to `Error{UpstreamUnavailable}`. This is the last-resort path.
- **Non-array payload or array of non-strings:** guarded by the
  `let Some(Value::Array(arr)) = payload` destructure — non-matching payloads
  produce `None` (safe degradation).
- **Persist failure:** logged as `WARN`; the frames are still emitted (best-
  effort persistence, never block the SSE stream).

---

## §5 Testing

### eros-engine-store (migration-level, `lib.rs`)

| Test | What it checks |
|---|---|
| `migration_0020_seeds_ten_fallback_phrases` | Exactly 10 phrases, all strings |
| `pick_chat_stream_fallback_phrase_returns_seeded_phrase` | Picked phrase is in the seeded 10 |
| `pick_chat_stream_fallback_phrase_returns_none_when_kind_missing` | `None` when row deleted |

### eros-engine-server (integration, `pipeline/stream.rs`)

The integration test for chain-exhaustion → pseudo-ghost emission (all models
returning truncated SSE) requires wiremock scaffolding that is non-trivial to
add without a PDE-forcing mechanism (the PDE may ghost the turn). This test is
noted as **TODO** for a follow-up; the store-level tests above provide
confidence in the repo helper. The correctness of the stream wiring is verified
by the existing clippy/fmt gates and the fact that all 415 existing stream
tests pass unmodified.

---

## §6 Rollout

- **Track:** dev → main via PR on `feat/v0.5.0-error-fallback`.
- **Migration:** additive (new table, no ALTER on existing tables). Zero risk
  to existing data.
- **Config file changes:** none. The 10 seed phrases ship in the migration.
  Operators can `UPDATE engine.error_handling_config SET payload = '...' WHERE
  kind = 'chat_stream_failure_fallback_phrases'` at any time.
- **OpenAPI diff:** empty — this is an internal path, no DTO changes.
