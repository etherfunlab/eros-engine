# eros-engine — Remove legacy Gift Event + dead GiftReaction machinery (`gift_user` → tip-only)

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track. **No migration.** `openapi.json` regenerated (one route + two schemas removed).
**Resolves**: Issue #72 (Option A — remove the legacy Gift Event path).

This tears out the legacy in-app **Gift Event** path and the **dead** gift-reaction
machinery, leaving the `gift_user` chat role dedicated exclusively to **tips**. The only
downstream consumer of the gift endpoint was `eros-app`, which is decommissioned and
archived — confirmed no live client calls it.

---

## 0. Background

`role = 'gift_user'` currently has **two** producers with different semantics sharing one
role:

1. **Tip** — `routes/companion_stream.rs`: `role='gift_user'`, `metadata.tips_amount_usd`
   set, `content` is `(打赏 $X)` or the user's accompanying text.
2. **Legacy in-app gift** — `routes/companion.rs::event_gift`: `role='gift_user'`, `content`
   a bare label (e.g. `rose`), **no** tip metadata.

Issue #72 documented the resulting cross-pipeline inconsistency (some paths gate on
`metadata ? 'tips_amount_usd'`, others include all `gift_user`). **Removing the legacy
producer dissolves that inconsistency at the root**: once `event_gift` is gone, every
`gift_user` row is a tip, so gated and ungated paths agree automatically.

Separately, an investigation confirmed the **gift-reaction machinery is dead code**:

- `ActionType::GiftReaction` is produced **only** by the PDE branch
  `if matches!(input.event, Event::Gift { .. })` (`eros-engine-core/src/pde.rs`).
- `Event::Gift` is constructed **only in tests** (`pde.rs` ×2, `pipeline/post_process.rs` ×1).
  The sole production `DecisionInput` site (`pipeline/stream.rs`) always builds
  `Event::UserMessage`; tips ride its `tips_amount_usd` field and the PDE routes them to
  `ActionType::Reply` (a tip is a user turn), never `GiftReaction`.
- Therefore `is_gift` at `stream.rs` (`plan.action_type == GiftReaction`) is **always
  false**: `build_gift_request`, `gift_reaction_context`, `PendingGift`, and
  `FrameActionType::GiftReaction` are unreachable. `pending_gifts` is always passed as `&[]`.
- The legacy `event_gift` endpoint **bypasses the PDE entirely** (it appends a row + applies
  affinity deltas directly), so it never drove the GiftReaction machinery either.

With the legacy endpoint removed and the GiftReaction path proven dead, both are deleted in
one sweep.

---

## 1. Goals / non-goals

**Goals**
- Remove the legacy `event_gift` HTTP endpoint and everything that exists only to serve
  non-tip in-app gifts.
- Make `gift_user` mechanically tip-only: simplify the now-tautological
  `metadata ? 'tips_amount_usd'` gates and drop the `is_tip_row` helper.
- Delete the dead gift-reaction machinery end-to-end (core action taxonomy + server
  prompt/stream/handler plumbing).

**Non-goals**
- **No DB migration.** The `gift_user` role stays in the `chat_messages` role CHECK (tips
  use it). The `companion_affinity_events.event_type` constraint (if it enumerates `'gift'`)
  keeps allowing `'gift'` — a harmless leftover with no remaining producer; dropping it is
  not worth a migration. No backfill, no rewriting historical rows.
- **Tip path untouched.** `tips_amount_usd` validation, `(打赏 $X)` persistence, the
  `tips_amount_usd` metadata marker + partial index (migration 0019), `tips_reaction_context`,
  and the BFF `tips_amount_usd` exposure all stay exactly as they are.
- No change to `recent_turn_pairs` / input-filter transcript assembly — they already include
  all `gift_user` rows, which is correct once all such rows are tips.

---

## 2. Removal — Group A: the legacy `event_gift` endpoint

`crates/eros-engine-server/src/routes/companion.rs`:
- Delete the `event_gift` handler (the `#[utoipa::path(...)] async fn event_gift`) — it
  appends a bare-label `gift_user` row and emits a `"gift"` `companion_affinity_events` row.
