# Voice barge-in via explicit interrupt + regeneration repair — design

- Date: 2026-08-11
- Status: design agreed, implementation plan pending
- Repo: `eros-engine`
- Touches the voice turn path shipped by `2026-07-07-voice-call-parts-design.md`
  and extended by `2026-08-09-voice-memory-bootstrap-recall-design.md` /
  `2026-08-09-voice-dreaming-ingestion-design.md`.

## Background & goals

Downstream wants **manual barge-in**: while the companion is speaking, the user
starts talking, the client stops playback and the engine stops generating.

Half of that is already free. `run_voice_turn` is a single `async_stream::stream!`
generator with no `tokio::spawn` anywhere in `pipeline/voice.rs`; axum's `Sse`
body owns it, so a client disconnect drops the generator at its current await
point, which drops the `DeltaStream` and closes the upstream connection. No
fallback candidate is tried, because the candidate loop only advances on
observed upstream conditions (open error/timeout `voice.rs:709`/`715`,
mid-stream `Some(Err(_))` `769`, clean-but-empty `775`, total timeout `738`) —
a disconnect is none of those. It is cancellation, not failure.

The other half is missing and is what barge-in actually depends on: **the
interrupted reply is never persisted.** `insert_voice_assistant_message` sits at
`voice.rs:807`, after the streaming loop, so a mid-stream drop destroys the
generator before it runs. The user row was already persisted by the route
(`routes/voice.rs:198`). The turn therefore leaves an orphaned `role='user'` row
with no assistant reply — even though the human *heard* part of a reply. The
next turn's history window (`VOICE_HISTORY_WINDOW = 8`) then shows two
consecutive user messages and a companion that never spoke. Barge-in exists to
make interruption frequent, so this compounds.

**Intent cannot be inferred at the transport layer.** The generator is dropped,
not resumed with an error; `Drop` has no error parameter and axum does not
expose the TCP close reason to the handler. Even a raw FIN/RST signal would not
help: a deliberate `abort()` with unread data in the socket — exactly the
barge-in case, since the server is mid-delta — produces **RST**, while a proxy
closing a dead network path produces **FIN**, and a mobile handoff produces
nothing at all until TCP retransmission gives up. The mapping is not merely
noisy, it is inverted for the case we care about.

So the client must say so explicitly. The presence or absence of an explicit
interrupt call becomes the classifier, and it is the only reliable one.

Goals:

1. A deliberate barge-in persists **what the user actually heard**.
2. An abnormal disconnect (network, crash) is repairable — the turn can be
   regenerated instead of being lost.
3. The two are distinguished by an explicit signal, never by inference.

## Non-goals

- No engine-side detection of *why* a connection closed. Out of reach, by
  design.
- No resume/continuation of a partially generated reply. A repaired turn is
  **regenerated from scratch**, not continued.
- No change to how generation stops. Disconnect already stops it; this spec
  adds no cancellation machinery.
- No streaming of `generation_id` to the client. `ProtocolFrame::Delta` keeps
  its `{message_id, content}` shape.
- No change to the text (`chat/stream`) path. It has the same
  persist-after-deltas structure (`stream.rs:581`, `// Persist BEFORE yielding
  Done`), but text clients do not barge in; if that changes it is a separate
  spec.
- No per-turn memory writes. The voice turn path stays read-only per
  `2026-08-09-voice-dreaming-ingestion-design.md`; an interrupted turn's rows
  are picked up post-call by the dreaming sweeper like any other.

## 1. Turn state machine

A voice turn's user row is always persisted first (`routes/voice.rs:198`).
The turn then reaches one of four terminal states:

| Terminal state | assistant row | interrupt marker | How it arises |
|---|---|---|---|
| Normal completion | yes | no | stream completes, `Done` |
| Deliberate barge-in | yes (spoken text) / no (nothing played) | **yes** | client aborts, then POSTs interrupt |
| Abnormal disconnect | no | no | abort or network death, no interrupt arrives |
| Upstream failure | no | no | all candidates failed, `Error` frame |

Retry of the same `client_msg_id` is decided by those two columns alone:

```
has assistant reply   → 409  (unchanged; this is what protects against double billing)
has interrupt marker  → 409  (deliberate; there is nothing to repair)
neither               → regenerate, reusing the existing user row
```

**This also fixes an existing bug.** The fourth state — upstream failure — is an
orphaned user row today. The engine emits `Error { retryable: true }`
(`voice.rs:794`) and then refuses the retry with 409 (`routes/voice.rs:203`).
The engine currently tells the client "retryable" and then rejects the retry.
The relaxation above repairs that case for free, with no extra mechanism.

## 2. The interrupt endpoint

```
POST /comp/voice/{session_id}/turn/interrupt
{ "client_msg_id": "01J…", "spoken_text": "你今天过得" }
→ 200 { "message_id": "01J…" | null }
```

`client_msg_id` travels in the body, matching the existing
`POST /comp/voice/{session_id}/turn/stream`. `spoken_text` is what TTS actually
played and MAY be an empty string.

