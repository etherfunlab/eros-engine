# Migrating to v2.7.2: vehicles on a trip, four actions, four species

**Applies to:**
- clients that keep their own catalog of event kinds (labels, icons, categories) or of action types;
- callers of `POST /v2/comp/session/{session_id}/events` that list kinds in `kinds`;
- personas whose `art_metadata.prefer_events` names `on_the_road`;
- personas whose `art_metadata.species` is `cyborg`, `robot`, `android` or `zombie`.

No migration. Everything new is additive on the wire; two behavior changes are below.
**Design:** [`2026-11-19-vehicles-actions-species-design.md`](../superpowers/specs/2026-11-19-vehicles-actions-species-design.md)

Six event kinds are added, all `mid` and drawn only while a trip is under way: `on_a_boat`, `on_a_cruise`, `on_a_bus`, `on_a_train`, `on_a_plane`, `driving`. 69 kinds are drawable. Four action types are added: `wink`, `blink`, `ask_for_dance`, `ask_dancing`. The rules are in [life-events-needs.md](../life-events-needs.md#on-the-move) and [api-reference.md](../api-reference.md).

## What users will notice

- **On a trip she says how she is travelling.** A modern persona on a trip sends "on a train", "on a plane", "driving" and so on instead of "on the road". The cruise never comes up on a business trip.
- **`on_the_road` comes up only for a `fantasy` persona.** A modern persona never draws it; a fantasy persona draws it and `on_a_boat`.
- **A robot or android persona** reads as a machine: her identity line says 机器人 or 仿生人, the prompt calls her no AI assistant rather than no AI, and she never has a period, a late-period scare or a pregnancy scare. `cyborg` and `zombie` get their Chinese labels (半机械人, 丧尸) and nothing else changes for them.
- **An accepted `ask_dancing`** comes with a picture of her dancing when the turn carries `image` and the deployment has the composer, the same way an accepted `photo` does.

## What to change

- **Add the six kinds to your catalog** before or with the upgrade: an engine on the new image draws them for any caller that does not list `kinds`. A client that drops kinds it does not know shows nothing for them.
- **Add the four action types** if your client offers them. None carries a field.
- **`prefer_events` naming `on_the_road`** does nothing for a modern persona now; name one of the vehicles instead if the persona should travel often.

## What to check

- **An `on_the_road` under way at the upgrade** runs out as it is.
- **A `period` under way for a robot or android persona** runs out as it is; no new one is drawn.
- **Stored rows** need nothing: `on_the_road` still parses everywhere.

## Rollback

The `v2.7.1` image draws `on_the_road` for every persona and none of the vehicles. A vehicle event written by the new image reads as no event on the old one, and an action of a new type on a stored row reads as no action. Requests that send a new action type answer `422` on the old image.
