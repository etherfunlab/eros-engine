# A settled date's per-line deltas (#534)

## Intent

A client shows a relationship change per line (bond / chemistry) and per tier,
the way the per-turn delta route reports a turn. For a date it cannot: the end
route returns the after-state and the six-axis `effective_deltas`, a client must
not rebuild the line composite from the axes, and a reader that comes to the
date later (a reload, a second device, the invitation's receipt in history) has
no before-state to diff against.

The date's settlement event already stores both values: `persist_activity_in_tx`
goes through the same `write_graded` as a turn, which writes
`effective_line_deltas` and `label_changes` on the `activity` event. This change
surfaces them; it computes nothing new.

## Goals

- `POST /v2/comp/session/{session_id}/activities/{activity_id}/end` gains
  top-level `effective_deltas_computed` and `label_changes`, beside
  `effective_deltas`, on the first call and on a repeat call alike.
- `ActivityView` gains the same two fields, filled wherever a settled date is
  shown: the invitation row's `activity` receipt on both history routes and the
  start bundle, and the end route's own `activity`. The `activity` object of a
  settled date is then the same everywhere, so a client can replace the receipt
  with the end response's `activity`.

## Non-goals

- No migration and no second copy: both values are read from the `activity`
  event row, their one authoritative source.
- The six-axis `effective_deltas` stays on the end route only; the receipt does
  not gain it.
- The `final` frame and the effects route keep `ActivityView` as is in practice:
  they only ever show a date that is not settled, so the new fields are absent
  there.
- The per-turn delta route is unchanged.

## Approaches

1. **Top level and `activity` (picked).** As the issue asks, plus the shared
   view carries the fields wherever the date is settled. The end response
   carries them twice; in exchange one date has one `activity` shape everywhere.
2. **The issue literally.** Top level on the end route, receipts in history;
   the end route's `activity` does not carry them. The end response's
   `activity` then differs from the history receipt for the same date, and a
   client that swaps one for the other loses the result until a reload.
3. **`activity` only.** One location, but it departs from the issue's first ask
   and splits the end route's six-axis deltas (top level) from its line deltas
   (inside `activity`).

## Shape

Same DTOs the per-turn route uses: `BondChemistryDeltas` and
`TurnLabelChangesDto`.

- End route, top level: always present, like `effective_deltas`.
  `effective_deltas_computed` is `null` when the settlement wrote no event (no
  answered turn, or the relationship archived meanwhile). `label_changes` is
  `null` then too, and also when the date moved no tier.
- Inside `activity`: omitted unless the date is settled and its settlement wrote
  an event; `label_changes` is also omitted when no tier moved (the per-turn
  route's key-presence contract).

## Decided by the builder (Claude's calls)

- The first settlement takes both values from the write itself:
  `persist_activity_in_tx` returns them beside the state and the six-axis
  deltas, so the end route reads nothing back after the commit. This changes
  that function's return type in `eros-engine-store`.
- A repeat call and the history receipts read the event row. The store's
  `activity_event_deltas` widens to return the event's three values.
- The receipt adds no round trip to a history page: the event's values come
  back with the page's activity lookup, scoped through the parent's affinity row
  (`idx_affinity_events_affinity`), matched on `context->>'activity_id'`.
- Docs (`activities.md`, `api-reference.md`, their `.zh` pages) go in a separate
  PR after the code PR merges. Additive, so no migration guide.

## Left to the builder

Type and function names, how the receipt query is folded into the activity
lookup, and where the fill helper for `ActivityView` lives.

## Tests

- End route, first call and repeat call: both fields present and equal to the
  event's columns; a tier crossing fills `label_changes`.
- A settlement with no answered turn: both top-level fields `null`, absent in
  `activity`.
- History receipt: carries the fields for a settled date, omits them for an
  ongoing one; `label_changes` omitted when no tier moved.
