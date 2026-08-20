# Async chat turn endpoint (v2) — Design

- **Date:** 2026-08-20
- **Status:** Approved
- **Type:** Engine change — one schema migration (queue table), one new
  endpoint (the first and only `/v2` route), one new background worker.
  The streaming path is untouched.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — additive throughout. Release numbering and
  timing are the owner's call, not this document's.

## 1. Motivation

The engine has exactly one way to send a chat message:
`POST /comp/chat/{session_id}/message/stream`, an SSE endpoint that generates
and streams in real time. That shape assumes a client that can hold a
connection open and consume frames — a web or app client.

Bot-style consumers (a messaging-platform bot such as a Telegram bot, a
background task, an offline-message sender) are asynchronous by nature. They
want to hand the engine a message, get an acknowledgement, and pick the reply
up later from the database — via Supabase Realtime on `engine.chat_messages`,
or via the history endpoint. Forcing them through SSE means holding a
connection they have no use for and discarding every frame on arrival.

The engine had an async endpoint once: `POST /comp/chat/{session_id}/message_async`
plus a pending-poll companion, removed in v0.2.0 (`a3f86ea`) when SSE landed.
It died of three specific wounds, all of which this design closes:

1. **No idempotency** — every retry spawned another generation.
2. **No concurrency control** — a bare `tokio::spawn` per request; two
   concurrent posts ran two pipelines.
3. **No durable state** — status was inferred from row presence; a crashed
   task left the turn permanently "processing", invisible to any reaper.

The new path name is versioned (`/v2/...`) precisely to avoid any confusion
with the removed endpoint.

## 2. Decisions

These were settled during design review and are not open for re-derivation
during implementation:

1. **Strict per-session LIFO.** Each session has its own logical stack; the
   newest pending message is always processed first. Sessions are independent
   and process in parallel.
2. **Every queued message gets a turn.** Older messages are never coalesced,
   superseded, or dropped by the queue — only reordered. (The pipeline itself
   may still ghost a turn; see §6.)
3. **User rows persist on receipt.** The user message is written to
   `chat_messages` inside the enqueue transaction, so history preserves true
   arrival order even when replies land out of order.
4. **No preemption.** LIFO reorders only messages that have not started. A
   message that arrives while a generation for the same session is in flight
   waits for that generation to finish, then takes the top of the stack.
5. **Auth unchanged.** The endpoint sits behind the existing `require_auth`
   layer and expects a per-end-user Supabase JWT. How a bot maps its platform
   users onto Supabase identities is the downstream consumer's concern.
6. **Queue state lives in a dedicated table**, not in new columns on
   `chat_messages`. Claim/attempt/failure facts are processing state, not
   message facts; the message table keeps its "a row exists ⇒ it happened"
   invariant.

### Why LIFO (design rationale)

Prior-art research found no production chat system that processes messages
LIFO — the norm is per-session FIFO, optionally with burst-coalescing or
cancel-and-supersede. LIFO here is a deliberate product choice, not an
oversight:

- **Single sends — the overwhelmingly common case — are unaffected.** The
  message is pushed and immediately popped; behavior matches the stream path.
- **Rapid successive sends (rare) mean the newest message is the one the user
  cares about now.** Answering it first reads as more human. Making a user's
  freshest message wait behind a backlog of stale ones would forfeit the
  point of an async path — a serial stream would do as well.

The known cost — older messages in a burst are answered later, and the reply
order inverts the send order — is accepted. Decision 2 caps that cost: nothing
is starved forever, because per-session serialization drains the stack between
arrivals.

## 3. Architecture

```
bot gateway (downstream, holds per-user JWT)
   │  POST /v2/comp/chat/{session_id}/message/async
   ▼
handler — reuse the stream endpoint's validation set
   │  one transaction: user row into chat_messages
   │                 + queue row into chat_turn_queue
   ▼  202 {status, user_message_id}
engine.chat_turn_queue          (authoritative queue state)
   │  tokio::sync::Notify nudge (fast path)
   │  interval poll             (correctness backstop)
   ▼
queue worker (fifth boot-time sweeper)
   │  claim: newest pending per session with no in-flight claim
   │  drive run_stream to exhaustion, discard frames
   ▼
engine.chat_messages            (persistence happens inside the generator)
   ▼
downstream pickup — Supabase Realtime / history endpoint (out of scope here)
```