Preconditions reuse the turn endpoint's ladder: session exists (404), owned by
the JWT user (403), session is voice-channel (409 `wrong_channel`). A
`client_msg_id` that names no row in this session is 404; one that names a row
which is not the session's latest user turn is `409 not_latest_turn` (see
below).

**No `501 voice_disabled` gate.** This endpoint makes no LLM call; it only
writes rows. Gating it on `[tasks.chat_voice]` would make an in-flight call's
interrupt fail if the deployment's config changed mid-call.

### Latest-turn guard

The named turn MUST be the session's most recent user row; otherwise the
request is rejected with `409 not_latest_turn`. Without this, the upsert below
would let a client overwrite the `content` of **any** past assistant reply —
assistant text is engine-generated, and the client having full control of *user*
content is not a reason to hand it control of the companion's words too. You can
only barge in on what is currently being spoken, so "latest turn" is also the
honest description of the feature.

The cost is an ordering requirement on the client: **send the interrupt before
the next turn.** That is the natural order anyway (you interrupt, then speak). A
late interrupt is rejected and the turn degrades to the abnormal-disconnect
state — recoverable, and strictly better than allowing history rewrites.

### Semantics — upsert on the assistant reply

Keyed by the user row's id:

| assistant row | `spoken_text` | Action |
|---|---|---|
| absent | non-empty | insert, `truncated = true` |
| absent | empty | write no assistant row |
| exists (completion race) | non-empty | overwrite `content`, set `truncated = true`; **keep** `model`, `usage`, `generation_id` |
| exists (completion race) | empty | leave `content` untouched |

**A fifth cell, outside this table: generator vs. generator, no interrupt
involved at all.** Two orphaned/live `insert_voice_assistant_message` writers
can race for the same turn — the branch's own premise (TCP retransmission
keeps a disconnected generator alive for tens of seconds) applies just as
well to a still-live generator racing the client's retry as it does to the
interrupt racing a generator. Migration 0041 still keeps this to one row, but
neither writer is the interrupt, so none of the four rows above apply — there
is no marker to make one side authoritative. The rule there is last-writer-
wins on **every** column (`content`, `truncated`, and the audit columns
together), never the audit-only refill the table above implies: refilling
only `model`/`usage`/`generation_id` while leaving a stale `content` in place
would pair one generation's text with a DIFFERENT generation's
`generation_id`, corrupting OpenRouter reconciliation. See
`insert_voice_assistant_message`'s `ON CONFLICT` clause in
`crates/eros-engine-store/src/chat.rs`, keyed off whether the existing row
carries the interrupt marker.

**An empty `spoken_text` never writes assistant content** — neither inserting
nor overwriting. The codebase holds a "never persist empty assistant content"
invariant (`voice.rs:791` refuses an empty `done`, `voice.rs:806` guards the
insert); an empty row would mislead history assembly and dreaming ingestion
alike. The fourth case is genuinely degenerate — generation finished but the
client played nothing — and leaving the completed reply in place is the least
destructive reading.

All four cases write the interrupt marker on the **user** row, so an empty
`spoken_text` is still distinguishable from a disconnect. Repeated interrupt
calls for the same turn are idempotent.

The governing split: **content belongs to the interrupt (it knows what was
played); billing audit belongs to the generator (it knows what actually
happened).**

### Marker and metadata shapes

User row, merged so unrelated keys survive:

```sql
metadata = COALESCE(metadata, '{}'::jsonb) || '{"voice_interrupt": true}'::jsonb
```

Assistant row: an interrupt-inserted row gets `{"voice_interrupt": true}`. On
the completion-race update the same key is **merged** into the existing
metadata, preserving the `relationship_scope` the generator wrote
(`voice.rs:805`). The interrupt request does not carry `relationship_scope`, so
an interrupt-inserted row simply has none.

### New store methods

- `voice_turn_repair_state(user_message_id) -> Option<VoiceTurnRepair> { has_reply: bool, interrupted: bool }`
  — one query returning both facts. `None` when `user_message_id` does not
  name an actual voice user turn (`role = 'user' AND channel = 'voice'`) —
  the id handed to this function comes from `insert_voice_user_message`'s
  conflict lookup, which is deliberately role-agnostic (a colliding row can
  be a `gift_user` tip row, or a text-channel `user` row), so the repair gate
  must independently confirm the row before treating it as repairable; the
  route falls back to the ordinary unconditional 409 on `None`.
- `resolve_latest_voice_user_turn(session_id, client_msg_id) -> LatestTurnLookup`
  — resolves the `client_msg_id` to a user row id and reports whether it is the
  session's most recent user row, so the route can separate 404 (no such turn)
  from `409 not_latest_turn` in one round trip.
- `upsert_voice_interrupt(session_id, user_message_id, candidate_assistant_id, spoken_text) -> Option<Uuid>`
  — marker write plus the assistant upsert in one transaction. Returns the
  assistant row's id when a row exists afterwards (inserted or updated),
  `None` only when nothing was played and no reply row exists (spoken_text
  empty and no completion race). `candidate_assistant_id` is used only on
  insert; on the race path the existing row's id is returned.

## 3. Regeneration repair path

`routes/voice.rs`'s `Duplicate` branch stops being an unconditional 409:

```rust
VoiceUserInsert::Duplicate(existing_id) => {
    let st = chat_repo.voice_turn_repair_state(existing_id).await?;
    if st.has_reply || st.interrupted {
        return Err(pre(StatusCode::CONFLICT, "duplicate", ...));  // wording unchanged
    }
    existing_id   // reuse the row; run the pipeline as normal
}
```

The existing user row is reused — no second user row, no duplicated utterance in
history.

**Content authority.** On the repair path the request body's `content` is
**ignored**; the persisted row is authoritative and the engine never rewrites
history. A mismatch is logged as a warn (it is a client bug and should not pass
silently). This has teeth beyond bookkeeping: the per-turn vector recall embeds
`turn.content` as its query text (`voice.rs:218-221`), so a repair must use the
persisted text or the same turn's recall drifts between attempts.

**Bootstrap is unaffected.** `set_voice_bootstrap` runs at `voice.rs:577`,
before the candidate loop, and is write-once. A repaired first turn therefore
reads `BootstrapPlan::Frozen` and reuses the snapshot. If the disconnect landed
before that line, no marker was written and the repair assembles it once. Both
orders are safe.

**No extra rate limiting.** The repair path is reachable only when the previous
attempt produced no reply at all — which is precisely when a retry *should*
issue a new call. The per-user in-flight cap (`CONCURRENT_STREAMS_PER_USER`)
still applies, and a client wanting to burn calls can already do so today with
fresh `client_msg_id`s, so this opens no new surface.

## 4. Migration 0041 — the double-write race

The client aborts the SSE and then POSTs the interrupt, but the server may not
have processed the FIN yet, so the generator can still be alive and later run
its own insert at `voice.rs:807`. Without a constraint that produces **two
assistant rows for one user turn**.

```sql
CREATE UNIQUE INDEX IF NOT EXISTS chat_messages_voice_assistant_reply_uidx
  ON engine.chat_messages (user_message_id)
  WHERE role = 'assistant' AND channel = 'voice';
```

`insert_voice_assistant_message` then becomes conflict-aware, refilling **only**
the audit columns and never touching `content` or `truncated`:

```sql
ON CONFLICT (user_message_id) WHERE role = 'assistant' AND channel = 'voice'
DO UPDATE SET model = EXCLUDED.model,
              usage = EXCLUDED.usage,
              generation_id = EXCLUDED.generation_id
```

The result is order-independent: whichever of the two arrives second fills in
its own half, and the final `content` is the interrupt's report either way. The
design does not depend on timing.

Operator note: voice writes exactly one assistant row per turn today, so
existing data should satisfy the index. If a deployment has duplicates the
migration fails loudly rather than silently dropping rows — check with
`SELECT user_message_id FROM engine.chat_messages WHERE role='assistant' AND
channel='voice' GROUP BY 1 HAVING count(*) > 1` before applying.

## 5. Known cost — audit nulls on interrupted turns

Barge-in kills the upstream connection, so the `Done` frame never arrives and an
interrupted row typically has `model`, `usage` and `generation_id` all NULL.
This is the same shape as the `companion_affinity_events` NULL trio: the turn
did not complete, which is a fact about the turn, not lost data. Reconciliation
continues to go through OpenRouter's own logs.

The client cannot supply them either — `ProtocolFrame::Delta` exposes only
`message_id` and `content`, and putting `generation_id` on the wire is a
non-goal above.

## 6. Testing

Following the existing `sqlx::test` + wiremock style in
`pipeline/voice.rs` / `routes/voice.rs`:

**Interrupt endpoint**
- non-empty `spoken_text` → assistant row with that content, `truncated = true`, marker on the user row
- empty `spoken_text` → no assistant row, marker still written
- completion race: assistant row pre-exists → `content` overwritten, `model`/`usage`/`generation_id` preserved, `relationship_scope` metadata preserved
- completion race with empty `spoken_text` → existing `content` left untouched, marker still written
- called twice → idempotent, still exactly one assistant row
- 403 non-owner; 409 on a text-channel session; 404 for a `client_msg_id` not in this session
- latest-turn guard: interrupt naming an older turn → `409 not_latest_turn`, and that turn's content is left untouched

**Regeneration**
- disconnect (no reply, no marker) + same `client_msg_id` → regenerates, and asserts **no second user row**
- upstream failure then retry → regenerates
- reply already present → still 409
- interrupt marker present → still 409
- retry body carrying different `content` → the persisted content is used

**Race and migration**
- generator and interrupt both write, tested in **both arrival orders** → exactly one row, `content` from the interrupt in both

**Bootstrap**
- first turn interrupted, then regenerated → frozen snapshot reused, not reassembled

## 7. Documentation to update

- `docs/api-reference.md` + `.zh.md` — the new endpoint, and the changed 409
  semantics on `turn/stream` (a retry after a failed/disconnected turn now
  regenerates instead of conflicting).
- `docs/architecture.md` + `.zh.md` — the voice turn terminal states, if the
  voice section enumerates them.

OpenAPI: the new route needs `utoipa` annotations and a regenerated
`crates/eros-engine-server/openapi.json` (CI diffs the snapshot).
