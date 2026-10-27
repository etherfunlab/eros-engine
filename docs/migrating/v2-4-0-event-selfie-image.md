# Migrating to v2.4.0: a selfie's picture on `/events`

**Applies to:** clients of `eros-engine` upgrading from `v2.3.1` to `v2.4.0` that
draw life events or render pictures.
**Design:** [`2026-10-26-event-message-image-design.md`](../superpowers/specs/2026-10-26-event-message-image-design.md)
**Sibling guide for this release:** [`v2-4-0-insights-2-0.md`](v2-4-0-insights-2-0.md)

A `taking_selfie` life event can now carry her picture. Nothing changes for a
client that does not opt in: without an `image` object on the request, or on a
deployment without `[tasks.chat_image_prompt_compose]`, the event is told in
words exactly as in v2.3.1, and responses without a picture keep their old
shape.

## Opting in

- **`image` on `POST /v2/comp/session/{session_id}/events`:**
  `{style, aspect_ratio, prompt_variant}`, as on a chat turn. There is no
  `force`, and an unsupported `aspect_ratio` is a `422`. Only `taking_selfie`
  uses it; other kinds ignore it.
- **`outfit` on `/events`:** chat-turn semantics. It reaches her words and the
  composer's `[此刻穿着]` block.
- **Response:** an `image_request` field (the same payload as
  `GET …/image-request`), rebuilt from the stored row, so replaying the same
  `event_key` returns it again. The field is **omitted** when there is no
  picture, never `null`.
- The stored message is a normal image row: `metadata.image` is set, history
  lists it with `image: true`, and the restore route works on it.

## Other visible changes

- **Editing an event picture needs `persist_instruction: true`** — it answers
  no user message, so without the flag the edit keeps returning the existing
  `409`.
- **`chat_images_events.source` gains the value `'event'`** (migration 0078
  widened the CHECK). A consumer with a strict enum over that column adds it.
