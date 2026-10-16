# Migrating to the v2.2.2 life events

**Applies to:** clients of `eros-engine` upgrading to `v2.2.2`, and code that
depends on the `eros-engine-core` or `eros-engine-store` crates.
**Design:** [`docs/superpowers/specs/2026-10-15-life-events-v2-design.md`](../superpowers/specs/2026-10-15-life-events-v2-design.md)

**New kinds are drawn by default.** With `kinds` absent,
`POST /v2/comp/session/{session_id}/events` can now draw any of 28 kinds, 23 of
them new: among them `away_from_phone`, `hit_someone`, `got_hit`, `breakup`,
`hospitalized` and `post_surgery`. A caller that wants to bring them in
gradually sends `kinds` with the kinds it allows before upgrading. The kinds,
their categories and their lengths are in
[api-reference.md](../api-reference.md#post-v2compsessionsession_idevents).
`police_encounter` now lasts 30 minutes instead of an hour.

**Backgrounds.** `body` and `trip` kinds last days. While one is under way, an
incident can still be drawn; in v2.2.1 nothing was drawn while any event was
under way.

**`EventView`.** `category` is new: `short`, `mid`, `body` or `trip`.
`message_id` is nullable. A drawn `away_from_phone` writes no message, so its
response is `"event": {…, "message_id": null}` with `"message": null`. Both
`null` still means nothing was drawn. A client that took a non-null `event` to
mean a message was written checks `message` instead.

**Silence while away.** While an `away_from_phone` lasts, every 1:1 text turn
of that persona instance ends as a ghost turn (`meta(action_type=ghost)` →
`done` → `final`), and `/open` returns `"greeting": null`. A turn that carries
an `action` or `image.force = true` is answered as before. The kind is never
drawn with the deployment's `ghosting = false`, nor in a session with fewer than
10 user messages. Afterwards the persona's next message in a session she ghosted
explains it. A client that wants to show her as away reads `event` on the
effects route.

**Effects.** `GET /v2/comp/session/{session_id}/effects` gains
`background_event`: the background under way now, in the same shape as
`event`, or `null`.

**Persona settings.** Three optional `art_metadata` keys shape the draw:
`prefer_events` (an array of `mid` kinds), `health` (`"weak"` | `"normal"` |
`"strong"`) and `prefer_trip` (bool). `period` is drawn only for a persona whose
`art_metadata.gender` is `"female"`. Absent keys keep the default weights.

**`eros-engine-core`.** In `events`:

- `draw(&DrawInput, Rolls) -> Option<Drawn>` replaces
  `draw(&DrawInput, f64) -> Option<EventKind>`. `Rolls { category, kind,
  duration }` are three uniform draws in `[0, 1)`; `Drawn { kind, lasts }`
  carries the drawn length.
- `DrawInput` gains `persona: &PersonaTraits`, built from `art_metadata`.
- `duration()` and `longest_cooldown()` are removed. A length is drawn, and
  `lookback()` is how far back `history` must reach.
- `EventKind` gains 23 variants, `category()` and `is_background()`;
  `EventCategory`, `Health` and `PersonaTraits` are new. A `match` on
  `EventKind` must cover the new variants.

**`eros-engine-store`.** The event methods on `ChatRepo` (`insert_event_message`,
`event_message`, `recent_events`, `current_event`) and `chat::EventRecord` /
`chat::CurrentEvent` are removed. The new `events` module holds `EventRepo`
(`insert`, `by_key`, `recent`, `current`, `unexplained_away`), `EventRecord`,
`CurrentEvents`, `NewEvent` and `Inserted`.

**Store.** Migration `0072` creates `engine.persona_events` and backfills it
from every v1 event message (`metadata.proactive = 'event'`). New event
messages keep `metadata.proactive = 'event'` and no longer carry
`metadata.event` or `metadata.event_key`. Anything that reads events from
`engine.chat_messages` directly reads `engine.persona_events` instead. The
backfill casts each v1 `metadata.event.ends_at` to `timestamptz`; a row the cast
rejects fails the migration, which then rolls back whole.

**Rollout.** Run `migrate`, then serve the new image. To roll back, deploy the
v2.2.1 image without running its `migrate`. It keeps reading and writing v1
events in message metadata, which `0072` leaves in place, but it does not see
events v2.2.2 wrote: no silence for an away stretch under way, no backgrounds,
and it may draw again inside a v2.2.2 event's window. Events it writes during
the rollback live only in that metadata, and a later roll-forward does not see
them. The next release removes `metadata.event` and `metadata.event_key` from
v1 rows and drops `chat_messages_event_key_uidx`.
