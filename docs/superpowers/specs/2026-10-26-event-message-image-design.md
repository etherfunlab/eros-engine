# Life events: a selfie that carries its picture — Design

- **Date:** 2026-10-26
- **Status:** Approved
- **Type:** Engine change. One migration (widens a CHECK). Additive API
  changes on `POST /v2/comp/session/{session_id}/events`: two optional
  request fields (`image`, `outfit`) and one optional response field
  (`image_request`). A request with neither new field behaves exactly as
  today.
- **Owner:** enriquephl (sole dev)
- **Issue:** #437
- **Builds on:** `2026-10-25-effects-as-of-and-three-kinds-design.md` (where
  `taking_selfie` was added as words only), the proactive message path
  (`pipeline/proactive.rs::greet`), and the delegated image path
  (`[tasks.chat_image_prompt_compose]`, the `metadata.image` marker,
  `chat_images_events`, `GET …/messages/{id}/image-request`).

## 1. Why

A life event is told in one proactive message, and that path emits text only.
`taking_selfie` is a kind that is about a photo, yet all she can do is say
she took one. A chat turn can already hand the client a composed picture
(`image_request`); the event path has no equivalent. This change lets an
event message carry the same image request, starting with `taking_selfie`.

Image generation stays downstream, as everywhere else: the engine composes
the prompt and the client draws.

## 2. Goals, non-goals, anti-goals

**Goals**

- A `taking_selfie` event message carries a picture: her words plus one
  delegated image request, the same shape a chat turn's `reply_text_image`
  produces.
- The picture matches what she says. Her message is written first; the
  composer reads it as the scene.
- She can wear what the caller dresses her in, as on a chat turn, so an
  event selfie need not change clothes from the picture before it.
- The stored message is a normal image row: history flags it (`image: true`),
  the `image-request` restore route recovers it, and later turns see
  `[你的照片：{caption}]` in her history line.

**Non-goals**

- Other kinds. `admiring_scenery`, `admiring_art`, `saw_bag`, `saw_clothes`
  would be pictures of a thing, not of her; `compose_image_prompt` always
  layers in her appearance, so they need a different assembly. Not here.
- Pictures on `/open` greetings, life-choice messages, or group chat.
- Any change to how `taking_selfie` is drawn (weight, cooldown, length,
  where it may be drawn).
- A cap on how often event selfies carry a picture. Default-open; the
  readings in §8 are what a later cap would be judged on.

**Anti-goals**

- A deployment that does not opt in sees any change. A request with neither
  `image` nor `outfit` is byte-identical to today: words-only message,
  today's cue, today's prompt.
- The text promising a photo that the engine never composed. The cue only
  says she sent it when a picture is actually being composed (§4).

## 3. Opt-in: the request's `image` object

Same convention as a chat turn (`image_capability_available`): a picture is
possible only when **both** hold —

