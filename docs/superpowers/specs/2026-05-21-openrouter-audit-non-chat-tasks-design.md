# OpenRouter Audit for Non-Chat LLM Tasks — Design

**Status:** Draft for review
**Date:** 2026-05-21
**Owner:** @enriquephl

## Problem

The OpenRouter client (`eros-engine-llm`) already supports audit passthrough
(`ChatRequest.user` / `session_id` / `metadata`), and the chat reply path
(`chat_companion`) forwards the caller-supplied `audit` on every turn. But the
engine's *other* LLM tasks build their `ChatRequest` with `..Default::default()`
and send no `user`, so in OpenRouter's dashboard their spend/usage is
unattributed:

- `affinity_evaluation` — per-turn haiku eval (`post_process.rs`)
- `insight_extraction` — per-turn fact + structured-insight extraction, two
  LLM calls (`post_process.rs`)
- `memory_extraction` — background "dreaming" session sweeper (`dreaming.rs`)

We want OpenRouter audit (`user` only) on these tasks too.

## Decisions (from brainstorming)

1. **Only forward `user` (the client id).** Not `session_id`, not `metadata`.
   OpenRouter's session id is not useful to us right now and user-only is the
   simplest correct thing.
2. **Reuse the client id the client already sends** (`audit.user`). No new
   frontend field, no new id.
3. **`affinity_evaluation` + `insight_extraction`** run inside
   `post_process::run`, which owns the driving `Event::UserMessage` — so they
   read `audit.user` straight off the event.
4. **`memory_extraction` (dreaming) forwards a fixed system sentinel**, not a
   real user id. Rationale: dreaming is a background task, not a per-turn
   user-triggered call; per-user dreaming attribution is meaningless. A single
   constant lets us read dreaming's *aggregate* usage in OpenRouter under one
   bucket. Sentinel: `11111111-1111-1111-1111-111111111111` (the `0…1`
   nil-plus-one is avoided because that id is already used in our database).

## Non-Goals

- **No change to `chat_companion`.** It already forwards the full audit object
  (`user` + `session_id` + `metadata`) on both the sync and streaming paths.
- **No `session_id` / `metadata` forwarding** for the three tasks here.
- **No new request DTO / API surface.** The client id arrives via the existing
  `audit.user` field on the chat request; nothing new is asked of the frontend.
- **No new validation.** `audit.user` was already validated (≤256 chars) by
  `validate_llm_audit` at the HTTP boundary when the chat request arrived; we
  forward the same value unchanged.
- **No persistence.** We do not store the client id anywhere (the dreaming
  case is handled by the constant, not by reading a stored client id), so no
  schema/migration changes.

## Design

### Client-id extraction (post_process tasks)

`Event::UserMessage` already carries `audit: Option<LlmAudit>`, and
`handlers.rs` already has:

```rust
fn audit_from_event(event: &Event) -> Option<&LlmAudit> { ... }
```

- Widen its visibility to `pub(in crate::pipeline)` so the sibling
  `post_process` module can reuse it (no second extractor).
- Add to `post_process.rs`:

```rust
/// The OpenRouter `user` (client id) to attribute this turn's post-process
/// LLM calls to. Forwards ONLY the caller's `audit.user` — never session_id
/// or metadata (audit decision: client id only).
fn client_id_from_event(event: &Event) -> Option<String> {
    super::handlers::audit_from_event(event).and_then(|a| a.user.clone())
}
```

In `post_process::run`, compute once:

```rust
let client_id = client_id_from_event(&event);
```

Thread `client_id.as_deref()` into the two task entry points.

### `affinity_evaluation`

`evaluate_affinity(...)` gains an `audit_user: Option<&str>` parameter; its
`ChatRequest` sets `user: audit_user.map(String::from)` (replacing the current
implicit `None` via `..Default::default()`). `session_id` / `metadata` stay
`None`.

### `insight_extraction`

`extract_insights(...)` gains `audit_user: Option<&str>` and passes it to both
LLM calls:

- `extract_facts(...)` → `ChatRequest.user = audit_user.map(String::from)`
- `extract_structured_insights(...)` → same.

### `memory_extraction` (dreaming)

`dreaming.rs` defines:

```rust
/// Sentinel OpenRouter `user` for system-initiated (non-user-triggered) LLM
/// calls. Dreaming runs in the background sweeper with no live request, so
/// per-user attribution is meaningless; this buckets all dreaming spend
/// under one id. All-ones (not the `0…1` nil-plus-one, which is already used
/// in our database). Not a real auth UUID (v4) and not a hashed client id,
/// so it cannot collide with a real user.
const SYSTEM_AUDIT_USER: &str = "11111111-1111-1111-1111-111111111111";
```

`classify_session`'s `ChatRequest` sets `user: Some(SYSTEM_AUDIT_USER.into())`.
`session_id` / `metadata` stay `None`.

### Behavior when the client omits audit

If a chat request carried no `audit` (or `audit.user` was absent),
`client_id_from_event` returns `None`, so `affinity_evaluation` /
`insight_extraction` send no `user` — identical to today. Dreaming always sends
the sentinel regardless.

## Tests

- **`client_id_from_event` (unit, `post_process.rs`):**
  - `Event::UserMessage` with `audit { user: Some("u_abc"), session_id:
    Some("s"), metadata: {non-empty} }` → returns `Some("u_abc")` (documents
    that only `user` is taken; session_id/metadata are ignored).
  - `Event::UserMessage` with `audit: None` → `None`.
  - A non-`UserMessage` event (e.g. `Gift`) → `None`.
- **Dreaming sentinel (unit, `dreaming.rs`):** assert
  `SYSTEM_AUDIT_USER == "11111111-1111-1111-1111-111111111111"` — a tiny guard
  so the sentinel can't be changed by accident.
- **Wire serialization is already covered** by `openrouter.rs`'s
  `wire_request_includes_audit_fields_when_set` /
  `wire_request_omits_audit_fields_when_none`, which prove `user` is emitted
  when set and omitted when `None`. We do not stand up a wiremock harness per
  background task — low value, high cost, and the serialization seam is already
  proven.

## Risks / Open Questions

1. **Sentinel format.** `11111111-1111-1111-1111-111111111111` is a readable
   "system" marker that is not a valid v4 UUID, so it won't collide with a real
   Supabase auth id or a hashed client id. If a different shorthand was
   intended (e.g. literal `"system"`), it's a one-line const change.
2. **`audit.user` is opaque.** The engine never inspects it; callers hash PII
   out (per `openrouter.rs` doc). We forward it verbatim, so any PII policy
   remains the caller's responsibility — unchanged from the chat path.

## Acceptance Criteria

- [ ] `cargo test -p eros-engine-server -p eros-engine-llm` green
- [ ] `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean
- [ ] `affinity_evaluation` / `insight_extraction` outbound requests carry
      `user` = the turn's `audit.user` when present, and no `user` when absent
- [ ] `memory_extraction` outbound requests carry
      `user = "11111111-1111-1111-1111-111111111111"`
- [ ] No `session_id` / `metadata` forwarded by any of the three tasks
- [ ] `chat_companion` behavior byte-identical to today
