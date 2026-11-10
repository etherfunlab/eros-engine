# Streaming her proactive replies on `/open`, `/events` and `/life-choices` — Design

- **Date:** 2026-11-10
- **Status:** Approved
- **Type:** Engine change. No migration. Additive API changes on three
  routes: an optional `stream` request field, `Accept: text/event-stream`
  negotiation, and an SSE response. A request that asks for neither behaves
  exactly as today.
- **Owner:** enriquephl (sole dev)
- **Issue:** #505
- **Builds on:** `2026-05-19-sse-streaming-chat-0.2-design.md` (the frame
  vocabulary) and the proactive message path
  (`pipeline/proactive.rs::greet`).

## 1. Why

Three routes generate one of her messages inside the request and answer with
a single JSON body once generation has finished:

- `POST /v2/comp/session/{session_id}/open` → `{ greeting }`
- `POST /v2/comp/session/{session_id}/events` → `{ event, message, image_request }`
- `POST /v2/comp/session/{session_id}/life-choices/{choice_id}` → `{ status, message, affinity, event }`

The client learns nothing until the whole reply is written, although part of
the outcome is known much earlier: the event is drawn before its narration
starts, and a life-choice answer commits before her reply to it is
generated. A client that also receives committed rows over a realtime
channel can see her talk about an event before it can show the event.

This change lets each route answer as a server-sent event stream: what is
known before generation goes out first, then her words token by token.

## 2. Goals, non-goals, anti-goals

**Goals**

- Each of the three routes can answer as SSE, in the chat stream's frame
  vocabulary, with her text arriving as it is generated.
- `/events` leads with the drawn event; `/life-choices` leads with the
  resolved answer and the new affinity.
- `meta.message_id` is the id the row is stored under, so a client can match
  the streamed message against the same row from history or a realtime
  channel.
- The JSON response stays for every client that does not ask for a stream.

**Non-goals**

- No change to what is generated: same prompt, same cue, same model chain
  (`chat_companion`, platform client, no BYOK), same guards.
- No change to when or whether anything commits, or to how it is stamped.
  The event row still commits together with its narration, stamped at that
  moment; a dropped connection still drops the generation.
- No streaming of the event image prompt compose; it stays one non-streaming
  call.
- No queue row and no detached task for these messages.

**Anti-goals**

- A JSON-path behaviour change. The JSON path keeps its non-streaming
  `execute` call and its text post-processing untouched; the stream is a
  second path beside it, not a replacement under it.
- A client left showing a message or an event that was never stored (§4.4).

## 3. Asking for a stream

A request gets the SSE response when **either** holds:

- the JSON body has `"stream": true` (a new optional field on all three
  request bodies, default `false`), or
- the `Accept` header lists `text/event-stream`.

Neither present means the JSON response, byte for byte as today. An explicit
`"stream": false` with an SSE `Accept` header still streams: the two are
OR-ed, there is no precedence to reason about.

`Accept` matching: any listed media range equal to `text/event-stream`
(case-insensitive), unless it carries `q=0`. `*/*` does not count.

Everything that is rejected today before generation stays a plain HTTP
status with today's body, whichever response was asked for: 400, 403, 404,
409, 422, 429. Once a request is past those checks, a streaming request
answers `200` with `Content-Type: text/event-stream`, and every later
failure is a frame.

## 4. Frames

Frames are the chat stream's `ProtocolFrame`, serialized the same way
(`data: <json>`, type in the JSON `type` field, 15 s `ping` keep-alive).

### 4.1 Sequences

| Route | Frames |
|---|---|
| `/open` | `meta` → `delta…` → `done` → `final`. No greeting: `final` alone. |
| `/events` | `event` → `meta` → `delta…` → `done` → `image_request?` → `final`. Nothing drawn, or an event that is not told: `event` → `final`. |
| `/life-choices/{choice_id}` | `resolved` → `meta` → `delta…` → `done` → `final`. No reply: `resolved` → `final`. |

Any sequence can be cut short by a terminal `error` (§5).

### 4.2 New frames

**`event`** (`/events` only, always first):

```json
{ "type": "event", "event": { "kind": "…", "category": "…", "offline": false,
  "started_at": "…", "ends_at": "…", "message_id": "…" } }
```

`event` is today's `EventView`, or `null` when nothing was drawn (a pending
life choice, an empty draw). What the frame is a fact about depends on
`message_id`:

- **`message_id` is `null`: a stored event.** An event that is not told is
  inserted first, as today, and the frame carries what the read by
  `event_key` returns: the stored event, or `null` when the insert yielded
  and nothing landed. If that read finds a same-key winner that has a
  narration, the stream is the replay sequence (§4.5).