1. the request carries an `image` object (the client saying "I handle
   images"), and
2. `[tasks.chat_image_prompt_compose]` is configured.

`DrawEventRequest` gains:

```json
"image": {
  "style": "realistic",        // optional, StyleKey; default realistic
  "aspect_ratio": "3:4",       // optional
  "prompt_variant": "a"        // optional, as on a chat turn
}
```

The three fields mean what they mean on a chat turn's `image`. There is no
`force`: the kind decides whether a picture is wanted. Whether this is a new
struct or `ImageReplyParams` with `force` ignored is the builder's call; the
OpenAPI schema should not advertise `force` on this route.

When either condition fails, the event is told exactly as today.

## 3a. `outfit`

`DrawEventRequest` gains a top-level `outfit`, with the chat request's
semantics and validation: the outfit she wears, typically one of her wardrobe
entries, not checked against it; 1–64 chars after trim; blank reads as
absent. It is `plan.clothing` for the event, so, exactly as on a chat turn,
it reaches both the reply prompt (her words can follow it) and the
composer's `[此刻穿着]` and the audit `inputs.clothing`.

It applies to any told kind, with or without `image`. Absent: no clothing
line in the reply prompt (as today) and an empty `[此刻穿着]`; there is no
judge on this path to pick from the wardrobe, so the composer infers her
clothes from the scene.

## 4. Order of work for a `taking_selfie` draw with a picture

1. **Text.** `greet` generates her message as today, with one change: the
   cue states that she sent the photo.
   - with a picture: `（你刚拍了一张自拍，发给了对方。）`
   - without: unchanged, `（你刚拍了一张自拍，想发给对方看看。）`
   - A background under way still opens the cue as today
     (`（你正在短途旅行。你刚拍了一张自拍，发给了对方。）`). This is how the
     scene becomes persona- and situation-aware: the background, the
     `[now]` time, her persona and the recent chat all reach the text model,
     and she says where she is.
2. **Blank text** (empty after layer 0): nothing is written, as today, and
   the composer is never called.
3. **Compose.** Run the chat composer (`build_delegated_image_prompt`'s
   machinery) with:
   - `[人物外观]`: her appearance, as on a chat turn.
   - `[此刻穿着]`: the request's `outfit` (§3a), else empty.
   - `[身体状态]`: the given-item effects text a chat turn would use at this
     instant (`load_effects` → `prompt::body_state`); empty when none.
   - `[最近场景]`: the recent transcript (the same line format the judge
     transcript uses, newest-N with no anchor row) with **her new message
     appended as the last line**.
   - `[对方最新消息]`: empty. The user did not ask for this picture.
   - `[风格]` / `[画幅]` / variant: from the request's `image`.
   - `image_ref`: `face`.
   Fail-open exactly like a chat turn: a failed chain yields the generic
   persona-portrait prompt, which for a selfie is still the right picture.
4. **Persist.** The marker (`build_delegated_image_marker`: subject, caption,
   aspect ratio, compose audit trio, `compose_event_id`, `image_ref: face`)
   goes into the `ProactiveInsert` metadata under `image`, so the message,
   its marker and the event row land in the one existing insert. No second
   write.
5. **Lost insert.** If the event insert is refused (another event under way,
   a concurrent call won the key), the composer call and its audit row are
   already spent and point at no message. Accepted, as on a chat turn whose
   picture never ships.

Both LLM calls run under the per-user stream slot the route already holds.
The route is not streamed; the composer adds one serial hop to its latency.

## 5. Response

`DrawEventResponse` gains a sibling of `message`:

```json
{
  "event":   { … },
  "message": { "message_id": "…", "content": "…", "sent_at": "…", "occasion": "event" },
  "image_request": {
    "message_id": "…",
    "composed_prompt": "<base64>",
    "image_ref": "face",
    "aspect_ratio": "3:4"
  }
}
```

- The type is the restore route's `ImageRequestPayloadResponse`, same fields
  and encoding (`composed_prompt` is base64 of the UTF-8 wire prompt).
- It is absent when the message carries no picture, so a response without
  one keeps today's shape.
- It is rebuilt from the stored row in `respond()`, by the same logic as
  `GET …/image-request` (extract it into one shared function). So a replay
  of the same `event_key` returns it too, and the composed prompt keeps its
  one home on the compose audit row.
- A row whose compose audit write failed (fail-open) has the marker but no
  recoverable prompt: `image_request` is absent, matching the restore route's
  404 for the same row.

`GreetingDto` is unchanged.

## 5a. Editing an event selfie

`POST …/message/{mid}/image/edit` accepts the row like any image row. An
event message has no originating user message, so an edit without
`persist_instruction` returns the existing 409 ("source image turn has no
originating user message"); with `persist_instruction: true` it works as on
any other row. `routes/image_edit.rs` logic is unchanged. Its comment that
no engine path produces such a row is now false and is corrected, and the
api-reference edit section states the requirement for event pictures.

## 6. Audit

`chat_images_events` gains the source `event`. Migration `0078` widens
`chat_images_events_source_check` the way `0057` did. The kind is not
repeated on the audit row: it is reachable through the message's
`persona_events` row. The audit `inputs` snapshot is the usual seven slots.

Composer failures go where the chat `reply_text_image` path puts them; the
builder matches that path.

## 7. Docs

- `docs/api-reference.md` + `.zh.md`: the events route's request and response
  (`image`, `outfit`, `image_request`); the kind table's `taking_selfie` row
  (a picture when the request carries `image`); the image-edit section's
  note that an event picture needs `persist_instruction: true` (§5a).
- `docs/life-events-needs.md` + `.zh.md`: the "Sights, shows and a selfie"
  paragraph's "words only" sentence.
- Wherever docs list the composer's call sites or the `chat_images_events`
  sources.
- A migrating note for the release that ships this: a client that wants the
  picture sends `image` on `/events` and renders `image_request` the way it
  renders a turn's; a client that dresses her on chat turns sends the same
  `outfit` on `/events`; editing an event picture needs
  `persist_instruction: true`.
- Regenerate `crates/eros-engine-server/openapi.json`.

## 8. Readings

Rows in `chat_images_events` with `source = 'event'`, joined through the
message to `persona_events`, are the record of how often event selfies carry
a picture, how often the composer fails, and what it draws. They are what a
later decision to gate this (rung, cooldown) would be made from.

## 9. Left to the builder

- The exact Rust shape: how `Occasion::Event` learns it has a picture, how
  `kind_cue` receives that, whether `greet` returns the image payload or
  `respond()` alone rebuilds it.
- The transcript fetch for `[最近场景]` (window size, helper reuse).
- The request struct for `image` (§3).
- Tests. At least: a `taking_selfie` draw with `image` and a configured
  composer returns `image_request` and stores the marker; with neither
  `image` nor `outfit` the request to the model is byte-identical to today;
  a blank text calls no composer; a replayed key returns the same
  `image_request`; another kind with `image` gets no picture; `outfit`
  reaches the reply prompt, `[此刻穿着]` and the audit `inputs.clothing`;
  an edit of an event picture without `persist_instruction` returns 409.
