# Migrating to v2.6.1: romantic life choices, answer receipts, gives that move needs

**Applies to:**
- clients that show life-choice controls, or read history rows;
- callers that set `memory_scope` / `affinity_scope`;
- deployments (one migration);
- persona authors.

Every HTTP change is additive. Migration `0083` must run before the new image serves.
**Design:** [`2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`](../superpowers/specs/2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md)

The life choice gains two kinds besides `ask_for_space`:
- `sleep_together`, where she asks the user to sleep with her;
- `propose`, where she asks the user to marry her.

Her ask row in history now says how the choice ended. An accepted give of food, a cigarette, alcohol, herb, medicine or coffee moves her needs. Two persona keys arrive: `coffee` and `sex_addict`. The rules are in:
- [the life-choice route](../api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id);
- [the romantic choices](../life-events-needs.md#the-romantic-choices);
- [gives move needs](../life-events-needs.md#gives-move-needs).

## What users will notice

- **She can ask to sleep together, or propose.** This only happens for a pair whose scopes mark them as romantically compatible (below), once chemistry, and for a proposal also bond, are high and the last few judged turns have been warm and patient.
  - Accepting costs nothing.
  - Declining `sleep_together` drops chemistry one tier. Declining `propose` drops bond and chemistry one tier each.
  - Typing an answer instead settles the ask with no cost.
  - An accepted or ignored kind is not asked again in that session.
- **The answer stays visible.** A client that draws the user's answer between her ask and her reply can redraw it on a reload or a second device.
- **Gives change how she feels.** After an accepted meal, hunger drops one level. After a cigarette, a drink or herb, hunger drops and her social and indulgence needs rise. After medicine or coffee, all three drop.
- **A streamed turn now sees how long the user was away.** The judge and the 24-hour stale rule read the real gap instead of zero. The first 10 user messages of a session are never ghosted. Before, the 10th could be.

## What to change

- **Branch on `kind` before showing choice controls.** The `life_choice` field of `GET /bff/v1/comp/affinity/{session_id}/event` now carries `kind`: `ask_for_space`, `sleep_together` or `propose`. A client that shows its separation controls whenever `life_choice` is present will show separation wording on a proposal. Do one of these before deploying:
  - add wording for the two new kinds;
  - stop showing choice controls.

  A client without controls sees an ordinary proactive message, and a typed answer settles it at no cost. The answer route and its `accept` / `decline` body are unchanged.
- **Opt pairs in through the scopes.** She asks `sleep_together` or `propose` only when relationship memory is on and the affinity scope includes trust, intrigue or patience. For example, send `memory_scope: "full"` with `affinity_scope: "chemistry"` or `"full"`. The defaults (`neutral_and_relationship` with `bond`) never qualify.
  - The engine reads the pair from the scopes recorded on the session's newest assistant reply, so a caller that sends qualifying scopes on its turns opts that pair in.
  - The engine takes no gender or orientation. Whether a pair should be compatible is decided by the caller, through the scopes it sends.
- **Read the receipt (optional).** On both history routes, and in the start bundle's `history`, her ask row carries `life_choice: {id, kind, status, resolved_at}`.
  - `status` is one of `pending`, `accepted`, `declined`, `ignored` or `expired`.
  - Every other row omits the field. A client that deserializes history rows strictly must accept it.

## What to check

- **Established pairs can be asked on their first judged turn after deploy.** A session that was never asked has no earlier choice to count from, so judged turns from before the deploy count toward the streak.
- **Clients that display needs** see `hunger`, `social` and `indulgence` move on an accepted consumable give. The `needs` shape is unchanged. On such a turn, the engine moves those three needs one level from the pre-turn ratings, in place of the judge's rating for them.
- **Your judge prompt** needs no change.
- **Persona authors.**
  - `art_metadata.coffee` works like `smoking`, `alcoholic` and `drug`. It counts toward the habit total. An explicit `false` refuses a `give` of `coffee` two times in three.
  - `art_metadata.sex_addict` weighs `sudden_desire` three times when `true`, and a third when `false`. Its rung gate holds either way.
  - Absent keys keep today's behavior, and neither key reaches a prompt.

## Migration `0083` and deploy order

`0083` makes `engine.life_choices.tier_gap` nullable and adds a CHECK that ties it to the kind: `ask_for_space` rows carry a gap, and the romantic kinds carry none. It rewrites no row.

**Run `migrate` before the new image serves.** The new image writes romantic choices with a NULL `tier_gap`. Against a database without `0083`, that insert fails.

## Rollback

The `v2.6.0` image decodes `tier_gap` as non-null. Once a `sleep_together` or `propose` row exists, rolling back breaks two things:
- history pages that contain that ask fail with `500`;
- the affinity delta fails the same way while such an ask is pending.

Before deploying the `v2.6.0` image, run:

```sql
DELETE FROM engine.life_choices WHERE kind <> 'ask_for_space';
```

Do not backfill `tier_gap` instead: the old image would then treat a proposal as an ask for space.

Then deploy the `v2.6.0` image without running its `migrate`, because its migrator does not know `0083` and refuses a database that has it. The relaxed column and the CHECK can stay. Everything `v2.6.0` writes satisfies them.