- Delete the `GiftEventBody` and `GiftEventResponse` DTO structs.
- Remove `.routes(routes!(event_gift))` from `router()`.
- Delete the endpoint's tests: `event_gift_appends_message_and_emits_event` and
  `event_gift_403_for_foreign_session`.
- Remove any now-unused imports left behind (e.g. `AffinityDeltas`/`AffinityRepo` if only the
  gift handler used them — verify against the file; `get_profile`/others may still need them).

`openapi.json` regenerated: the `/comp/chat/{session_id}/event/gift` path and the two
schemas disappear. This is the only generated-artifact change.

---

## 3. Removal — Group B: `gift_user` → tip-only (gate cleanup)

Once `event_gift` is gone, every `gift_user` row carries `tips_amount_usd`, so the tip gate
is a tautology. Simplify:

- `crates/eros-engine-server/src/pipeline/handlers.rs`
  - In `assemble_chat_request`, change `"gift_user" if is_tip_row(&msg) => ("user", …)` to a
    plain `"gift_user" => ("user", …)` arm (a tip turn is still emitted to the model under
    the `user` role — that behavior is preserved, just ungated).
  - Delete the `is_tip_row` helper.
  - Update the stale comment block that explains the tip gate and references the open issue.
- `crates/eros-engine-server/src/pipeline/mod.rs`
  - In `compute_signals_for_session`, change both the COUNT and the MAX(sent_at) queries from
    `role = 'user' OR (role = 'gift_user' AND metadata ? 'tips_amount_usd')`
    to `role IN ('user','gift_user')` (or `role = 'user' OR role = 'gift_user'`).
  - Retarget the test `signals_count_includes_tip_gift_user_but_excludes_legacy_gifts` →
    rename to e.g. `signals_count_includes_gift_user_tip_rows` and drop the legacy-gift
    fixture row + the "excludes legacy" assertion (legacy gift rows can no longer be
    produced). Keep it as a regression guard that `user` + `gift_user` rows are counted.

---

## 4. Removal — Group C: dead GiftReaction machinery

**`crates/eros-engine-core/src/`**
- `types.rs`: remove the `Event::Gift { … }` variant and the `ActionType::GiftReaction`
  variant.
- `pde.rs`: remove the `if matches!(input.event, Event::Gift { .. }) { … GiftReaction … }`
  branch in `decide`; remove `pick_gift_style` and the `ENERGY_COST_GIFT` constant (used only
  by that branch — verify); remove the tests `test_gift_event_maps_to_gift_reaction` and
  retarget `test_unknown_tip_personality_falls_back_to_warm` (which constructs `Event::Gift`)
  to exercise the tip-personality fallback through the live tip path (a `UserMessage` with
  `tips_amount_usd`) instead — or delete if redundant with existing tip tests.

**`crates/eros-engine-server/src/`**
- `prompt.rs`: remove `PendingGift`, `gift_reaction_context`, its `gift_reaction_context(&[], …)`
  test, and the `pending_gifts: &[PendingGift]` parameter from `build_prompt` (plus the
  `let gift = gift_reaction_context(...)` line and its concatenation into the system prompt).
  Update all `build_prompt(...)` call sites (incl. the many prompt-builder unit tests) to drop
  the argument.
- `pipeline/handlers.rs`: remove `build_gift_request`; in `build_reply_request` remove the
  `let pending_gifts: Vec<PendingGift> = vec![];` and the argument passed to `build_prompt`.
- `pipeline/stream.rs`: remove `FrameActionType::GiftReaction`; collapse the
  `ActionType::Reply | ActionType::GiftReaction` dispatch + the `is_gift` branch
  (`if is_gift { build_gift_request } else { build_reply_request }`) to call
  `build_reply_request` unconditionally; remove the `(FrameActionType::GiftReaction,
  "gift_reaction", ActionType::GiftReaction)` persist tuple (always `"reply"` now); remove
  the replay arm `Some("gift_reaction") => FrameActionType::GiftReaction` (default → Reply;
  no existing rows hold `"gift_reaction"` since it was never produced).
