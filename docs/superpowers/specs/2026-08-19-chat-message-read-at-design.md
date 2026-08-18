# Read receipts for chat messages — Design

- **Date:** 2026-08-19
- **Status:** Approved
- **Type:** Engine change — one schema migration (one nullable timestamp
  column), one new endpoint, one additive field on two history responses, one
  fire-and-forget write inside the text turn.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — additive throughout. The release number and its
  timing are the owner's call, not this document's.

## 1. Motivation

A chat client that wants to show read state has nothing to read it from. The
engine records when a message was written (`chat_messages.sent_at`) and nothing
about when it arrived anywhere. A downstream consumer that wants the ordinary
messenger affordance — the tick that says *this landed, this was seen* — has to
invent and store that fact itself, in a table the engine cannot see, keyed
against rows the engine owns.

That is the same shape of problem the session soft-delete spec described: a
rule about engine-owned data with no owner inside the engine.

The purpose of this change is to **give downstream the chance to render read
receipts**. The engine records timestamps; it does not count unread messages,
does not notify, and does not decide what "read" should look like on screen.

A secondary and genuinely incidental benefit: on a user's own row the stamp
falls at a moment nothing else records, so the gap `read_at - sent_at` becomes
a readable number (§5.3). That is a side effect of the receipt, not a reason to
build one.

## 2. Design principles applied

1. **One column, one concept: *this message reached the party it was addressed
   to*.** Not "an event happened to this row" — the concept has to survive being
   read by someone who has never seen this document.
2. **Two writers, disjoint by role, never overlapping.** A row has exactly one
   party that can read it, so it has exactly one writer. Any row where the
   answer to "who is the reader?" is *nobody* stays NULL from both.
3. **The engine records the fact; the consumer decides the meaning.** No unread
   count, no notification, no "seen by" endpoint. Rendering is downstream's.
4. **A timestamp is only worth a column if it is not already derivable.** Two
   candidate stamp points were rejected on exactly this ground (§5.2).
5. **Never on the latency path.** The engine's own write is fire-and-forget: a
   failure costs a missing timestamp, never a turn.
6. **A stamp records the first read, not the latest.** `read_at IS NULL` guards
   both writers, so a client polling on every mount cannot walk the value
   forward.

## 3. The column

`crates/eros-engine-store/migrations/0053_chat_messages_read_at.sql`:

```sql
ALTER TABLE engine.chat_messages
    ADD COLUMN read_at TIMESTAMPTZ;
```

Nullable, no default, no backfill, no index.

**No backfill** — every row written before the migration predates the concept.
There is no timestamp to reconstruct and no honest value to invent.

**No index.** The endpoint's `UPDATE` reaches its rows through the existing
`idx_chat_messages_session (session_id, sent_at DESC)`; the engine's reaches one
row through the primary key. Neither wants `read_at` indexed, and leaving it
unindexed is also what keeps the engine's stamp a HOT update — the row version
can stay on its page without touching any index entry.

### 3.1 Who writes what

| Row | Reader | Writer |
|---|---|---|
| `assistant` (and any future companion-authored role), **either channel** | the human | `POST /comp/chat/{session_id}/read` |
| `user`, text channel | the model | the engine, inside the turn |
| `user`, voice channel | — | nobody; stays NULL |
| `gift_user` | — | nobody; stays NULL |

Only the *engine's* writer is text-only. A voice `assistant` row is a message
addressed to the human exactly like a text one, and the endpoint stamps it.

`gift_user` is a tip the user sent. Nobody reads a tip on the user's behalf, so
there is no receipt to record and the row keeps `read_at` NULL from both sides.
This is the one place where the two predicates are not exact complements, and it
is deliberate: filling the column for every role would be filling it because the
column exists, not because a message reached a reader.

`system_error` is legal in the table's role `CHECK` but nothing in the engine
currently writes it. It is covered by the endpoint's predicate anyway — see
§4.1 for why the predicate is stated negatively rather than as `role =
'assistant'`.

## 4. Writer 1 — the endpoint

```
POST /comp/chat/{session_id}/read
→ 200 { "session_id": "…", "marked": 7 }
```

No request body. Session-scoped, whole-session: a client that opens a
conversation marks it read in one call. There is no per-message form and no
`up_to` watermark — a chat screen that renders a backscroll has shown the whole
window, and giving the client a partial-read vocabulary would mean the engine
storing a fact ("how far they scrolled") that it has no way to be right about.

