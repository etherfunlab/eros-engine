# Image-turn discovery in chat history

**Date:** 2026-08-21
**Status:** Approved
**Prereq:** the recoverable `image_request` payload endpoint (PR #290)

## 1. Goal and background

PR #290 shipped the payload half of image-turn recovery: `GET
/comp/chat/{session_id}/messages/{message_id}/image-request` reproduces the wire-only
`image_request` frame from what the turn persisted. The discovery half is missing
(raised by a web client on the PR:
<https://github.com/etherfunlab/eros-engine/pull/290#issuecomment-5364944925>):

- **A client cannot learn which `message_id` to ask about.** `ChatHistoryEntry` carries
  no image marker, so the only strategy is probing recent assistant messages and reading
  404 as "not an image turn" — N requests per rehydrate, most of them 404s.
- **The 404 is overloaded.** "Not an image turn" and "compose event was never recorded"
  are indistinguishable from outside, and only the second should surface to a user as a
  genuinely unrecoverable image.
- **Canonical history rows are not addressable at all.** `ChatHistoryEntry` has no `id`
  field, so a consumer that rehydrates purely from the canonical route has no
  `message_id` to feed the recovery endpoint even if it knew which row to pick. Live
  consumers get ids from the `meta` frame; rehydrating consumers — the entire audience
  of #290 — get nothing. (The BFF mirror already carries `id`; only the flag is missing
  there.)

There are **two history surfaces**, and a cold-mounting client uses the BFF one:

- Canonical: `GET /comp/chat/{session_id}/history` → `ChatHistoryEntry`
  (full-row `ChatRepo::history()`).
- BFF: `GET /bff/v1/comp/chat/{sid}/history` and the bundled history inside
  `POST /bff/v1/comp/chat/start` → `BffHistoryEntry`
  (narrow-projection `ChatRepo::history_slim()`; the slim SELECT does not fetch
  `metadata`).

Both contracts get the flag, or the consumer that raised the issue never sees it.

This matters beyond bots: since stream-on-queue (#288) a disconnected stream turn still
completes and persists, but `image_request` is the turn's **last** frame and wire-only. A
disconnect before it fires leaves a persisted assistant message that promises an image no
one will ever draw, and the client cannot detect that from history.

## 2. Scope and non-goals

In scope: both history response contracts (canonical + BFF, including the start-bundle
history), the `history_slim` projection, docs, tests.

Non-goals:

- **No per-session batch endpoint** listing image turns. It adds a route and the client
  still has to diff the result against its own draw records; a flag on the history call
  every client already makes is the cheaper cut (free across pagination).
- **No `action_type` on history.** The persisted `assistant_action_type` is
  CHECK-constrained to `reply` / `gift_reaction`; the wire action names
  (`reply_text_image` / `reply_image`) are never persisted. The honest persisted signal
  for "this turn delegated an image" is `metadata.image` presence — a bool is exactly as
  much as the store can truthfully answer.
- **No change to the recovery endpoint or its 404 ladder.** The flag resolves the
  double-meaning client-side: a 404 on a *flagged* message now unambiguously means the
  compose event was never recorded (fail-open audit) — genuinely unrecoverable.
- **No migration, no extra query.** The canonical path is handler-level mapping only
  (`history()` already selects full rows). The BFF path adds **one projected
  expression** to the `history_slim` SELECT rather than widening it to full rows —
  the slim query stays slim.

## 3. Design

The flag everywhere, `id` where it was missing. All values derive from data the
existing queries already touch.

**Canonical — `ChatHistoryEntry`** gains two fields:

- `id: Uuid` — always present (the `engine.chat_messages` PK, same documentation as
  `BffHistoryEntry.id`). Makes canonical history rows addressable; closes the
  discovery → fetch loop entirely within the engine API, and makes the two history
  contracts symmetric.
- `image: bool` — serialized **only when `true`** (key-presence contract, matching
  `read_at` / `channel` on the same DTO). `true` iff the row's `metadata.image` object is
  present, i.e. the turn delegated an image to the consumer. Doubles as the
  "show a drawing/pending state on rehydrate" signal. Derived in the handler map from
  the full row's `metadata`.

**BFF — `ChatMessageSlim` + `BffHistoryEntry`**:

- `history_slim` projects `(metadata->'image' IS NOT NULL) AS image` into a new
  `ChatMessageSlim.image: bool` (`IS NOT NULL`, not the `?` operator: `metadata` is a
  nullable column and the flag must decode as `false`, not SQL `NULL`, on rows without
  metadata).
- `BffHistoryEntry` gains `image: bool` with the identical skip-if-false key-presence
  contract, so both routes read identically.
- `POST /bff/v1/comp/chat/start` bundles history through the same DTO and therefore
  **inherits the flag** — stated here explicitly because the start bundle is the path a
  cold mount actually uses.

OpenAPI regenerated. All additions are backwards-compatible (new keys, nothing removed
or renamed).

## 4. Documentation

`docs/api-reference.md` + `.zh.md`:

- History sections (canonical **and** BFF/start): document `id` and `image`, and the
  disambiguation rule — *flagged message + 404 from the image-request endpoint = the
  image is unrecoverable; surface it as such rather than silently dropping it.*
- Image-request section, one caveat line: `image_ref` is absent on rows persisted before
  the marker carried it (pre-v1.5.1). **Absent means unknown, not `face`** — a consumer
  that defaults absent to `face` will redraw a `previous`-ref turn against the wrong
  reference image.

## 5. Tests

Extend the existing history route tests, covering **all three serving paths** (canonical
history, BFF history, BFF start bundle):

- Every canonical entry echoes its row `id`.
- A seeded image turn (assistant row with `metadata.image`, seeded the same way as the
  #290 endpoint tests) serializes `"image": true` on each path.
- A plain assistant turn and a user turn omit the `image` key entirely on each path.

## 6. Rollout

Single PR to `dev`. Contract additions only; no config, no migration, no deploy coupling.
