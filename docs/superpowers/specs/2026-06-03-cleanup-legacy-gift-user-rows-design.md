# eros-engine — Cleanup migration: drop legacy non-tip `gift_user` rows

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track. **One migration (`0027`), no code change.**
**Follows**: PR #76 (legacy Gift Event teardown). Resolves the two codex P2 findings on #76.

A one-time data cleanup that removes the legacy in-app **Gift Event** rows the removed
`event_gift` endpoint left in `chat_messages`, so the "`gift_user` = tip-only" invariant
PR #76 established in code also holds in the **data** for any database that accrued such rows.

---

## 0. Background

PR #76 removed the `event_gift` endpoint and made `gift_user` tip-only in code: the
`assemble_chat_request` history arm and both `compute_signals_for_session` queries no longer
gate on `metadata ? 'tips_amount_usd'` — they treat every `gift_user` row as a tip (a user
turn). That is correct going forward because the only remaining `gift_user` producer (the
streaming tip path) always writes `metadata.tips_amount_usd`.

codex flagged (two P2s on #76) that on a database which already contains **legacy** `gift_user`
rows — written by the now-removed `event_gift` route with a bare label (e.g. `"rose"`) and
**no** `tips_amount_usd` metadata — those rows would now be:
- sent to the model as `user` messages by `assemble_chat_request`, and
- counted as user turns by `compute_signals_for_session` (affecting `message_count` and
  `hours_since_last_message`, hence ghost / stale-message decisions).

The only client that ever called `event_gift` was `eros-app`, which is decommissioned and
archived; the OSS engine is not hosted by us, so fresh deployments have **zero** such rows.
This migration is therefore a no-op for fresh/OSS databases and a belt-and-suspenders cleanup
for any database that did accrue legacy rows. There is precedent for a data-cleanup migration
in this repo: `0022_filter_triggers_wipe_legacy_shape.sql`.

---

## 1. Goals / non-goals

**Goals**
- Delete exactly the **non-tip** `gift_user` rows from `chat_messages`, making
  "`gift_user` = tip-only" true at the data layer.
- Preserve all tips (`gift_user` rows carrying `metadata.tips_amount_usd`) and all
  `user` / `assistant` / `system_error` rows.

**Non-goals**
- **No code change.** The engine already treats `gift_user` as tip-only (PR #76).
- **Leave `companion_affinity_events` `event_type='gift'` rows.** They are append-only audit;
  the EMA effect of those legacy gift events is already baked into the stored affinity values,
  so deleting the rows would not change current state — it would only rewrite audit history.
  They are still validly listed by the affinity BFF (`routes/bff/affinity.rs`) and accepted by
  `debug.rs::VALID_EVENT_TYPES`.
- **Leave the immutable CHECK-constraint values** `event_type … 'gift'` (0002/0014) and
  `assistant_action_type … 'gift_reaction'` (0012). Applied migrations are immutable; a
  constraint that merely *allows* an unproduced/legacy value is harmless. (Consistent with #76.)
- The `chat_messages.role` CHECK keeps `'gift_user'` (tips use it).

---

## 2. Migration `0027_drop_legacy_gift_user_rows.sql`

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- One-time cleanup: remove legacy in-app Gift Event rows from chat_messages.
-- The event_gift endpoint (removed in #76) wrote role='gift_user' rows with a
-- bare label (e.g. "rose") and NO tips_amount_usd metadata. With gift_user now
-- tip-only, those rows would be miscounted as user turns by assemble_chat_request
-- and compute_signals_for_session. This deletes exactly the non-tip gift_user
-- rows; tips (gift_user rows carrying metadata.tips_amount_usd) are preserved.
-- Idempotent; a no-op for fresh/OSS deployments (eros-app, the only event_gift
-- caller, is the sole source of such rows). companion_affinity_events
-- event_type='gift' audit rows are intentionally left (append-only, already
-- EMA-applied, still listed by the affinity BFF).
--
-- Spec: docs/superpowers/specs/2026-06-03-cleanup-legacy-gift-user-rows-design.md

DELETE FROM engine.chat_messages
WHERE role = 'gift_user'
  AND (metadata IS NULL OR NOT (metadata ? 'tips_amount_usd'));
```

### Predicate rationale
- `role = 'gift_user'` scopes to the gift/tip role only.
- `metadata IS NULL OR NOT (metadata ? 'tips_amount_usd')` targets **non-tip** rows:
  - Legacy `event_gift` rows have `metadata IS NULL` → matched by the first disjunct. A bare
    `NOT (metadata ? 'tips_amount_usd')` would **miss** these (`NULL ? key` is `NULL`, and
    `NOT NULL` is `NULL`, which the `WHERE` treats as not-matched) — hence the explicit
    `metadata IS NULL` branch.
  - A hypothetical `gift_user` row with non-null metadata lacking the `tips_amount_usd` key →
    matched by `NOT (metadata ? 'tips_amount_usd')`.
  - Tip rows (`metadata.tips_amount_usd` present) → `metadata IS NULL` false and
    `NOT (true)` false → **not** matched (preserved).

### Safety
- **FK / threading:** `continues_from_message_id` is a plain nullable `UUID` (no enforced
  foreign key). Legacy `event_gift` returned `reply: None`, so no `assistant` row continues
  from a legacy gift row; tip rows (which *do* get replies) carry `tips_amount_usd` and are
  not targeted. Deleting the targeted rows orphans nothing and cannot error on a constraint.
- **Idempotent:** a second run matches no rows (`DELETE` is naturally repeatable).

---

## 3. Testing / verification

One `#[sqlx::test]` in `crates/eros-engine-store/src/chat.rs`, mirroring the existing
`0018` backfill test pattern (`#[sqlx::test]` auto-runs all migrations on an empty DB — which
makes `0027` a no-op — then the test seeds rows and **re-runs the migration SQL** via
`include_str!("../migrations/0027_drop_legacy_gift_user_rows.sql")`, exercising the real
statement; `DELETE` idempotency makes the re-run valid):

- Seed a `chat_sessions` row, then insert into `chat_messages`:
  - a `user` row (`metadata` NULL),
  - a **tip** `gift_user` row with `metadata = '{"tips_amount_usd": 20.0}'::jsonb`,
  - a **legacy** `gift_user` row with `metadata` NULL (content e.g. `'rose'`),
  - a `gift_user` row with `metadata = '{"label": "rose"}'::jsonb` (non-null, no tip key).
- Run the embedded `0027` `DELETE`.
- Assert: both non-tip `gift_user` rows are gone; the `user` row and the tip `gift_user` row
  survive (read back by `role` / `metadata`).

**Gate:** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test -p eros-engine-store` (DB tests via `.test-env`). `openapi.json` unchanged (no
route/DTO change).

---

## 4. Files touched

- `crates/eros-engine-store/migrations/0027_drop_legacy_gift_user_rows.sql` — new.
- `crates/eros-engine-store/src/chat.rs` — one `#[sqlx::test]`.

No code change to the engine; no change to `companion_affinity_events`, the CHECK constraints,
or the `gift_user` role.
