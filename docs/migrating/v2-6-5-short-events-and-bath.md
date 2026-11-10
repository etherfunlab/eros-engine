# Migrating to v2.6.5: eight short events and the bath-together choice

**Applies to:**
- clients that show life-choice controls, draw event kinds, or filter adult kinds;
- persona authors who want the `hygienic` or `employed` keys;
- crate users who build `eros_engine_core::events::PersonaTraits` or match on `EventKind` or `LifeChoiceKind`.

No migration and no breaking wire change: the event and life-choice kind enums gain values. A client that does nothing keeps working (see [Unknown kinds](#unknown-kinds)).
**Design:** [small things in her day](../life-events-needs.md#small-things-in-her-day), [a bath together](../life-events-needs.md#a-bath-together) and [the life-choice route](../api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id).

## What users will notice

- **Eight more things happen in her day**, all short, all told, none offline: `bird_encounter`, `ex_wants_back`, `want_to_quit_job`, `pondering_life`, `streaking`, `want_to_see_sea`, `want_hot_spring`, `want_sauna`.
  - `ex_wants_back` needs intimacy rung 2.
  - `streaking` needs rung 3 and a romance-compatible pair, as `strip` does. It carries adult themes.
- **She can ask the user to take a bath with her** (`bathe_together`). It needs a romance-compatible pair, chemistry at tier 3 or more (tier 2 for a persona whose `hygienic` is `true`; never when it is `false`), and at least 3 of the last 7 judged turns with warmth 0.5 or more.
  - Accepting starts a 30-minute `bathing_together` event. She keeps replying.
  - Declining drops chemistry one tier, as for `sleep_together`. A typed reply costs nothing.
  - An accepted or ignored ask is not repeated for 7 days.
  - It is independent of `sleep_together` and of a trip under way; she still has only one ask pending at a time.
  - The bath stays out of her world story.

## Clients

1. **Add the `bathe_together` choice kind** wherever you draw life-choice controls: the ask message, its accept and decline buttons, and any tray of pending choices. The answer route is unchanged (`POST /v2/comp/session/{session_id}/life-choices/{choice_id}` with `accept` or `decline`). An accept returns the bath as `event`.
2. **Add the `bathing_together` event kind** to your event catalog: its label and icon. It is a `short` incident the engine places itself. It is never drawn, and naming it in an `/events` `kinds` filter is a `422`.
3. **Add the eight new kinds** to your event catalog.
4. **Add `streaking` to any adult-content `disallow` list.** A `disallow` list leaves every kind it does not name in, so without this `streaking` reaches users you keep `strip` from. A caller that allow-lists adds the kinds it wants.

### Unknown kinds

A client that knows none of the new kinds degrades safely, as it did for `travel_together`:
- Her ask arrives as an ordinary proactive message.
- A typed reply leaves the choice ignored, which costs nothing.
- The new events and the bath show wherever unknown event kinds show.

## Persona authors

Two optional `art_metadata` keys, part of [character card 1.1](../eros-character-v1.1.md):

- `hygienic` (boolean): `true` makes showering, bathing, hot springs and saunas three times as likely and lets her ask for a bath at chemistry tier 2; `false` makes them a third as likely and she never asks.
- `employed` (`"free"`, `"part_time"`, `"full_time"`): scales the work kinds (`temp_work`, `scolded_by_superior`, `scolded_superior`, `business_trip`) a third, one or three times; a `free` persona never draws `want_to_quit_job`. Absent or any other value reads as `part_time`.

An existing persona with neither key draws as before, apart from the eight new kinds: they join the `short` pool, so each existing `short` kind comes up somewhat less often within it. The category's share is unchanged.

## Crate users

- `PersonaTraits` gains `hygienic: Option<bool>` and `employed: Employment`. A struct literal adds them, or ends with `..Default::default()`.
- `EventKind` gains nine variants and `LifeChoiceKind` gains `BatheTogether`: an exhaustive `match` adds the arms.