- **`message_id` is set: a drawn event, not yet stored.** The id is the UUID
  the narration will be stored under. The row commits with the narration, as
  today, so this event stands only once the `done` for that message arrives
  (§4.4). `started_at` and `ends_at` are the draw's: the stored event starts
  when the narration is stored, so the client moves the start to
  `done.sent_at` and keeps the length.

**`resolved`** (`/life-choices` only, always first):

```json
{ "type": "resolved", "status": "accepted", "affinity": { … }, "event": { … } }
```

Today's `status`, `affinity` (`AffinitySnapshot`) and `event` fields, sent
once the answer's transaction has committed. `event` is omitted when the
answer started none. Everything in this frame is committed; nothing after it
can undo it.

### 4.3 Changes to existing frames

- `meta.action_type` gains the value `proactive` (the stored
  `assistant_action_type` of these rows). `meta.model` is what `hop_display`
  gives for the serving hop, as on a chat turn. `continues_from` and
  `speaker_session_id` are never set.
- `done` gains two optional fields, present only on these three routes:
  `sent_at` (the stored row's `sent_at`) and `occasion` (the
  `GreetingOccasion` of the message). A chat turn's `done` does not carry
  them. `truncated` is always `false`; `usage` and `generation_id` as on a
  chat turn.
- `StreamErrorCode` gains `superseded` (§5.3).
- `image_request` and `final` are unchanged in shape. `final` carries the
  request's `tier`, `filtered: false`, `retries_chat` as the index of the
  hop that served (as the chat stream counts it), `retries_filter: 0`, and
  the `llm_attempts` / `gateway_errors` lists whenever they are non-empty.

`message_id` in `meta`, `delta`, `done` and `image_request` is the ULID
string form, as on a chat turn. The `event` frame's `message_id` is the
UUID form, as in `EventView` today. Both encode the same 128 bits.

### 4.4 Ordering rules, and what the client may keep

- `meta` is sent when the first non-whitespace text is ready, not before the
  upstream call. A reply that comes back blank therefore produces no `meta`:
  the stream goes straight to `final`, matching today's `greeting: null` /
  `message: null`.
- `done` is sent only after the row is stored.
- On `/events` with a picture, the image prompt is composed after the last
  `delta` and before the commit, so `done` arrives after that call;
  `image_request` follows `done`, as on a chat turn.

The client rule that follows, for every way a stream can end (`final`,
`error`, or the connection dropping):

- **A message exists once its `done` has arrived.** A `meta` whose `done`
  never comes is a message to drop.
- **On `/events`, an `event` frame with a `message_id` exists once the
  `done` for that id has arrived.** Without it the event is withdrawn,
  whether the stream ended in `error`, in `final` (a blank narration), or by
  a dropped connection. An `event` frame without a `message_id` stands as
  sent.

One narrow window remains: a connection that drops between the commit and
the `done` frame leaves a stored row the client was told nothing final
about. The same key replays it (§4.5), and so does history.

### 4.5 Replays

A request whose key already has a stored answer (an `event_key` that already
drew, a repeated `open_key`, a dated greeting already sent that day) streams
the stored row: `meta` → one `delta` with the full content → `done` →
`final`, with `event` first on `/events` and `image_request` after `done`
when the stored message has one. Nothing is generated. A replayed event that
has no narration is `event` → `final`.

`/life-choices` has no replay: a second answer is `409`, as today.

## 5. Failure and concurrency

### 5.1 Generation fails

The model chain is walked as today (`model`, then each fallback), with one
difference forced by streaming: **a fallback is only taken before the first
`delta` has been sent.** A hop that fails or garbles after text has gone out
ends the stream.

What counts as a failed hop is what counts in the non-streaming `execute`
these routes use today, not what the chat stream's live mode counts: a
status or transport error, `finish_reason: error`, or a byte-BPE garble. A
reply cut by `length` or `content_filter` is served and stored as a normal
message (`done.truncated: false`), and an empty completion is a blank reply
(§5.2) without trying the next model; on those three outcomes the stream
stores what the JSON path stores. Each hop runs under the chat stream's
open and total timeouts.

When no hop produces a reply, the stream ends with `error`, using the chat
stream's code mapping (`upstream_unavailable`, `rate_limited`, `timeout`,
`internal`). Nothing is stored for the message.

- `/open`: nothing stored; the same key can be asked again. `retryable` as
  the chat stream maps it.
- `/events`: neither the event nor the message is stored; the client
  withdraws the event, and the same `event_key` draws afresh. `retryable`
  as the chat stream maps it.
- `/life-choices`: the answer stands (`resolved` was sent). Today's JSON
  path swallows this failure into `message: null`; the stream reports it as
  `error` after `resolved`, always with `retryable: false`, since a second
  answer can only be `409`.

