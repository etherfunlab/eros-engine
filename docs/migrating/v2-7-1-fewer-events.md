# Migrating to v2.7.1: fewer life events

**Applies to:**
- callers of `POST /v2/comp/session/{session_id}/events` that list kinds in `kinds` (a list, `allow` or `disallow`);
- clients that keep their own catalog of kinds (labels, icons, categories);
- personas whose `art_metadata.prefer_events` names a kind.

No migration. A request that names a deleted kind in `kinds` answers `422`, so such a caller must change before the new image serves.
**Design:** [`2026-11-16-prune-life-events-design.md`](../superpowers/specs/2026-11-16-prune-life-events-design.md)

Seven kinds are deleted: `grocery_shopping`, `in_class`, `watching_stocks`, `trading`, `bumped_someone` (`short`), `weather_change` and `watching_match` (`mid`). 63 kinds remain drawable. Two kinds now need a trip, and two fit only a modern persona. The rules are in [life-events-needs.md](../life-events-needs.md#life-events).

## What users will notice

- **The seven never come up.**
- **`on_the_way` and `on_the_road` come up only on a trip:** while a `business_trip`, `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` is under way, as `missed_ride` already did.
- **A `fantasy` persona never draws `temp_work` or `scolded_superior`.** Her clashes with someone above her come as `hit_someone`.
- **The remaining kinds come up more often.** How often an event happens is still the caller's call; a draw always picks a kind, so the deleted kinds' share goes to the rest of their category: roughly a quarter more for each remaining `short` kind and a third more for each remaining `mid` kind. To draw fewer events, lower your own draw rate.

## What to change

- **Drop the seven from every `kinds` list, before the upgrade.** A request naming one, in a list, `allow` or `disallow`, fails with `422` on the new image. A list without them works against both the old and the new image.
- **Drop the seven from your own catalog** (labels, icons, categories). Either order is safe: a client that drops kinds it does not know shows no event for one the old image still draws, and an unused catalog entry is harmless.
- **`prefer_events` naming `weather_change` or `watching_match`** is ignored; the persona's other preferred kinds still count. Drop the names when convenient.

## What to check

- **An event of a deleted kind under way at the upgrade** drops out of `…/effects` and no longer holds back the draw; the message it sent stays in the chat. Stored rows need nothing.

## Rollback

The `v2.7.0` image draws the seven again and draws the commute kinds outside a trip. Rows it writes of a deleted kind read as no event once you roll forward again. Restore any `kinds` lists that named the seven only if you need them on the old image.