- **Auth:** the existing bearer-JWT layer, through `require_session_for_user` —
  the same helper `GET /comp/chat/{session_id}/history` uses. Not your session
  is `403`, unknown is `404`, and an archived session is `404` for free
  (`get_session` filters `NOT archived`).
- **Voice sessions are not rejected.** A voice session's assistant rows are
  messages addressed to the human like any other. The `409 wrong_channel` gate
  on the send routes exists because those routes *write* channel-specific rows;
  this one does not.
- **Idempotent.** A second call returns `200` with `marked: 0`.

### 4.1 The write

```sql
UPDATE engine.chat_messages
   SET read_at = now()
 WHERE session_id = $1
   AND read_at IS NULL
   AND role NOT IN ('user', 'gift_user')
```

`marked` is `rows_affected()`, so it counts real transitions: a client can call
this on every mount without manufacturing an event.

The role predicate is **negative on purpose**. Written as `role = 'assistant'`
it would silently skip `system_error` today and any companion-authored role
added later; written as the complement of what the user authored, a new
companion-side role is covered the day it is introduced. The two roles named in
the `NOT IN` are exactly the two a user can author, which is a fact about the
send path (`companion_stream.rs` picks the role from `tips_amount_usd`), not a
list that grows.

## 5. Writer 2 — the engine

One call site, in `run_stream` (`pipeline/stream.rs`), **immediately before
`run_pde_decision`** — the moment the turn hands the message to the PDE judge.

```sql
UPDATE engine.chat_messages
   SET read_at = now()
 WHERE id = $1 AND role = 'user' AND read_at IS NULL
```

Dispatched with `tokio::spawn` and never awaited. It runs while the turn is
already blocked on the judge's LLM call, so it costs the turn nothing and sits
outside its failure surface entirely: a pool hiccup produces a log line and a
missing timestamp, never a degraded reply.

**Dispatch, not confirmed receipt.** The stamp lands before the judge's call
returns, so a judge that then fails on transport leaves it standing. That is the
intended reading: this is the *delivered* tick, and handing the message over is
the only half of the exchange the engine can witness. Waiting for the judge's
response instead would fold the judge's own latency into the gap of §5.3 and
change what the number means. The narrow case where nothing reads the message at
all — judge transport failure *and* the rule-engine fallback returning `ghost` —
is accepted at that reading.

The `role = 'user'` guard and the endpoint's `role NOT IN (…)` keep the two
writers off each other's rows even though nothing else coordinates them.
`read_at IS NULL` makes a replayed turn (`replay_stream`, reached on a duplicate
`client_msg_id`) a no-op.

Voice needs no exclusion rule: a voice turn runs `run_voice_turn`, a different
function that simply never calls this. There is no judge on that path, and its
pre-flight is a different shape anyway — recall and its embedding run *before*
the model there, so a voice `read_at - sent_at` would not measure the same thing
as a text one (§5.3). Two channels writing incomparable numbers into one column
is worse than one channel writing none.

### 5.1 Why the judge, and only the judge

The judge is the first generative model that sees the message. It is dispatched
before vision, before the input filter, and before `build_reply_request` —
deliberately, so that a `ghost` verdict short-circuits all of them. So "the
first model read it" and "the judge was dispatched" are the same instant.

**Deployments with the judge disabled do not stamp user rows.** When
`resolve_pde()` returns `None` — no `[tasks.pde_decision].filter_prompt`
configured — the turn goes through the rule engine and this stamp never fires;
so does a tip turn, whose driving row is `gift_user` and out of scope regardless.
This is accepted, not a gap to close. Covering it would mean four call sites
(judge / vision / input filter / chat burst) plus a local latch to fire once, and
four places to keep correct forever, to serve a configuration where the receipt
is a nice-to-have. One call site is the whole of it.

### 5.2 Why not the two obvious alternatives

**At INSERT.** `sent_at` is `DEFAULT now()`, stamped by Postgres on the same
statement that creates the row — it already *is* the moment the engine received
the message. A `read_at` written there would be a verbatim copy, and the row
would carry one fact in two columns.

**When the assistant row lands.** That value is `assistant.sent_at`, already in
the table. It would also miss every ghost turn, which produces no assistant row
at all.

The judge dispatch is the only candidate that records something the schema does
not already know.