The generation path gains nothing new: PDE, ghosting, input/output filters,
`insert_assistant_batch`, `post_process`, and all audit writes run exactly as
they do for a streamed turn, because the worker executes the same
`run_stream` generator and simply discards the frames.

## 4. Schema — migration 0054

```sql
CREATE TABLE engine.chat_turn_queue (
    id              UUID PRIMARY KEY,
    session_id      UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_message_id UUID NOT NULL UNIQUE REFERENCES engine.chat_messages(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','claimed','done','failed')),
    attempts        INT  NOT NULL DEFAULT 0,
    claimed_at      TIMESTAMPTZ,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX chat_turn_queue_active_idx
    ON engine.chat_turn_queue (session_id, created_at DESC)
    WHERE status IN ('pending','claimed');
```

- RLS is enabled and locked down in the same migration, following the
  0013/0016 pattern for engine-owned tables.
- `user_message_id UNIQUE` makes the queue row 1:1 with the driving user
  message and gives replay detection a join target.
- `done`/`failed` rows are retained as audit; no cleanup job. They are cheap,
  and a retention sweep can be added later if volume ever warrants it.

**Atomicity.** The user-message insert and the queue-row insert commit in one
transaction (a new store fn wrapping the existing
`upsert_user_message_idempotent` logic). A user row without a queue row would
be a message in history that nothing will ever answer — the same hole a
client-aborted stream leaves today, and not one this path may reintroduce.

## 5. Endpoint contract

`POST /v2/comp/chat/{session_id}/message/async` — merged into the
authenticated `comp` subtree; `#[utoipa::path]` carries the full prefix per
house convention; `openapi.json` regenerated.

