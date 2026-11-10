# Migrating to v2.6.5: sixteen short events, the bath-together choice and `having_sex` for sex addicts

**Applies to:**
- clients that show life-choice controls, draw event kinds, or filter adult kinds;
- persona authors who want the `hygienic` or `employed` keys, and anyone whose personas draw `having_sex`;
- crate users who build `eros_engine_core::events::PersonaTraits` or match on `EventKind` or `LifeChoiceKind`.

No migration and no breaking wire change: the event and life-choice kind enums gain values. A client that does nothing keeps working (see [Unknown kinds](#unknown-kinds)). One behaviour changes: `having_sex` is drawn only for a sex addict ([below](#having_sex-only-for-a-sex-addict)).

**Before you upgrade:** if you keep adult kinds from some users with a `disallow` list, add `streaking`, `just_have_sex` and `masturbating` to it first ([Clients, step 4](#clients)). Otherwise those users start seeing them as soon as the new version serves them.

**Design:** [small things in her day](../life-events-needs.md#small-things-in-her-day), [the market, getting lost, her habits and sex](../life-events-needs.md#the-market-getting-lost-her-habits-and-sex), [a bath together](../life-events-needs.md#a-bath-together) and [the life-choice route](../api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id).

## What users will notice

- **Eight more things happen in her day**, all short, all told, none offline: `bird_encounter`, `ex_wants_back`, `want_to_quit_job`, `pondering_life`, `streaking`, `want_to_see_sea`, `want_hot_spring`, `want_sauna`.
  - `ex_wants_back` needs intimacy rung 2.
  - `streaking` needs rung 3 and a romance-compatible pair, as `strip` does. It carries adult themes.
- **Eight more after those**, also short, told and never offline: `watching_stocks`, `trading`, `got_lost`, `drinking`, `smoking`, `smoking_weed`, `just_have_sex`, `masturbating`.
  - `watching_stocks` is for a `modern` persona only.
  - `got_lost` happens only on a trip: her own leisure trip or visit home, or the trip with the user.
  - `drinking`, `smoking` and `smoking_weed` need the persona's `alcoholic`, `smoking` or `drug` key to be `true`, and come up three times as often while her craving is high.
  - `just_have_sex` needs rung 3; `masturbating` needs rung 3 and `sex_addict: true`. Both carry adult themes.
- **She can ask the user to take a bath with her** (`bathe_together`). It needs a romance-compatible pair, chemistry at tier 3 or more (tier 2 for a persona whose `hygienic` is `true`; never when it is `false`), and at least 3 of the last 7 judged turns with warmth 0.5 or more.
  - Accepting starts a 30-minute `bathing_together` event. She keeps replying.
  - Declining drops chemistry one tier, as for `sleep_together`. A typed reply costs nothing.
  - An accepted or ignored ask is not repeated for 7 days.
  - It is independent of `sleep_together` and of a trip under way; she still has only one ask pending at a time.
  - The bath stays out of her world story.

### `having_sex` only for a sex addict

A persona without `sex_addict: true` (unset or `false`) no longer draws `having_sex`, so she no longer goes offline for it. She tells `just_have_sex` instead, from rung 3, after the fact. A persona with `sex_addict: true` draws both. Existing `having_sex` rows stay as they are; nothing is rewritten. Either kind ends an `is_virgin` persona's virginity.

## Clients

1. **Add the `bathe_together` choice kind** wherever you draw life-choice controls: the ask message, its accept and decline buttons, and any tray of pending choices. The answer route is unchanged (`POST /v2/comp/session/{session_id}/life-choices/{choice_id}` with `accept` or `decline`). An accept returns the bath as `event`.
2. **Add the `bathing_together` event kind** to your event catalog: its label and icon. It is a `short` incident the engine places itself. It is never drawn, and naming it in an `/events` `kinds` filter is a `422`.
3. **Add the sixteen new kinds** to your event catalog.
4. **Add `streaking`, `just_have_sex` and `masturbating` to any adult-content `disallow` list.** A `disallow` list leaves every kind it does not name in, so without this they reach users you keep `strip` and `having_sex` from. A caller that allow-lists adds the kinds it wants.

### Unknown kinds

A client that knows none of the new kinds degrades safely, as it did for `travel_together`:
- Her ask arrives as an ordinary proactive message.
- A typed reply leaves the choice ignored, which costs nothing.
- The new events and the bath show wherever unknown event kinds show.

## Persona authors

Two optional `art_metadata` keys, part of [character card 1.1](../eros-character-v1.1.md):

- `hygienic` (boolean): `true` makes showering, bathing, hot springs and saunas three times as likely and lets her ask for a bath at chemistry tier 2; `false` makes them a third as likely and she never asks.
- `employed` (`"free"`, `"part_time"`, `"full_time"`): scales the work kinds (`temp_work`, `scolded_by_superior`, `scolded_superior`, `business_trip`) a third, one or three times; a `free` persona never draws `want_to_quit_job`. Absent or any other value reads as `part_time`.

Existing keys do more:

- `sex_addict: true` is now what lets her draw `having_sex` and `masturbating`, and it makes `just_have_sex` three times as likely (`false`: a third). Set it on a persona who should keep going offline to have sex.
- `smoking`, `alcoholic` and `drug` set to `true` let her draw `smoking`, `drinking` and `smoking_weed`.

An existing persona draws as before apart from those keys and the new kinds: the sixteen join the `short` pool, so each existing `short` kind comes up somewhat less often within it. The category's share is unchanged.

## Crate users

- `PersonaTraits` gains `hygienic: Option<bool>`, `employed: Employment` and `habits: Habits`. A struct literal adds them, or ends with `..Default::default()`.
- `EventKind` gains seventeen variants and `LifeChoiceKind` gains `BatheTogether`: an exhaustive `match` adds the arms.