### 5.3 What `read_at - sent_at` measures

On a text user row, the gap spans exactly the engine's pre-flight work between
persisting the message and dispatching the judge:

1. `affinity_repo.load_or_create` — DB
2. `apply_time_decay` + `refresh_endpoints` — CPU
3. `compute_signals_for_session` — DB
4. `recent_product_qa_pairs` — DB, only when product-QA is enabled
5. `build_input_filter_transcript` — DB, 20 rows
6. `build_pde_ctx` — CPU

It does **not** include embedding, memory or world recall, vision, the input
filter, or any model's generation time — all of those run after the judge.

## 6. Read surfaces

`read_at` is added to both history responses, with
`#[serde(skip_serializing_if = "Option::is_none")]`:

- `GET /comp/chat/{session_id}/history` — `ChatHistoryEntry`
- `GET /bff/v1/comp/chat/{session_id}/history` — `BffHistoryEntry`, and the
  bundled history inside `POST /bff/v1/comp/chat/start`

An unread row **omits the key** rather than sending `null`. A client tests for
presence; there is no third state to encode.

Both are needed. The BFF route is what a chat screen actually mounts from — a
client that could not see the field there would have to make a second call to
the canonical route for it. The canonical route is the documented OSS contract
and the two must not diverge in shape.

`ChatMessageSlim` gains the column in its projection (`history_slim`'s explicit
`SELECT` list); `ChatMessage` picks it up for free from `SELECT *`.

**There is no BFF read endpoint.** BFF exists to collapse round-trips on a cold
mount; a single write has nothing to bundle with.

## 7. Testing

| # | Level | What it pins |
|---|---|---|
| 1 | store | `mark_session_read` stamps companion-authored rows, leaves `user` and `gift_user` NULL, returns the transition count, and never pushes an existing stamp forward |
| 2 | store | `mark_user_message_consumed` stamps a `role='user'` row once; a second call is a no-op; `assistant` and `gift_user` rows are untouched |
| 3 | route | `POST …/read` returns `marked`, then history echoes `read_at` on stamped rows and omits the key on unread ones; a second call returns `marked: 0` |
| 4 | route | `POST …/read` is `403` for a foreign session, `404` for an unknown one, `404` for an archived one |
| 5 | BFF | `GET /bff/v1/…/history` carries `read_at` on a stamped row and omits it on an unread one |
| 6 | e2e | a text turn stamps its driving user row; the value is `>= sent_at` |

Test 6 must build its own `ModelConfig` — `test_state` uses
`ModelConfig::default()`, whose `tasks` map is empty, so `resolve_pde()` returns
`None` and the judge never runs. Use `ModelConfig::from_toml_str` with
`[tasks.pde_decision]` configured, the pattern already in `stream.rs`, plus a
`wiremock` server for the completions endpoint. Because the engine's write is
spawned rather than awaited, the assertion polls for the value rather than
reading once.

Adding the column changes two struct definitions, which forces `read_at: None`
onto their test fixtures in `pipeline/{handlers,stream,voice}.rs`. Those are
call-form changes compelled by the type, not weakened assertions.

## 8. Downstream

Additive on every surface — no existing field changes shape or meaning.

A consumer that wants receipts calls `POST /comp/chat/{session_id}/read` when a
conversation is on screen and reads `read_at` off the history entries. The two
halves of a messenger-style receipt land on different rows: the human's read of
the companion's message on `assistant` rows, the model's read of the human's
message on `user` rows.

Consumers must treat NULL as "no receipt", never as "not yet read" — voice user
rows and `gift_user` rows are permanently NULL by design, and so are user rows
on a deployment running without the judge.

Regenerate `crates/eros-engine-server/openapi.json`
(`cargo run -p eros-engine-server -- print-openapi > …`) — CI diffs it. Add the
endpoint and the response field to `docs/api-reference.md` and its `.zh.md`.

## 9. Not doing

- **Unread counts, badges, notifications.** The engine records timestamps; a
  consumer that wants a count derives it from the history it already has.
- **Per-message or `up_to` granularity.** §4.
- **Stamping `gift_user` or voice rows.** §3.1.
- **Stamping user rows outside the judge.** §5.1.
- **An index on `read_at`.** §3.
- **An "unread" or "mark unread" endpoint.** Reverting a receipt is an operator
  `UPDATE`, the same way reviving an archived session is.
