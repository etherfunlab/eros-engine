# Migrating to v2.3.0: life events and bring your own character

**Applies to:** clients of `eros-engine` upgrading from `v2.2.2` to `v2.3.0`,
operators rolling it out, and code that depends on the `eros-engine-core` or
`eros-engine-store` crates.
**Design:** [`2026-10-16-life-events-offline-and-scares-design.md`](../superpowers/specs/2026-10-16-life-events-offline-and-scares-design.md),
[`2026-10-17-life-events-run-ins-and-kind-filters-design.md`](../superpowers/specs/2026-10-17-life-events-run-ins-and-kind-filters-design.md),
[`2026-10-16-byoc-design.md`](../superpowers/specs/2026-10-16-byoc-design.md)

## Life events

**Ten new kinds are drawn by default.** With `kinds` absent,
`POST /v2/comp/session/{session_id}/events` draws from 38 kinds instead of 28:

| kinds | what a client sees |
|---|---|
| `busy`, `in_battle`, `asleep` | offline, as `away_from_phone`: while they last her 1:1 text turns ghost, except turns that carry an `action` or `image.force = true`, and `/open` returns `"greeting": null`. `busy` and `in_battle` are announced in a message at intimacy rung 2 and up; below that, and always for `asleep`, the event comes back with `"message": null` |
| `period_scare`, `pregnancy_scare` | a female persona only, at rung 2 and rung 3; told as a scare, resolved as a false alarm in her next message in that session after it ends |
| `got_bumped`, `bumped_someone`, `quarrel`, `scolded_by_superior`, `scolded_superior` | ordinary incidents, told in a message |

The offline kinds follow the same ghost gates as `away_from_phone`: none is
drawn with the deployment's ghosting switch off or in a session with fewer
than 10 user messages. A client that switches on `kind` handles the ten new
values. Every kind is in
[api-reference.md](../api-reference.md#post-v2compsessionsession_idevents).

**`kinds` takes an object.** Besides an array (an allowlist), `kinds` accepts
`{"allow": […], "disallow": […]}`: what `allow` allows (absent: every kind)
minus what `disallow` lists. `{"disallow": ["sudden_desire",
"pregnancy_scare"]}` keeps the two kinds the docs mark as adult themes away from
users who should not get them while every other kind, including kinds added
later, is still drawn. `v2.2.2` rejects the object form with `422`, so a caller that
wants none of the new kinds before it switches sends an array of the 28 old
kinds until the new image serves, then moves to `disallow`.

**`EventView.offline`** is new: `true` for the four offline kinds. It appears
on the events route and on `event` / `background_event` of
`GET /v2/comp/session/{session_id}/effects`.

**`GET …/effects` gains `changes_at`:** the earliest time the response changes
with no new turn and no new event (a need's next drift step, or the end of an
event under way), or `null`. A client that caches the response re-reads it then,
after a turn and after an event draw.

## Bring your own character

BYOC is off unless the deployment sets `BYOC_CALLER_SECRET`; with it unset,
nothing below applies. With it set, a user can hold a relationship with a
persona instance that has no genome ([byoc.md](../byoc.md)):

- `genome_id` on an item of `GET /bff/v1/comp/affinities/{user_id}` is `null`
  for such an instance. Every item now carries `instance_id`; a client that
  keyed the list by `genome_id` keys it by `instance_id`.
- `persona_name` on `POST /comp/chat/start` and `POST /bff/v1/comp/chat/start`
  is `null` for such an instance.

## Crates

`eros-engine-core`:

- `persona::PersonaGenome.id` and `persona::PersonaInstance.genome_id` are
  `Option<Uuid>`.
- `events::EventKind` has ten more variants, which breaks an exhaustive
  `match`; `EventKind::ALL` has 38 entries. New: `EventKind::is_offline`,
  `EventKind::is_false_alarm`, `events::is_told`.
- New: `needs::next_change`.

`eros-engine-store`:

- `persona::InstanceGate.genome_id` is `Option<Uuid>` and
  `InstanceGate.genome_name` is `Option<String>`.
- `affinity::UserAffinity.genome_id` is `Option<Uuid>`.
- `group::MemberRefusal` has a `Byoc` variant.
- `events::EventRepo::unexplained_away` is renamed `unexplained_offline` and
  covers every offline kind; `unresolved_scare` is new.
- New: `persona::ByocInstance`, `PersonaRepo::create_byoc_instance`,
  `PersonaRepo::load_byoc_instance`, `memory::ProfileMemoryChange` and
  `MemoryRepo::list_profile_changes`.

## Rollout and rollback

Three migrations: `0073` removes `metadata.event` and `metadata.event_key` from
v1 event messages and drops `chat_messages_event_key_uidx` (announced in the
v2.2.2 guide), `0074` adds an index for the profile-memory export, and `0075`
makes `persona_instances.genome_id` nullable and adds `byoc_key`. Run `migrate`,
then serve the new image.

To roll back, deploy the `v2.2.2` image without running its `migrate`. It
reads an event of a kind it does not know as absent: it does not go silent for
`busy`, `in_battle` or `asleep`, does not resolve a scare, and may draw inside
such an event's window. If any BYOC instance exists, it answers `500` on
`GET /bff/v1/comp/affinities/{user_id}` for a user with a BYOC relationship,
and treats the instance as missing (`404`) everywhere else. No image older than
`v2.2.2` runs on a database past `0073`: the v1 event metadata and its
idempotency index it relies on are gone.
