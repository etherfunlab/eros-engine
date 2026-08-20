# Stream-on-Queue: unifying the stream endpoint onto the chat-queue infrastructure

**Date:** 2026-08-21
**Status:** Approved
**Prereq:** the v2 async chat endpoint (PR #286, spec `2026-08-20-async-chat-endpoint-design.md`)

## 1. Goal and background

Today the SSE stream endpoint (`POST /comp/chat/{session_id}/message/stream`) moves the
`run_stream` generator into the SSE response body. When the client disconnects mid-turn,
axum drops the body, the generator is cancelled mid-flight, and **nothing persists** — the
user's message sits in history with no reply, no failure signal, and no recovery path.

PR #286 built a queue + worker that drives the same generator to completion detached from
any connection. This change reuses that infrastructure so that a stream turn survives its
connection: the generation always runs to completion and settles its queue row; the SSE
stream becomes a *tap* on the generation rather than its owner.

One generation pipeline, two front doors:

- **Async endpoint** — pure producer; the worker drives everything.
- **Stream endpoint** — enqueues the same way, drives its own turn in a detached task,
  and streams frames to the client for as long as the client stays.

## 2. Scope and non-goals

In scope: the chat stream endpoint's execution model, the shared drive/settle machinery
it needs, and the store support for born-claimed queue rows.

Non-goals:

- **No client-visible contract change.** Frame format, status codes (including the 409
  duplicate semantics), the per-user 3-concurrent-streams cap, and latency are preserved
  bit-for-bit on the happy path.
- **No long-poll / getHistory endpoint.** Post-disconnect pickup is the downstream
  consumer's concern (e.g. Postgres change feeds).
- **The voice turn endpoint is untouched.**
- **No runtime feature flag.** Rollback is redeploying the previous image.

## 3. Execution model: ownership moves to a detached task

The stream handler, after its existing pre-flight (validation, session ownership, image
capability, idempotent user-row upsert):

1. Persists the user row **and** a queue row in the same transaction (§4).
2. Spawns a **detached tokio task** that owns the generator: it drives `run_stream` to
   exhaustion, tallies Done/Error frames exactly like the worker path, and settles the
   queue row (§6). The detached task wraps its drive in `tokio::time::timeout` with
   `CHAT_QUEUE_GEN_TIMEOUT_SECS`, exactly as the worker's `process_turn` does: the reap
   threshold's arithmetic (§7) assumes every drive is bounded by the generation timeout,
   and an unbounded handler drive could be reaped and re-driven while still running.
3. Builds the SSE body from a subscription to the task's **frame tap** — an
   `mpsc::unbounded` channel the drive task forwards every frame into. Send errors
   (receiver dropped) are ignored.

Disconnect is therefore a non-event for the generation: the SSE body drops its receiver,
the drive task keeps running, the reply persists, and the queue row settles. The drive is
never backpressured by a slow or dead client (unbounded channel; frame volume per turn is
small — tokens plus a handful of control frames).

Ghosting, PDE, filters, and post-process all live inside `run_stream` already and need no
changes; they now simply always run to completion.

## 4. Queue row semantics for stream turns

- **Born claimed.** The stream enqueue inserts the queue row with `status = 'claimed'`,
  `claimed_at = now()`, `attempts = 1`, in the same transaction as the user row. It never
  passes through `pending` and is never eligible for `claim_next` while the handler's
  drive is live.
- **Bypasses the in-flight guard.** The handler drives its own row directly; it does not
  consult the per-session single-in-flight rule. Two rapid stream sends on one session run
  concurrently — today's behavior, preserved deliberately. Serialization remains available
  as a future ratchet (the mechanism exists; the gate is simply not applied here).
- **One-way serialization holds.** `claim_next`'s `NOT EXISTS (status = 'claimed')` guard
  sees stream-owned claimed rows, so async turns queue up behind a live stream turn on the
  same session. The reverse is intentionally not enforced.
- **Params are serialized** into the row exactly as the async endpoint does
  (`QueuedTurnParams`), because crash recovery (§7) rebuilds the turn from the row.
- **Depth cap:** the pending cap does not gate stream enqueues (their rows are born
  claimed, and the stream path has its own per-user stream cap). A live stream turn does
  count toward the session's `pending + claimed` total that gates *async* enqueues; that
  is correct — the session genuinely has a turn in flight.
- **Idempotency unchanged:** the existing upsert outcomes keep their stream-path
  responses (replay → replayed reply, duplicate-in-progress → 409). Only the `Inserted`
  outcome creates a queue row.

## 5. Concurrency semantics (decided)

| Situation | Behavior |
|---|---|
| Two stream sends, same session | Parallel generation — today's behavior, unchanged |
| Async turn in flight, stream send arrives | Stream runs immediately (bypasses guard) |
| Stream turn in flight, async send arrives | Async enqueues; worker waits for the stream row to settle |

Rationale: "behave exactly as today when the connection holds" was the design constraint;
per-session strictness for stream is a gate we can add later without new mechanism.

## 6. Terminal semantics: they follow the driver

| Driver | On generation failure | Rationale |
|---|---|---|
| Handler task (normal path) | **Single attempt.** Terminal immediately: `failed` + the `system_error` chat row (atomic, via the existing `mark_failed_with_notice`) | A connected user saw the Error frame and will resend — a background retry would race that resend into a double reply. A disconnected user gets the `system_error` row as their failure signal instead of a silent black hole. |
| Worker (crash recovery only, §7) | The async ladder, unchanged (release/retry up to `CHAT_QUEUE_MAX_ATTEMPTS`, then terminal) | By then no client is attached; the turn is indistinguishable from an async turn. |

Success and ghost outcomes classify exactly as the worker path (`classify_outcome`): any
Done ⇒ done; zero Done + zero Error ⇒ ghost ⇒ done.

`settle_turn` grows a terminal-on-failure mode (parameter, not a fork of the ladder
code). The served-guard re-check before going terminal applies to both modes.

## 7. Crash recovery (process death mid-turn)

Free, by construction: a killed process leaves the stream row `claimed`; the reaper
collects it after `CHAT_QUEUE_GEN_TIMEOUT_SECS + CHAT_QUEUE_CLAIM_STALE_SECS`; the row
goes `pending`; the worker claims it and blind-drives it from its params. The existing
replay guard prevents double replies if the reply had already persisted before the crash.
Worst-case recovery latency at defaults is ~10 minutes, same as the async path.

## 8. Unchanged surface

SSE frame contract and keepalives; the 409 duplicate-in-progress response; the per-user
stream slot cap — with one qualification: the `StreamSlotGuard` moves from the SSE body
into the detached drive task, so the cap keeps bounding three concurrent *generations* per
user (not merely three connections); a client that disconnects and immediately resends
still holds its old slot until the background drive settles; the ghost/PDE/post-process
pipeline; the async endpoint's behavior in every regard (PR A must show zero async-path
behavior change); the voice endpoint; all `CHAT_QUEUE_*` knobs and their meanings.

