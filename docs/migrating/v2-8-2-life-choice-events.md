# Migrating to v2.8.2: her accepted bed and bath asks place their events

**Applies to:**
- clients that read `event` on the life-choice answer, in the JSON response or the SSE `resolved` frame;
- clients that assume a `bathe_together` accept is always `bathing_together`.

No migration and no new setting. Both changes are to values a client may already handle: `event` was already an `EventView`, and both event kinds already exist.
**Design:** [`2026-11-23-sleep-together-accept-places-event-design.md`](../superpowers/specs/2026-11-23-sleep-together-accept-places-event-design.md)

The rules are in [the romantic choices](../life-events-needs.md#the-romantic-choices), [a bath together](../life-events-needs.md#a-bath-together) and [the life-choice route](../api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id).

## What users will notice

- **Her accepted `sleep_together` places sex together.** Before, accepting her ask to go to bed placed no event. Now it places a one-hour `having_sex_together` (和对方上床), the event the user's accepted `sex_together` places. The incident under way ends, and a background such as a trip keeps going. Her reply reads it as what she is doing now, and it counts as sex everywhere the draw reads that: her virginity ends, and a pregnancy scare becomes likelier while it is recent. Costs are unchanged: an accept costs nothing, and a decline drops chemistry one tier.
- **Her accepted `bathe_together` is a shower or a bath.** Before, it was always `bathing_together`. Now it is `showering_together` (和对方一起淋浴) or `bathing_together` (和对方一起洗澡), one or the other at even odds, 30 minutes either way. A shower gets its own reply cue, `（对方答应了和你一起洗澡，你们现在一起站在花洒下。）`.
- `propose` and `travel_together` are unchanged. Choices accepted before the upgrade keep what they placed then: no event is backfilled for an earlier `sleep_together` accept.

## Deployments

Nothing to run: no migration, no new setting. Deploy the image as for any release.

### Rollback

Roll back the image; there is no database change to undo. On the `v2.8.1` image, a `sleep_together` accept places nothing again and a `bathe_together` accept is always a bath. Rows the new image already placed stay valid and are read as before.

## Clients

1. **Read `event` after a `sleep_together` accept.** It is no longer `null`: it is a `having_sex_together` `EventView` with `offline: false`, in the JSON response and in the `resolved` frame. Show it as you show the event of any other accept.
2. **Accept `showering_together` from a `bathe_together` accept.** A client that labels or illustrates the bath by its kind needs the shower's label too.
