# Speech markup for chat messages — Design

- **Date:** 2026-11-27
- **Status:** Approved
- **Type:** Engine change. No migration. Additive: one new route, one new
  `model_config.toml` task (`[tasks.chat_speech]`), off when absent, one
  new SSE frame. The speech text is not stored.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-07-07-voice-call-parts-design.md` (the voice turn:
  its thin prompt, its model resolution, its lean frames),
  `2026-07-11-voice-tts-audio-tags-design.md` (inline audio tags),
  `2026-08-02-voice-relationship-line-bond-chemistry-design.md` (the
  relationship line), `2026-11-10-proactive-reply-streaming-design.md`
  (one route answering in JSON or as a stream),
  `2026-11-27-erotic-tone-design.md` and the two tone designs before it
  (`metadata.tone`).

## 1. Intent

A client that wants to read one of her chat messages aloud needs more than
its words: a text-to-speech model that understands inline audio tags
(`[whispers]`, `[sighs]`, `[laughs]`) speaks far better when the text
carries them. The voice channel already writes its replies that way. Chat
messages are written to be read, not spoken.

This adds a task that turns chat messages she already sent into the same
text with delivery cues, for the client to hand to its TTS model. The cues
are chosen from what the engine knows about that message: who she is, the
tone the message was written in, where the pair stands, and what was being
said.

It is not a general "mark up any text" service. The input is a message of
a chat session, because the tone and the relationship are part of the
judgement.

## 2. Goals, non-goals, anti-goals

**Goals.**

- One request names up to eight of her messages in a session; each comes
  back as speech text, or with its own error.
- The markup reads the message's `metadata.tone`, the pair's affinity, the
  persona and the lines just before the message.
- By default her spoken words are kept as written; a request switch lets
  the model rephrase for the ear.
- The route answers in JSON or as a stream, as the proactive routes do.
- A deployment without `[tasks.chat_speech]` is unchanged.

**Non-goals.**

- Speech synthesis. The engine returns text; TTS is the caller's.
- Group sessions and voice sessions. A voice reply is already spoken text.
- Bring-your-own-character sessions: the request carries no card, so there
  is no authored head to judge from.
- The user's own messages.
- Any change to how chat replies or voice replies are generated.
- An audit table of its own. Each model hop is recorded in
  `engine.llm_generations`, as every LLM response the engine owns is.

**Anti-goals.**

- Free text in the request. The words to mark up always come from a stored
  message.
- The result stored anywhere: no column, no metadata key, no cache. The
  caller keeps the audio it synthesises.
- A second tag vocabulary. The tags are the ones the voice channel names,
  listed once in the code.
- Any effect on the conversation: no chat row written or changed, no
  affinity, memory or insight work, no unread or read state touched.

## 3. Approaches

Three ways to build it:

1. **A task of its own, a route under the chat messages, stateless.**
   Taken.
2. **No new task: reuse `[tasks.chat_voice]`.** The owner asked for a new
   task; a deployment also wants a different model here (a short rewriting
   job) from the one that holds a live call.
3. **Write the speech text at turn time, next to each reply.** It would
   have to be stored, and most messages are never played.

Inside the taken way the owner ruled four points:

- **Batch.** A list of message ids, each converted by its own model call,
  over one call for the whole list (one bad output would void the batch)
  and over one id per request.
- **The words.** Kept by default, with a switch for rephrasing, over
  either alone. A strict mode (strip the tags, compare byte for byte) was
  set aside: it would leave emoji and bracketed narration for the TTS
  model to read out.
- **Storage.** None, over a copy on the message row.
- **Response.** JSON and SSE both.

## 4. The route

```
POST /comp/chat/{session_id}/messages/speech
```

Same authentication and ownership check as the chat send routes. Feature
off (no `[tasks.chat_speech]`): `501`, as the voice turn answers without
`[tasks.chat_voice]`.

### 4.1 Request

```json
{ "message_ids": ["…", "…"], "rewrite": false, "stream": false }
```

| field | value |
|---|---|
| `message_ids` | 1 to 8 distinct message ids, in the form the history route returns them |
| `rewrite` | optional, default `false` (§6.2) |
| `stream` | optional, default `false`; `true`, or an `Accept: text/event-stream` header, asks for the stream (§4.3) |

An empty list, more than eight ids, a repeated id or an id that does not
parse: `422`.

The sessions served are the ones the chat send route serves for a stored
persona: a 1:1 text session and a date sub-session. A session that is not
found, is not the caller's, or is a group or a voice session gets the
refusal the send route gives for it. A bring-your-own-character session is
refused with `byoc_unsupported`, as the voice turn refuses it.

### 4.2 JSON response

`200`, with one item per requested id, in request order:

```json
{ "items": [
  { "message_id": "…", "text": "[whispers] 你怎么才来 [sighs] 我等好久了",
    "truncated": false, "model": "…", "usage": { … }, "generation_id": "…" },
  { "message_id": "…", "error": { "code": "not_speakable", "message": "…" } }
] }
```

An item has `text` or `error`, never both. The request succeeds whenever
it was well formed, even if every item failed.

| item error | when |
|---|---|
| `not_found` | no such message in this session |
| `not_speakable` | the message is not hers, or has no text to speak |
| `generation_failed` | every model hop failed, or the output was blank |

**A cut-off output** is handled as the voice turn handles one: a hop that
stops on its length limit or its content filter, or dies after text has
arrived, ends the item with the text so far and `truncated: true`; the
chain is not walked further. The caller decides whether to speak a
truncated line. The two response forms behave alike, so an item never
sends `delta`s and then an `item_error`.

### 4.3 Stream

Frames are the chat stream's (`data: <json>`, the type in `type`, the 15 s
`ping`). Items run concurrently and their frames interleave; each frame
names its item by `message_id`, the id of the source message.

```
delta…  → done            (one item, in order, tagged with its message_id)
item_error                (an item that failed; the stream goes on)
final                     (after every item has ended)
```

- `delta` and `done` are the voice turn's, with `message_id` set. `done`
  carries the item's whole `text`, `truncated`, `usage` and
  `generation_id`.
- **`item_error`** is new: `{ "type": "item_error", "message_id": "…",
  "code": "…", "message": "…" }`, with the codes of §4.2. It ends its item
  and nothing else. The existing terminal `error` frame keeps its meaning:
  the whole request died.
- No `meta` frame, as on the voice turn.

A client that streams text into its TTS model must wait for a tag's
closing bracket before sending it on; the engine does not align deltas to
tag boundaries.

*(Owner's rulings: both forms. Engine's calls: the path, the frame set,
`item_error`, the cap of eight, a well-formed request always answering
`200`.)*

## 5. What a message is judged from

For each item, in one model request:

| input | source |
|---|---|
| who she is | the persona's authored system prompt, as the voice prompt uses it |
| the directive | §6 |
| the tone | the row's `metadata.tone` (§5.1) |
| where the pair stands | the relationship line (§5.2) |
| what was being said | the four rows before the message in the session, as a plain transcript |
| the line to mark up | the message's stored text |

Nothing else: no profile, no memories, no character state, no events.

### 5.1 The tone

One sentence, present only for a non-default tone:

| `metadata.tone` | the sentence |
|---|---|
| `emotional` | `She said this in the grip of a feeling, unguarded: let it show in the delivery.` |
| `erotic` | the `emotional` sentence, then `She wants the other person: soft, slow, close, alluring.` |
| `focused` | `She said this with her mind on her work: brisk, even, brief.` |
| `default`, or no key | none |

`erotic` is a superset of `emotional` here as in the chat prompt.

### 5.2 The relationship line

The voice channel's relationship line, the same function and wording: a
bond half and a chemistry half, each present when the scope has an axis of
that line.

- The affinity is the one a chat turn in that session reads, decayed the
  same way; a date sub-session reads its parent's. It is the pair's
  standing now; the engine keeps no reading per message.
- The scope is the one the row recorded (`metadata.affinity_scope`); a row
  with none reads the chat stream's default scope.

*(Owner's ruling: tone and affinity take part. Engine's calls: the four
inputs' exact form, four rows of context, the sentences, the scope read
from the row.)*

## 6. The directive

Three parts, in order.

### 6.1 The base

Built in, product-identity-free, overridable by
`[tasks.chat_speech].filter_prompt` as the voice directive is. It says: you
are preparing a line she already said, to be read aloud by a
text-to-speech voice; the transcript and the notes above are context, not
text to rewrite; answer with the prepared line and nothing else.

### 6.2 The mode

Appended by the engine, not overridable, chosen by `rewrite`:

- **`false` (default).** Keep every spoken word exactly as written, in the
  same order and language. Only add audio tags. Emoji, and narration in
  brackets or asterisks, are not spoken: drop them, or turn them into a tag
  where one fits.
- **`true`.** The line may be rephrased so that it sounds natural spoken
  aloud; what she means and how she talks stay hers.

The engine does not check the words against the source in either mode.

### 6.3 The tags

The audio-tag guidance of the voice channel: the bracket syntax, the
commonly supported tags, leave to improvise, tags in English whatever the
language spoken. The tag list exists once in the code and both channels
read it. This task uses it whether or not `[tasks.chat_voice]` has
`tts_audio_tags` on, and whether or not that block exists.

The model's output is trimmed and returned as it is: no parsing, no tag
validation, no output filter.

How a marked-up line sounds is judged by listening; no test asserts on it.

## 7. Configuration

```toml
[tasks.chat_speech]
model       = "vendor/fast-model"
max_tokens  = 400
temperature = 0.7
#filter_prompt = """…optional override of the built-in base directive…"""
```

Resolved as `[tasks.chat_voice]` is: a single fixed model id (anything
else fails the boot), the outage fallback chain walked by the engine, the
usual sampling keys. Absent: the feature is off. The shipped examples
carry the block commented out. `max_tokens` has to leave room for the
source line plus its tags; too low a value shows up as `truncated` items.

## 8. Cost and limits

One model call per item, so a request of eight costs eight calls. A
repeated request converts again and may word the cues differently.

A request holds one of the caller's concurrent-stream slots for its whole
length, in either response form, under the limit the voice turn uses.

## 9. Tests

- The route: the `422` cases; `501` with the task absent; a group, a
  voice and a bring-your-own-character session refused; a date
  sub-session served under its parent's affinity; item order in the JSON
  form.
- Items: `not_found` for an id of another session; `not_speakable` for a
  user row and for a row with no text; `generation_failed` for a failed
  chain and for a blank output; one failing item leaves the others' texts;
  a cut-off output comes back as `text` with `truncated: true`.
- The request sent to the model: the persona head, the base directive, the
  mode sentence for each value of `rewrite`, the tag guidance; the tone
  sentence for each of the four values and none for an absent key; the
  relationship line under the row's recorded scope; exactly four earlier
  rows, fewer at the start of a session; the source text.
- A custom `filter_prompt` replaces the base and keeps the mode and tag
  parts.
- The stream: `delta` and `done` carry the source `message_id`; an
  `item_error` does not end the stream; `final` comes last.
- Nothing is written but the generation rows: the message rows, the
  affinity and the session are as before.
- Config: a non-fixed model fails the boot; the shipped example passes.

## 10. Order of work

After the `erotic` tone has merged. One pull request at a time into `dev`:
the code, then the docs (`docs/api-reference.md` + `.zh`,
`docs/model-config.md` + `.zh`, the two example config files). The change
is additive and off by default, so there is no migrating note.

A web-side issue lists the route, the task block and the frame.

## 11. Decisions

Ruled by the owner:

- A new task that shares the voice channel's machinery; input is stored
  chat messages, never free text; tone and affinity take part.
- A list of ids, one model call each.
- Words kept by default; a switch allows rephrasing.
- The speech text is not stored.
- JSON and SSE.

Calls made in this document, open to correction:

- Names: `[tasks.chat_speech]`, the path, `rewrite`, `item_error`.
- `rewrite` is a request field, not a deployment key.
- Her messages only; 1:1 text sessions and date sub-sessions; group,
  voice and bring-your-own-character sessions refused.
- A cut-off output is returned with `truncated: true`, as on the voice
  turn.
- Model hops are recorded in `engine.llm_generations`; the speech text is
  not stored.
- The cap of eight ids; four rows of context.
- The persona's authored head is part of the request.
- The tone sentences, and the affinity read as it stands now under the
  row's recorded scope.
- The base directive is overridable; the mode and the tag guidance are
  not.
- No check of the words in either mode; no audit table of its own.
- A well-formed request answers `200` with per-item errors.

## 12. Left to the builder

- Rust names; how the tag guidance is split so that the voice directive's
  output does not move and the list still lives once.
- The exact wording of the base directive and the two mode sentences.
- How the request to the model is laid out between the system prompt and
  the user message.
- Which existing refusal codes the session checks reuse.
- How much of the voice turn's hop walker and frame plumbing is shared by
  extraction and how much by a second call site.
