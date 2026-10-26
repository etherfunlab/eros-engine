# Migrating to v2.3.1: `having_sex`, follow-on factors and group pictures of two

**Applies to:** clients of `eros-engine` upgrading from `v2.3.0` to `v2.3.1`,
operators rolling it out, and code that depends on the `eros-engine-core` crate.
**Design:** [`2026-10-25-effects-as-of-and-three-kinds-design.md`](../superpowers/specs/2026-10-25-effects-as-of-and-three-kinds-design.md),
[`2026-10-25-life-events-follow-ons-and-persona-keys-design.md`](../superpowers/specs/2026-10-25-life-events-follow-ons-and-persona-keys-design.md),
[`2026-10-26-group-member-scopes-and-last-message-design.md`](../superpowers/specs/2026-10-26-group-member-scopes-and-last-message-design.md),
[`2026-10-26-group-two-person-image-design.md`](../superpowers/specs/2026-10-26-group-two-person-image-design.md)

## Life events

**`having_sex` is drawn by default, and the v2.3.0 adult-theme list no longer
covers it.** The v2.3.0 guide recommended `{"disallow": ["sudden_desire",
"pregnancy_scare"]}` for users who should not get adult themes. `having_sex`
is a third adult-theme kind: `mid`, 1 to 2 hours, offline for every persona,
never announced, and explained afterwards in as many words (`- 刚才：在做爱，
对方那时发来的消息你现在才看到`). It has no intimacy gate. A caller that fences
adult themes adds it before the new image serves:

```json
{"kinds": {"disallow": ["sudden_desire", "having_sex", "pregnancy_scare"]}}
```

`v2.3.0` rejects an unknown kind in `kinds` with `422`, so send the three-item
list only once `v2.3.1` is serving; until then the two-item list is all the
old image accepts, and the new image draws `having_sex` for those users.

**Six more kinds** are drawn with `kinds` absent: `admiring_scenery`,
`admiring_art`, `taking_selfie` (`short`) and `on_the_road`, `watching_match`,
`watching_show` (`mid`). All are told in a message; `taking_selfie` is words
only, no image is composed. A client with a strict enum over `EventKindDto`
adds the seven new values.

**`pregnancy_scare` weighs a fifth** while the persona instance has never had a
`having_sex` event. A deployment that disallows `having_sex` keeps the scare,
at a fifth of its former weight. Nothing to do; worth knowing when a count
drops.

**Follow-on factors** make a kind three times as likely while an event that
leads to it is under way or ended within 48 hours
([life-events-needs.md](../life-events-needs.md#follow-on-factors)). No
request changes.

**Two persona-card keys**, `is_virgin` and `homesick`, are read from
`art_metadata` like `prefer_trip`
([life-events-needs.md](../life-events-needs.md#persona-settings)). A card
without them behaves as before.

## `GET …/effects`

`as_of` is new: the server time the response was computed for. Schedule the
re-read by `changes_at - as_of` from receipt instead of comparing `changes_at`
with the device clock. The old scheduling still works.

## Group chat

**`member_scopes`** on the group send is optional; absent, every speaker runs on
the engine defaults as before. **`last_message`** is new on the group DTO and
`null` for a group with no visible row.

**A picture of two.** The `image_request` frame and
`GET …/messages/{message_id}/image-request` may carry `companion:
{member_session_id, instance_id}` on a group image turn; a client that ignores
it draws the speaker alone, as before. The group director's strict schema now
requires `with` (a seated handle or `null`) on every speaker entry, and the
engine-owned director message explains it; an operator's `filter_prompt` needs
no change, but the shipped example shows `"with": null` in the output shape.
The built-in composer prompt (`DEFAULT_COMPOSE_PROMPT`) gained one sentence
about the optional `[同框人物]` section, on the 1:1 path too; a custom
composer prompt should depict both people when the section is present
([model-config.md](../model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns)).

## Crates

`eros-engine-core`: `EventKind` gains seven variants (breaks exhaustive
matches); `PersonaTraits` gains `is_virgin` and `homesick`, `DrawInput` gains
`ever_had_sex`, `group::Cue` gains `with` and `group::DirectorEntry` gains
`with` (break struct literals; `PersonaTraits` is `Default`).
