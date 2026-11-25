# More together actions, what she wears in the water, a chemistry drop that ends the event, and a custom together action — Design

- **Date:** 2026-11-26
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: seven new
  action types, seven new event kinds (placed, never drawn), one new field on
  the event view. One behaviour change on the wire: five kinds can end before
  their `ends_at`.
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md`
  (§5, the together actions; §7, her state in the picture),
  `2026-11-23-twelve-together-actions-and-propose-design.md`,
  `2026-11-23-hot-spring-fight-challenge-bet-design.md`,
  `2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md`
  (§6, `swim_together` and `hike_together`; §7, `[对方]`),
  `2026-11-08-strip-and-world-setting-design.md` (§2, the romance gate and the
  clothing an accepted strip writes),
  `2026-10-05-chat-user-actions-design.md` (`custom`),
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md` and
  `2026-11-11-eight-short-events-and-hygienic-design.md` (her
  `sleep_together` and `bathe_together` asks),
  `2026-11-24-period-cycle-design.md` §9 (an ask that waits).

## 1. Intent

Five changes around doing things together:

1. The user can ask her to dance, go on an adventure, play with fire, sing,
   swim naked, or commit a crime together.
2. A picture drawn while the pair is in a hot spring or a sauna shows her
   dressed for it, never in street clothes.
3. An intimate event the pair is in ends at once when chemistry falls a
   tier, as when she refuses another action of the user's while it lasts.
4. While the pair is already in the water or in bed, she does not ask the
   user to bathe with her or to go to bed with her.
5. The user can ask her to do something together in their own words, as
   `custom` lets them act in their own words.

The owner asked for swimming and hill-climbing too; `swim_together` and
`hike_together` (marker 想和你一起爬山) shipped in v2.8.2 and are unchanged.

## 2. Goals, non-goals, anti-goals

**Goals.** The wire additions of §4 and §8; the clothing rule of §5; the
early end of §6; the two asks held back in §7.

**Non-goals.** A caller-chosen length for a custom together action; a
world rule on any of the seven new actions; a romance gate or a period roll
on any but `skinny_dip_together`;
changing the built-in composer prompt; changing what the reply prompt says
she wears; ending a walk, a hike or any other non-gated together event on a
chemistry drop; a message from her when an event ends early.

**Anti-goals.** A second stored copy of the user's text; a stored copy of
"the chemistry tier when she said yes"; removing `ask_for_dance` or
`ask_dancing`.

## 3. Approaches

**Who picks what she wears in the water.** Four ways: the engine by romance
compatibility; both written and left to the composer; always nude; always a
towel. The owner picked the first: the same test that decides `strip`,
`shower_together` and `bath_together`, and a fixed outcome for a given pair.

**Which events a chemistry drop ends.** Three scopes: the kinds whose
action carries the romance gate; those plus `asleep_together`,
`hot_spring_together` and `sauna_together`; every together kind. The owner
picked the first: the list is the romance gate's own, and a refused kiss
does not end a hike.

**Where a custom together action's text lives.** Three ways: a generic kind
whose row points at the user's message, the text read from that message; the
same kind with a text column on `engine.persona_events`; no event at all,
only a forced picture. The owner picked the first: no column, and no second
copy of the user's words for anything else to keep in step.

**"Play with fire."** Literal fire, a fantasy-only fire magic, or a figure of
speech for risky flirting. The owner picked literal fire.

## 4. Six together actions

| action `type` | marker | places | label | category | lasts | fits |
|---|---|---|---|---|---|---|
| `dance_together` | 想和你一起跳舞 | `dancing_together` | 和对方一起跳舞 | `short` | 1 h | both |
| `adventure_together` | 想和你一起去冒险 | `adventuring_together` | 和对方一起冒险 | `mid` | 2–3 h | both |
| `play_with_fire_together` | 想和你一起玩火 | `playing_with_fire_together` | 和对方一起玩火 | `short` | 1 h | both |
| `sing_together` | 想和你一起唱歌 | `singing_together` | 和对方一起唱歌 | `short` | 1 h | both |
| `skinny_dip_together` | 想和你一起裸泳 | `skinny_dipping_together` | 和对方一起裸泳 | `short` | 1 h | both |
| `commit_crime_together` | 想和你一起去犯罪 | `committing_crime_together` | 和对方一起犯罪 | `short` | 1 h | both |

Each behaves as `jog_together` does: no payload; the judge decides; an
accept places the kind in the write that stores the answer (no event inside
a date) and ends the incident under way; the accepted turn is a picture turn
when the image executor is available; a refusal drops chemistry
(`commit_crime_together`: bond, below); no romance
gate, no species gate, no world rule, no period roll; not answered in the
`emotional` tone. The six kinds are `SPECIAL`: weight 0, no cooldown,
`Neutral` place, never told, not offline, shared with the user (`[对方]`),
and they reach the story director as the other outings do.

**`skinny_dip_together` differs** where `streak_together` and
`swim_together` do:

- ruled out before the judge when the turn's scopes are not
  romance-compatible, as `streak_together` is; that refusal drops chemistry;
- answered in the `emotional` tone when the deployment has it on, accepted
  or refused;
- rolled during her period as `swim_together` is (spec 2026-11-24 §8): a
  refusal the roll rules drops nothing;
- `skinny_dipping_together` stays out of the story director's
  `life_events`, as `streaking_together` does;
- it is one of §6's kinds, and not one of §7's.

`dance_together` is not `ask_for_dance` (a gesture: no event, no picture)
nor `ask_dancing` (she dances for the user). All three stay.

`commit_crime_together` names no crime: what the pair does, the user says
in the turn's text and the conversation plays out. Nothing follows from it:
no `police_encounter`, no other kind. A refusal drops bond, not chemistry:
it is the one together action whose `refusal_line` is the bond line.
*(Owner's ruling.)*

`play_with_fire_together` is literal fire: a campfire, torches, sparklers.
*(Owner's ruling.)* The label is short and can be read as a figure of
speech by a model; whether it needs a more literal wording is the owner's
to judge on real output.

*(Engine's calls: every value in the table except the six actions
themselves, what follows a crime, and every rule on `skinny_dip_together`.)*

## 5. What she wears in a hot spring or a sauna

**Cause.** The composer is told that her state wins over what she has on
(spec 2026-11-20 §7). For a hot spring or a sauna "dressed as that state has
her" does not settle what she wears, and pictures come back in street
clothes.

**Rule.** When the `[此刻状态]` of a picture names `hot_spring_together` or
`sauna_together`, the engine writes the composer's `[此刻穿着]` itself, over
the judge's `clothing` and over an outfit the caller named:

| the pair | `[此刻穿着]` |
|---|---|
| romance-compatible | 什么都没穿 |
| not romance-compatible | 身上只裹着一条浴巾 |

*(Owner's ruling: the engine picks, by romance compatibility. Engine's call:
the two wordings.)*

- **Romance-compatible** is `scope::romance_compatible` of the turn's
  `affinity_scope`, the test `strip` uses. A picture drawn on a path that
  has no request reads the scopes that path already resolves for the session
  or the speaker; with none, the pair reads as not compatible.
- **Which pictures.** Every picture the chat pipeline draws while either
  kind is in `[此刻状态]`: the accepted turn's own (the kind being placed
  counts as live), any later picture while the event lasts, and the accepted
  turn's picture inside a date, where the kind is her state for that turn
  only.
- **Only the composer's slot.** The reply prompt's clothing line, the
  stored `plan.clothing` and her wardrobe are untouched. The composer's
  input snapshot records the clothing as sent.
- No other kind is covered: `showering_together`, `bathing_together`, her
  own `showering` and `bathing` stay with "the state wins".

## 6. A chemistry drop ends an intimate event

**The kinds.** `showering_together`, `bathing_together`,
`having_sex_together`, `streaking_together`, `skinny_dipping_together`: the
kinds placed by the actions the romance gate covers
(`pde::together_ruled_out`). The rule reads the
kind, not who asked: a row her own accepted `bathe_together` or
`sleep_together` ask placed ends the same way. *(Owner's ruling: the
romance-gated kinds. Engine's call: her asks' rows included.)*

**The trigger.** A write that moves the chemistry tier of a session of the
instance down, from something the user did after the event started:

- a chat turn's affinity write, when the turn's user message was sent after
  the event started. A refused action's tier drop is the common case; a
  graded turn that carries chemistry across a tier edge counts the same;
- the drop of an ask of hers the user declined, when the answer came after
  the event started.

The tier that fell is measured inside the write, after time decay: a tier
that decay alone carried down ends nothing. The turn that placed the event,
and an earlier turn whose write lands late, were sent before the event
started and end nothing. A date's settlement ends nothing. Nothing compares
against the tier at the moment she accepted. *(Engine's calls: decay and
the "sent after it started" test.)*

**The effect.** A foreground event of one of these kinds under way at
that moment ends there: its `ends_at` becomes the time of the write. A
background and a cycle row keep running. Nothing is told and no message is
written; the turn whose write dropped the tier is already answered, and
from the next turn on her state block and her pictures no longer carry the
event.

**Not covered.** A together action accepted inside a date placed no event,
so there is nothing to end. A drop on the bond line ends nothing.

**Locks.** Two writers take the instance lock and then the affinity row's
(`ChatRepo::set_action_response`, the life-choice answer); none takes them
in the other order. The cap must not take the instance lock while the
affinity row is locked, so it runs after the affinity write commits, in a
write of its own. The two are then not atomic; a crash between them leaves
the event to run to its stored end, which is accepted.

**On the wire.** No new field. A client that scheduled anything on a
together event's `ends_at` must re-read events after a turn.

## 7. Two asks she holds back

While a foreground event of `bathing_together`, `showering_together`,
`hot_spring_together`, `sauna_together` or `having_sex_together` is under
way for the instance:

- `try_bathe` does not ask `bathe_together`.
- `try_romance` does not ask `sleep_together`.

*(Owner's ruling: the five kinds, the two asks.)*

The ask only waits, as during her period: nothing is settled and no
cooldown starts, so the next judged turn after the event ends may ask.
`propose`, `fall_asleep_together`, `travel_together` and the status asks
are asked as before. An ask already pending when the event starts stands.
*(Engine's calls.)*

## 8. `custom_together`

### 8.1 The action

`{ "type": "custom_together", "text": "…" }`. `text` is required, 1 to 100
characters, validated as `custom`'s is. Both send routes take it.

| | |
|---|---|
| history marker | （想和你一起：{text}） |
| judge's `[用户动作]` | 对方想和你一起做一件事：「{text}」 |
| who decides | the judge; nothing rules it out before |
| refused | chemistry drops |
| accepted | places `custom_together`; a picture turn |
| tone | not `emotional` |

The user's text is quoted with 「」 wherever it reaches a model, and stays
unquoted in the user-role history marker, as for `custom`. *(Engine's
calls: every row; the owner asked for "the same as a custom action".)*

### 8.2 The kind

`custom_together`: `SPECIAL`, weight 0, no cooldown, `Neutral` place, fits
both settings, never told, not offline, category `short`, lasts 1 hour,
shared with the user. It is not one of §6's kinds, takes no period roll,
and stays out of the story director's `life_events`: what it holds is
unknown. *(Engine's calls.)*

Accepted, it is placed as every together action's event is: in the write
that stores the answer, ending the incident under way; none inside a date.

### 8.3 The text

The placed row's `message_id` is the user's message that asked. The text is
read from that row's `metadata.action.text`; nothing else stores it.

- **Label.** 和对方一起：「{text}」 wherever an event's label is printed:
  her state block, the composer's `[此刻状态]`. On the accepted turn, and on
  an accepted turn inside a date, the text comes from the action in hand.
- **The message gone.** The text lives exactly as long as the user's
  message does. The engine deletes no single message today, and an erasure
  leaves `chat_messages` as it is, so neither removes the text. Should the
  row go, the foreign key nulls `message_id` and the label falls back to
  和对方一起做一件事.
- **`message_id` on this kind** names the user's message, where on a told
  kind it names her message. Two readers take an event's `message_id` for
  her message and must not for this kind: `stored()` in the events route
  (reached when a draw is asked under an `event_key` equal to the action's
  key) returns `message: null` for it, and `StoryRepo::life_events` joins
  the message as her telling, so the kind is in its kept-out list (§8.2).

### 8.4 On the wire

- The action enum gains `custom_together`; the event kind enum gains
  `custom_together`.
- The event view gains `text`: the user's text for a `custom_together`
  whose message still exists, `null` otherwise and for every other kind.
- On a `custom_together` event `message_id` is the user's message.

## 9. Tests

- §4: the six actions name their kinds and markers; the kinds are placed
  kinds with the values of the table; only `skinny_dip_together` is ruled
  out before the judge, rolled on her period, and answered emotionally; a
  refused `commit_crime_together` drops bond and leaves chemistry.
- §5: the composer payload of a picture in either kind carries the engine's
  clothing, for a compatible and an incompatible pair, over a named outfit;
  a picture in any other kind is byte-identical to today's.
- §6: a refused action during `having_sex_together` ends it at the write;
  the placing turn's own write, a write of a turn sent before the event
  started, and a tier that only decay carried down do not; a bond drop does
  not; a chemistry drop during `walking_together` does not;
  a background survives.
- §7: with one of the five kinds under way neither ask is written and
  nothing is settled; after it ends the ask is written.
- §8: validation of `text`; accepted, the row points at the user's message
  and the state block prints the text; with the message deleted the label
  falls back; the event view carries `text`.

## 10. Order of work

One pull request at a time, each into `dev`:

1. Code: §4–§7.
2. Code: §8.
3. Docs: `api-reference` (action types, event kinds, the event view's
   `text`), `life-events-needs` (the placed kinds, the early end, the two
   asks), and a migrating guide for the release.

A web-side issue lists the seven action types, the seven kinds, `text`, and
the early end.

## 11. Migrating notes to write

- Seven action types; seven event kinds a client must be ready to read.
- `text` on the event view, and `message_id` naming the user's message on
  a `custom_together`.
- Five intimate kinds can end before the `ends_at` first read.

## 12. Left to the builder

- Rust names; where the kinds go in `SPECIAL`.
- Whether §6's list and `together_ruled_out`'s share one definition.
- Where each affinity writer learns the tier fell, and where the cap runs
  for each (§6, locks).
- How the text reaches each place a label is printed: on the event record,
  or read when the kind is `custom_together`.
- Which scopes each non-request picture path reads for §5.
- Whether §7 shares one read of the current events with the period check.