## 9. Delivery: two serial PRs

**PR A — infrastructure (no behavior change):**
- `drive_turn` accepts an optional frame tap (forwards every frame; drives to exhaustion
  regardless of tap liveness).
- `settle_turn` grows the terminal-on-failure mode.
- Store: `enqueue_user_message_claimed` (born-claimed variant sharing the transaction
  shape and idempotency handling of `enqueue_user_message`).
- Tests: tap receives the frame sequence; tap dropped mid-stream does not stop the drive;
  terminal-on-failure settles `failed` + one `system_error` row on first failure;
  born-claimed rows block `claim_next` for the session; async e2e suite untouched and
  green.

**PR B — stream endpoint rewire:**
- Stream handler: queue row joins the pre-flight transaction; drive spawns detached; SSE
  body subscribes to the tap.
- Tests: happy path streams the same frame sequence as before (regression lock);
  disconnect e2e — drop the SSE receiver mid-generation, assert the reply persists and
  the row settles `done`; failure e2e — terminal `failed` + `system_error` row, no retry;
  duplicate 409 regression lock.
- Docs: api-reference (stream section gains the disconnect-continuation note) and
  deploying (queue section mentions stream turns riding the table), each with their
  `.zh.md` mirrors; llm-audit/prompt-traits unaffected (already widened);
  `.env.example` unchanged.
- Extract the drive-to-exhaustion loop (frame tally + tap forward + `classify_outcome`)
  out of `drive_turn` into a shared function that accepts a prebuilt
  `PersistedUserMessage` and a prefetched `CompanionPersona`; the worker's `drive_turn`
  keeps its rebuild-from-row path and calls the shared loop, the stream handler calls it
  directly with what the request already resolved — zero added DB round trips on the hot
  path. The tap parameter migrates to the shared function.
- Move `StreamSlotGuard` into the detached drive task (see §8).
- Known async-path answer change, deliberate: once stream turns have queue rows, a
  replayed `client_msg_id` for a terminally-failed stream turn answers `failed` where it
  previously answered `already_queued`; also update the store comment 'no queue row means
  the turn is mid-flight on the STREAM path' which becomes false.

Each PR: branch off dev → PR → codex review → CI green → squash-merge, one at a time.