- `pipeline/post_process.rs`: remove `ActionType::GiftReaction` from the match arms (the
  `ActionType::Reply | ActionType::GiftReaction | ActionType::Proactive` groupings and the
  `ActionType::GiftReaction => "gift"` affinity-event-type mapping); remove/retarget the test
  `client_id_from_event_none_for_non_user_message` that builds `Event::Gift` (use another
  non-`UserMessage` event variant if one remains, else drop the case).

The `assistant_action_type` chat column stays (it only ever held `"reply"`).

---

## 5. Error handling / compatibility

- Removing an enum variant (`Event::Gift`, `ActionType::GiftReaction`, `FrameActionType::
  GiftReaction`) is a compile-time change; the compiler surfaces every match site to fix.
  None of these are serialized in a public DTO (they are internal decision/stream types) —
  the plan must confirm `FrameActionType` / `ActionType` / `Event` are not embedded in any
  `utoipa::ToSchema` DTO or OpenAPI component (expected: they are not — only the streamed SSE
  frame carries an action label as a plain string, always `"reply"`).
- No runtime behavior change for any live request: tips, replies, proactive, ghost all
  unaffected; the only externally observable change is the removed `event_gift` route.

---

## 6. Testing / verification

- **Build/compile** drives most of the work — the deleted variants force every consumer to
  be updated.
- **Unit/integration** (`#[sqlx::test]` + pure): the retargeted `signals_count` test; the
  retargeted/removed PDE tip-personality test; prompt-builder tests updated for the dropped
  `build_prompt` arg; `assemble_chat_request` still emits a `gift_user` (tip) turn under the
  `user` role (add/keep a test asserting a `gift_user` tip row reaches the model).
- **OpenAPI**: regenerate `crates/eros-engine-server/openapi.json`; the diff must show only
  the removal of the gift path + `GiftEventBody`/`GiftEventResponse` schemas.
- **Grep gate**: no residual `event_gift`, `GiftEventBody`, `GiftEventResponse`,
  `gift_reaction_context`, `PendingGift`, `build_gift_request`, `Event::Gift`,
  `ActionType::GiftReaction`, `FrameActionType::GiftReaction`, `is_tip_row`,
  `"gift_reaction"` references remain in `crates/` (outside intentional historical notes).
- **Full gate**: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -D
  warnings` (watch for newly-unused imports/consts after the deletions); `cargo test
  --workspace`.

---

## 7. Files touched

- `crates/eros-engine-server/src/routes/companion.rs` — remove endpoint + DTOs + router reg +
  2 tests (+ unused imports).
- `crates/eros-engine-server/src/pipeline/handlers.rs` — drop `is_tip_row`, ungate the
  `gift_user` arm, remove `build_gift_request` + `pending_gifts`.
- `crates/eros-engine-server/src/pipeline/mod.rs` — simplify `signals_count` queries + retarget
  its test.
- `crates/eros-engine-server/src/pipeline/stream.rs` — remove `FrameActionType::GiftReaction`,
  collapse the `is_gift` dispatch, remove the `"gift_reaction"` persist/replay strings.
- `crates/eros-engine-server/src/pipeline/post_process.rs` — remove `GiftReaction` match arms
  + the `"gift"` event-type mapping; retarget the `Event::Gift` test.
- `crates/eros-engine-server/src/prompt.rs` — remove `PendingGift`, `gift_reaction_context`,
  the `build_prompt` param + call sites/tests.
- `crates/eros-engine-core/src/types.rs` — remove `Event::Gift` + `ActionType::GiftReaction`.
- `crates/eros-engine-core/src/pde.rs` — remove the GiftReaction branch + `pick_gift_style` +
  `ENERGY_COST_GIFT` + retarget/remove tests.
- `crates/eros-engine-server/openapi.json` — regenerated.

No migration files. No change to the tip path, the `gift_user` role CHECK, or
`companion_affinity_events`.
