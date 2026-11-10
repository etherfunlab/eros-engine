# Migrating to v2.6.4: the travel-together choice

**Applies to:**
- clients that show life-choice controls or draw event kinds;
- crate users of `eros_engine_core::scope::romance_compatible`.

No migration and no wire change. A client that does nothing keeps working (see [Unknown kinds](#unknown-kinds)).
**Design:** the [travel-together choice](../life-events-needs.md#going-away-together) and [the life-choice route](../api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id).

## What users will notice

- **She can suggest going away together.** This needs bond at tier 3 or more and the last three judged turns warm. It does not depend on the pair's scopes, and it does not come while any background event is under way.
  - Accepting starts a `traveling_together` background of 48 to 72 hours. She keeps replying, and her on-the-road, scenery and selfie events follow the trip.
  - Declining costs nothing.
  - An accepted or ignored suggestion is not repeated for 14 days.
- **More pairs count as romantically compatible.** Compatibility now reads only whether `affinity_scope` carries a chemistry axis; `memory_scope` no longer matters. A pair that sends a chemistry-bearing `affinity_scope` with a narrow `memory_scope` can now see `strip` and the `sleep_together` / `propose` choices.

## Clients

1. **Add the `travel_together` choice kind** wherever you draw life-choice controls: the ask message, its accept and decline buttons, and any tray of pending choices. The answer route is unchanged (`POST /v2/comp/session/{session_id}/life-choices/{choice_id}` with `accept` or `decline`). An accept returns the new trip as `event`.
2. **Add the `traveling_together` event kind** to your event catalog: its label, icon and any image you show for a background. It is a `trip`-category background the engine places itself. It is never drawn, and naming it in an `/events` `kinds` filter is a `422`.

### Unknown kinds

A client that knows neither kind degrades safely:
- Her ask arrives as an ordinary proactive message.
- A typed reply leaves the choice ignored, which costs nothing.
- The trip shows wherever unknown event kinds show.

## Crate users

`eros_engine_core::scope::romance_compatible` drops its `memory` parameter: call it with the affinity scope alone.