**Request** — the `StreamSendRequest` field set, with one tightening:
`client_msg_id` is **required**. Bot gateways sit behind at-least-once
webhook delivery; without a dedup key every redelivery is a duplicate reply
generator. (Platform-native ids — e.g. a Telegram `update_id` — make natural
keys; minting them is downstream's job.)

**Responses**

| Case | Response |
|---|---|
| New message enqueued | `202 {"status":"queued","user_message_id":...}` |
| Same `client_msg_id` still pending/claimed (webhook redelivery) | `202 {"status":"already_queued","user_message_id":...}` |
| Same `client_msg_id` already processed | `200 {"status":"already_completed","user_message_id":...}` |
| Same `client_msg_id` terminally failed | `200 {"status":"failed","user_message_id":...}` — the client may retry with a fresh `client_msg_id` |
| Pending depth over limit | `429`, code `rate_limited` |
| Errors | `StreamPreError` shape and code table, same as the stream endpoint (`invalid_payload`, `session_not_found`, `session_forbidden`, `wrong_channel`, ...) |

Note the deliberate divergence from the stream endpoint: an in-flight
duplicate is `202 already_queued`, not `409 duplicate_in_progress`. For an
enqueue-only endpoint a redelivered request is a success ("your message is in
the queue"), not a conflict.

No `ai_message_id` in the response — assistant ids are minted at generation
time inside the pipeline. Consumers pick replies up from Realtime or history.

**The one new gate: per-session pending depth.** A configurable cap
(generous default: 20 pending rows per session) returns `429` when exceeded.
Without it, a hostile or broken client can enqueue unbounded LLM spend. This
is a threshold, not a capability — tighten by config later if reality asks.

## 6. Worker

The fifth boot-time sweeper, spawned in `main.rs` alongside dreaming,
snapshot, world, and world-town, cloned `AppState`, self-disabling when
config is off. `dreaming.rs`'s fused-claim shape and `store/world.rs`'s
`claim_due`/`release_claim` pair are the templates.

**Config (env):** enabled flag, poll interval, global generation concurrency,
claim-stale seconds, max attempts, per-session pending cap (§5), generation
timeout seconds.

**Wake-up.** After the enqueue transaction commits, the handler nudges the
worker through an in-process `tokio::sync::Notify` — a single-send message
starts generating immediately instead of waiting out a poll tick, so async
latency ≈ stream latency for the common case. The interval poll remains as
the correctness backstop (missed notifies, stale claims, process restart);
single-instance deployment makes Postgres `LISTEN/NOTIFY` unnecessary.

**Claim.** One fused statement per free worker slot, the repo's standard
shape:

```sql
UPDATE engine.chat_turn_queue
SET status = 'claimed', claimed_at = now(), attempts = attempts + 1
WHERE id IN (
    SELECT DISTINCT ON (session_id) id
    FROM engine.chat_turn_queue q
    WHERE status = 'pending'
      AND NOT EXISTS (
          SELECT 1 FROM engine.chat_turn_queue c
          WHERE c.session_id = q.session_id AND c.status = 'claimed')
    ORDER BY session_id, created_at DESC
    LIMIT $n
    FOR UPDATE SKIP LOCKED
)
RETURNING ...;
```

- `created_at DESC` within a session — strict LIFO.
- The `NOT EXISTS` guard — per-session serialization. The worker is the only
  claimer, so this needs no advisory locks.
- The exact SQL (fusing `DISTINCT ON` with `FOR UPDATE` may need a lateral or
  CTE rewrite) is an implementation detail; the semantics above are the spec.

**Execution.** The claimed turn is run by driving `run_stream` to exhaustion
with `StreamExt::next`, discarding frames. Persistence, PDE, filters, ghost
decisions, and post-process all live inside the generator already. The drive
is wrapped in `tokio::time::timeout` — a hung LLM call must not wedge a
session's queue slot forever, and HTTP-client timeouts have a documented
habit of silently not covering this.

**Completion ladder.**

- Success → `status = 'done'`.
- Error or timeout → release to `pending`, keep the incremented `attempts`,
  record `last_error`.
- Stale claim (`claimed_at < now() - stale_secs`, i.e. the worker died
  mid-turn) → reaped back to `pending` by the same sweeper pass.
- `attempts >= max` → `status = 'failed'` **and** one `role = 'system_error'`
  row inserted into `chat_messages`, so a Realtime consumer receives a
  failure signal instead of silence. (This revives the `system_error`
  producer the old async path once was.)
- Ghost → `run_stream` produces no assistant row and marks
  `ghost_decision = true` as it does today; the queue row is `done`. A silent
  companion is a completed turn.

**What the worker does not do:** it does not consult `StreamSlots` (that cap
is SSE-connection accounting, process-local and user-keyed); its concurrency
is its own global slot count plus per-session exclusivity.

## 7. Boundaries and non-goals

- **Downstream pickup is out of scope.** No push pipeline, no Realtime
  wiring, no bot-side delivery. The contract ends at rows in
  `engine.chat_messages`.
- **The stream path is untouched**, and the two paths are not serialized
  against each other. A stream request and an async turn on the same session
  can generate concurrently — exactly the behavior two concurrent streams
  have today. Accepted, not a bug.
- **No preemption** (Decision 4). Cancel-and-supersede is explicitly out.
- **No service-token auth.** Per-user JWT only (Decision 5).
- **No retention job** for `done`/`failed` queue rows (§4).

## 8. Testing

`#[sqlx::test]` coverage, following existing store/route test patterns:

1. Enqueue atomicity — user row and queue row commit or roll back together.
2. Idempotency mapping — `Inserted`/`DuplicateInProgress`/`Replay` →
   `queued`/`already_queued`/`already_completed`.
3. LIFO claim order within a session; arrival order across bursts.
4. Per-session exclusivity — no second claim while one is in flight;
   independent sessions claim in parallel.
5. Stale-claim reaping and the attempts ladder, including terminal `failed`
   plus the `system_error` row.
6. Pending-depth cap → `429`.
7. Worker loop against the existing pipeline test harness where a mock LLM
   exists; otherwise the state machine is covered at the SQL layer and the
   drive loop stays thin enough to read.

## 9. Open verification point (plan stage, does not block this spec)

**History window for an older-than-latest driving message.** When the worker
answers B after C's exchange is already persisted, the design requires B's
context to include C and C's reply — the companion must remember what it just
said. The stream handler resolves a history anchor from the driving message's
`sent_at`; if `run_stream` truncates history at that anchor, the worker path
must pass an anchor equivalent to *now* instead. Verify during
implementation planning and pin with a test either way.