### 5.2 Blank reply

After the output rules and a trim leave nothing: no `meta`, nothing stored
for the message, `final`. On `/events` the event is not stored either, as
today; the stream is `event` → `final`, and the rule in §4.4 withdraws the
event because its `done` never came.

### 5.3 The insert yields

The inserts keep their guards: a message that landed in the session since
the request began, a life choice that became pending, an overlapping event,
a concurrent request that won the same key. In the JSON path a yielded
insert answers with the winner's row or nothing. In the stream her text has
already gone out, so the stream ends with:

```json
{ "type": "error", "code": "superseded", "retryable": false, … }
```

The client drops the bubble and, on `/events`, the event. To learn what did
land: on `/open` and `/events`, ask again with the same key, which replays a
same-key winner and otherwise answers by today's rules; on `/life-choices`,
read the session's history. `superseded` is only ever sent on these three
routes.

### 5.4 The client disconnects

Generation runs inside the response body, as `/persona/{id}/image/compose`
and the voice stream do. A dropped connection drops the generation: nothing
further is stored, the per-user generation slot is released, and the same
key asks again cleanly. This is what a dropped connection does on the JSON
path today. On `/life-choices` the answer was already committed and stands;
only her reply to it is lost, also as today.

## 6. Text

The streamed path cleans text the way the chat stream's live mode does: each
delta passes through the `StreamScrubber` for the serving model's output
rules, and the stored content is `apply_output_regex` over the whole reply,
trimmed. The concatenated deltas and the stored content can differ by
leading or trailing whitespace only. The fence-and-quote strip that the
non-streaming `execute` applies (`clean_response`) is not applied to
streamed replies, as it is not on a streamed chat turn.

## 7. How it is built

The shape, not the code:

- `proactive::greet` gains a streaming form: a stream of frames that walks
  the hop chain with `execute_stream_as`, yields `meta` / `delta`, and then
  performs exactly the inserts `greet` performs today, stamped as today,
  under a message id minted before the upstream call. The non-streaming
  `greet` is left as it is. What the two share (prompt build, cue,
  occasion-specific insert, event image compose) is called by both, not
  copied.
- Each route handler does today's validation and pre-generation work, then
  branches: JSON as today, or `Sse` over a stream that yields the leading
  frame, the streaming `greet`, and `final`. The slot guard moves into that
  stream. The handlers' return type becomes a plain `Response`, as
  `/persona/{id}/image/compose` already does.
- On `/life-choices`, the transaction commits in the handler before the
  response is returned; `resolved` is the stream's first item.
- OpenAPI: each route's `200` documents both `application/json` (today's
  schema) and `text/event-stream`; the request schemas gain `stream`. The
  checked-in `openapi.json` is regenerated.

Left to the builder: where the shared pieces are cut, whether the streaming
form is a second function or a parameter, the exact helper for `Accept`
parsing, and test layout. Tests follow the existing harness (wiremock
upstream with an SSE body, `sse_frames` over the response bytes) and cover,
per route: the frame order on the happy path, a replay, a blank reply, an
upstream failure before and after the first delta, a yielded insert, both
ways of asking for a stream, and that a request asking for neither still
gets today's JSON; on `/events` also the not-told branch and a picture.

## 8. Docs and release

- Docs go in a separate PR after the code PR: `docs/api-reference.md` and
  `.zh.md` (the three routes, the two new frames, the `done` and
  `StreamErrorCode` additions, the client rules of §4.4), and
  `docs/life-events-needs.md` and `.zh.md` where they describe the event and
  answer responses.
- Additive for every existing client; no migrating guide needed. The
  release note names the two new frames and the opt-in.

## 9. Decisions

Made by the owner:

- A stream is asked for by `stream: true` in the body, by
  `Accept: text/event-stream`, or both; the header exists for semantic
  completeness beside the body flag.
- The `event` frame is sent at the draw while the event row still commits
  with its narration; a stream that does not reach `done` withdraws the
  event.
- Generation runs inside the response body; a dropped connection drops it.

Made in this design:

- `meta` waits for the first non-whitespace text (§4.4).
- `occasion` and `sent_at` ride on `done` (§4.3).
- No fallback after the first delta (§5.1).
- A failed hop is defined as in `execute`: a cut-off reply is stored, an
  empty one does not try the next model (§5.1).
- A yielded insert is `error` / `superseded` (§5.3).
- A failed reply on `/life-choices` is an `error` frame with
  `retryable: false`, not a silent `final` (§5.1).
- A streamed event is stamped when it is stored, as on the JSON path; the
  `event` frame's times are the draw's (§4.2).
- The JSON path is not rebuilt on top of the streaming one (§2).
