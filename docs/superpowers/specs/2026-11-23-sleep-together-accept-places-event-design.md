# Her `sleep_together`, accepted, places `having_sex_together` — Design

- **Date:** 2026-11-23
- **Status:** Approved
- **Type:** Engine change. No migration. Wire: the life-choice answer's
  `event` is no longer `null` for an accepted `sleep_together`.
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`
  (§4.5, the answer and its costs),
  `2026-11-11-eight-short-events-and-hygienic-design.md` (`bathe_together`,
  whose accept places its event),
  `2026-11-23-twelve-together-actions-and-propose-design.md` (§6,
  `sex_together` as her `sleep_together` asked the other way).

## 1. Intent

When the user accepts one of her asks, the outcome matches what happens when
the user asks her for the same thing and she accepts. An accepted
`sex_together` places `having_sex_together`, so an accepted `sleep_together`
places it too. Her ask to wash together can likewise turn out as either of the
user's two asks: `shower_together` or `bath_together`.

## 2. How it stands

- **`travel_together`** already does this. An accept places
  `traveling_together`, and every accepted one in production has its event. No
  change.
- **`bathe_together`** places an event on every accept, but always
  `bathing_together`, never a shower.
- **`sleep_together`** does not. An accept ends the incident under way and
  places nothing. No accepted one in production has an event, so the
  virginity read and the `pregnancy_scare` follow-on never see her own
  accepted ask.

## 3. Goals, non-goals, anti-goals

**Goals.** Accepting her `sleep_together` places `having_sex_together` (one
hour) in the transaction that resolves the choice. The choice's `event_id`
points to the new event, and both the JSON response and the `resolved` frame
carry it as `event`. Accepting her `bathe_together` places
`showering_together` or `bathing_together`, one or the other at even odds
*(owner's ruling)*.

**Non-goals.**
- Backfilling the accepted `sleep_together` rows already stored. *(Owner's
  ruling: forward only.)*
- Placing anything for a typed answer. A choice answered in words stays
  `ignored`.
- A picture or the `emotional` tone on her reply.
- Any change to `propose`. The user's `propose` places no event either.
- Any change to `travel_together`.

**Anti-goals.** A read that counts accepted choices as events. That would be
a second source for "the pair has been to bed".

## 4. Approaches

1. **Place the event when the answer is stored**, the way `bathe_together`
   does. *Taken.*
2. **Derive her state from the accepted choice at read time.** Every reader
   of events would need a second read: the draw's virginity, the reply
   prompt, the picture state, the events route and the story director.
   *Rejected.*

## 5. Design

- **The placement.** In the answer route's `placed` match, a `sleep_together`
  accept maps to `having_sex_together`, with the span from `events::lasts`.
  The event key is `life_choice:{choice_id}:having_sex_together`. The
  pre-empting insert ends a running incident; a background such as a trip
  keeps going.
- **The explicit end of the incident under way** stays for `propose` only.
  For `sleep_together` the pre-empting insert already ends it.
- **Costs do not change.** An accept costs nothing; a decline drops chemistry
  one tier.
- **Her reply.** The cue stays as it is (`（对方答应了你的邀请。）`). The
  reply prompt reads the new event as under way, as it already does after a
  `bathe_together` accept.
- **What follows without new code.** Once the row exists:
  - the instance stops counting as a virgin in the draw (`EventKind::SEX`);
  - `having_sex_together` leads to `pregnancy_scare`;
  - the story director already leaves the kind out;
  - her own `sleep_together` stays settled for the session, as it is
    already `accepted`.
- **Doc comments and OpenAPI.** The module header, the handler's doc and
  `LifeChoiceRespondResponse.event` say that a `sleep_together` accept
  places an event.

### 5.1 Her bath ask

- **The pick.** A `bathe_together` accept places `showering_together` or
  `bathing_together` at even odds. Both last 30 minutes, take the key
  `life_choice:{choice_id}:{kind}`, and are returned as `event`.
- **Her reply.** A bath keeps today's cue,
  `（对方答应了和你一起洗澡，你们现在一起泡在浴缸里。）`. A shower gets
  `（对方答应了和你一起洗澡，你们现在一起站在花洒下。）` *(engine's wording,
  overridable)*.
- **Nothing else changes.** The ask, its gates, its 7-day cooldown and the
  decline's cost do not read the event kind. Both kinds already stay out of
  the story director's events. Stored accepts keep their `bathing_together`.

## 6. Rollback

There is no schema change. v2.8.1 already knows `having_sex_together` and
`showering_together`, and
nothing there reads `event_id` by kind. Rolling back the image only stops new
placements; the rows already placed stay valid.

## 7. Order of work

One pull request at a time, each into `dev`:

1. **The code**, with tests:
   - an accept places one `having_sex_together` linked to the choice, and
     the response carries it;
   - a decline places nothing;
   - an accept during a bath together ends the bath and starts the event
     (the test exists today; its expectation changes);
   - a bath accept places a shower or a bath, with the cue that matches it.
2. **Docs**:
   - `api-reference` (en, zh): the `event` line of the life-choice answer;
   - `life-events-needs` (en, zh): her accepted `sleep_together` places
     `having_sex_together`, and her accepted `bathe_together` places a
     shower or a bath;
   - a migrating note for the release.

A web-side issue says that the answer to `sleep_together` now carries
`event`, and that the answer to `bathe_together` may carry
`showering_together`.

## 8. Left to the builder

- Whether the `sleep_together` and `bathe_together` arms share a match arm.
- Test names.
