# Dessert and soup, four taste habits, the `mean` and `drowsy` tones — Design

- **Date:** 2026-11-28
- **Status:** Approved
- **Type:** Engine change. No migration. Wire changes, all additive: two
  `give` items (`dessert`, `soup`), two `metadata.tone` values (`mean`,
  `drowsy`). Two new `model_config.toml` keys (`tones.mean`,
  `tones.drowsy`), each off when absent. Four new `art_metadata` keys
  (`diet`, `sugar`, `salty`, `spicy`), each without effect when absent.
  Character card `spec_version` `"1.2"`, accepted beside `"1"` and `"1.1"`.
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-11-08-five-actions-two-items-gluttony-card-1-1-design.md` §3–§5
  (adding a give item, adding a habit key, the card's documentation),
  `2026-10-28-outing-indulgence-needs-and-habits-design.md` §3 (the habit
  roll), `2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md`
  §5 (`focused`: the default prompt with one line),
  `2026-11-27-erotic-tone-design.md` (the `emotional` template with
  `[relationship]` and one line by tier, the order of the rolls, the
  record), `2026-11-27-chat-speech-markup-design.md` §5.1 (the sentence a
  tone gives the speech model), `2026-11-23-affinity-50-design.md` §6.3
  (the cold war),
  `2026-11-12-activity-sub-session-design.md` §8 (a state that holds until
  her first line after an event ended).

## 1. Intent

Four small additions, shipped together as v2.9.1:

1. Two things to hand her: a dessert and a soup.
2. Four card keys about how she eats: three tastes (`sugar`, `salty`,
   `spicy`) that are habits like `gluttony`, and `diet`, which makes her
   turn food down.
3. A tone for the cold war, `mean`: today a cold war is one sentence in
   `[relationship]` and nothing tells her how to speak.
4. A tone for waking up, `drowsy`: her first reply after the pair slept
   together or went to bed together.

## 2. Goals, non-goals, anti-goals

**Goals.**

- `give` accepts `dessert` and `soup`, and each behaves in every table a
  consumable is in (§4).
- `sugar`, `salty` and `spicy` are in every table `gluttony` is in (§5.1).
- `diet = true` puts a `give` of `food`, `cuisine` or `dessert` through
  the habit roll (§5.2).
- While a cold war is on, a turn that may draw a tone is `mean` with
  probability `tones.mean`, before any other tone is rolled (§6).
- Her first reply after an `asleep_together` or `having_sex_together`
  event ended is `drowsy` with probability `tones.drowsy` (§7).
- A deployment that sets neither key, and a card that carries none of the
  four, see no change at all.

**Non-goals.**

- A give item for `salty` or `spicy`. They have none; their `false` does
  nothing.
- A taste read from a give's free-text `name`. The engine does not parse
  it.
- `diet`, or any of the four, in the judge's `[角色人格]` block, in a life
  event's gate, or in the image composer.
- A tone on a turn that has none today: group turns, `/open` greetings,
  edit turns, voice turns, proactive and event messages, her asks and her
  replies to an answer, the message that announces a cold war.
- `mean` or `drowsy` on a turn that carries a user action. Those turns
  keep the tone they have today.
- A change to how a cold war starts or ends, or to the judge.
- `drowsy` for an event that is still under way, or after `asleep` (alone)
  or any other kind.

**Anti-goals.**

- A second list of which items `diet` blocks, or of which kinds she wakes
  from, anywhere but one place in `eros-engine-core`.
- The tone recorded in two places, or handed to the judge.
- `drowsy` days later: the reading has an upper bound in time (§7.1).

## 3. Approaches

**What the three tastes do.** Three ways were weighed: (a) the full
`gluttony` treatment: counted in the `indulgence` drift, named when
`indulgence` is high, `false` rolls on the matching give; (b) named but
not counted, on the grounds that a taste is not an addiction; (c) as (a),
with `soup` as `salty`'s item. The owner picked (a). Only `sugar` has an
item (`dessert`); a card with three tastes `true` reaches the fastest
drift, which is accepted.

**What `diet` blocks.** `food`, `cuisine` and `dessert`; or those plus
`soup` and `soft_drink`; or `dessert` and `cuisine` alone. The owner
picked the first: soup and every drink pass.

**What makes a `mean` reply.** The `focused` way (the default prompt with
its history, memories and relationship, one line in `[this_turn]`) against
the `erotic` way (the `emotional` template, no history). The owner picked
the `focused` way: she remembers what the quarrel was about and can throw
the user's words back.

**What makes a `drowsy` reply.** The same two ways. The owner picked the
`erotic` way: someone just awake does not pick the earlier topic back up.
The one line carries the fact that they just woke up side by side, so
nothing else in the prompt has to.

**The lines.** Three candidates were offered for each tone. The owner
keeps all three of each, by tier: `mean` by the bond tier, `drowsy` by
the chemistry tier (§6.2, §7.2).

**Where the "just woke up" fact lives.** In the `刚才` line beside "back
from a date", or in the tone's one line. The second is taken *(engine's
call)*: the first would show on a turn whose roll was lost or whose key is
off, and would reach the judge; the second changes nothing unless the
roll is won.

Everything else has one sensible way: items follow `cuisine`, keys follow
`gluttony`, the keys, the rolls and the record follow `focused` and
`erotic`.

## 4. Give items

`GivenItem` and `GivenItemDto` gain two values, both consumables.

| `item` | 中文 | unit | word | curve | needs moved | effect words, level 1–4 |
|---|---|---|---|---|---|---|
| `dessert` | 甜点 | 份 | 甜点 | front (as `food`) | social +1, indulgence +1 (as `cuisine`) | 嘴里还留着甜 / 吃得挺开心 / 甜得正满足 / 甜得发腻 |
| `soup` | 汤 | 碗 | 汤 | front | none (as `food`) | 胃里还暖着 / 喝得挺舒服 / 浑身暖和 / 喝得肚子发胀 |

The phrases read 「递给你 1 份甜点」 and 「递给你 1 份「提拉米苏」」,
「递给你 1 碗汤」 and 「递给你 1 碗「罗宋汤」」.

`dessert` is `sugar`'s item (§5.1) and moves needs as the things she is
hooked on do. `soup` is nobody's item: no key rolls on it, `diet`
included. *(Engine's calls: the units, the words, the curves, the needs
and the effect words.)*

## 5. Card keys

All four are read as the habit keys are: `art_metadata.<key>`, a boolean,
anything else reads as unset.

### 5.1 `sugar`, `salty`, `spicy`

Three habit keys beside `smoking`, `alcoholic`, `drug`, `coffee` and
`gluttony`, in every table those are in:

| where | the three |
|---|---|
| `Habits` | gains `sugar`, `salty`, `spicy`, each `Option<bool>`; `count_true` counts them |
| `indulgence` drift | counted with the others; three or more `true` is the fastest pace, as today |
| `needs_line`, `indulgence` high | may be picked, after `gluttony`, in this order: 很想吃点甜的 / 很想吃点咸的 / 很想吃点辣的 |
| `Habits::uses` | `dessert` → `sugar`. `salty` and `spicy` map to no item |
| the habit roll | a `give` of `dessert` with `sugar` explicitly `false` rolls the same 1/3 before the judge, with the same consequences as a `cuisine` under `gluttony = false` |
| the judge's `[角色人格]` | not listed |
| life events | not read |

### 5.2 `diet`

`diet = true` means she is on a diet. It is not a habit: `count_true`
does not count it, `needs_line` never names it, the drift does not read
it. It does one thing.

A `give` of `food`, `cuisine` or `dessert` to a persona whose `diet` is
`true` takes the habit roll, exactly as a give whose habit key is `false`
does: rolled once before the judge, 1 time in 3 the turn goes on as usual,
otherwise the judge's line ends 「，这次你不会收下」, the response in
effect is `refuse`, the give leaves no effect and moves no need, and it
costs the bond line a tier.

`false` and unset do nothing. `diet` decides alone: `gluttony = true` or
`sugar = true` does not exempt a dish or a dessert (she craves it and
turns it down). A turn rolls at most once, whichever of the two reasons
applies, or both. *(Engine's call.)*

The list of the three items lives in one function in `eros-engine-core`,
beside `Habits::uses`.

### 5.3 Character card 1.2

`spec_version` `"1.2"` is accepted beside `"1"` and `"1.1"`; all three
parse to the same `Card`, and the engine reads the same keys from each.
Nothing about the document's shape changes: `art_metadata` is an open
object, so an older card may carry the four keys too and the engine reads
them. 1.2 is the version whose documentation lists them. *(Owner's
ruling.)*

Docs: a new `docs/eros-character-v1.2.md` holds the full field tables as
`eros-character-v1.1.md` does, with the three tastes in the habit row, a
row for `diet`, and a *Versions* section that says 1.2 adds the four keys
and that `"1"` and `"1.1"` are still accepted. `eros-character-v1.1.md`
gains one sentence in its *Versions* section pointing at 1.2 and is
otherwise untouched. `byoc.md`, `byoc.zh.md`, the API reference and the
deploying guide link to 1.2 as the current format.

## 6. The `mean` tone

### 6.1 When

A turn rolls `mean` when all of these hold:

- `tones.mean` is above 0;
- it is a 1:1 user-message turn whose plan bears text;
- the turn carries no user action;
- the session's cold war is on, as the turn's affinity row reads it.

The roll is against `tones.mean`. `1.0` makes every such turn `mean`. A
BYOC session never enters a cold war, so it never rolls.

The turn on which a cold war starts is not `mean`: the cold war is set
after that turn's reply. The message that announces it keeps its own cue.

A date's turns draw their tone at the same place and read the inviting
session's affinity row, so a date held during a cold war rolls `mean` as
the main chat does. *(Engine's call: no exception for a date, as no other
tone makes one.)*

**The exit.** A cold war ends on three judged turns above the lowest
patience level, and the judge reads her replies in the history. The
affinity 5.0 design (§6.3) already notes that her cold tone drags that
reading down; `mean` makes her replies colder still, so a cold war may
last longer with the tone on. This is accepted and left to be observed:
the exit rule is unchanged, `tones.mean` is a probability a deployment can
lower, and every `mean` reply and every start and end of a cold war is on
record (`metadata.tone`, the `relationship` events). *(Engine's call.)*

### 6.2 The prompt

`mean` is built as `focused` is: the default request and system prompt
with the history, the memories and the relationship, without `[feelings]`
and `[emotional_context]`, `[reply_length]` at the lowest tier, and one
line alone in `[this_turn]`. No nudge is rolled.

The line is set by the pair's bond tier, as the turn's prompt reads it:

| bond tier | line |
|---|---|
| 1–2 | 你还在生对方的气：话里带刺，回得冷淡简短，不主动缓和 |
| 3 | 你们在冷战，你不想好好说话：挑对方话里的毛病，冷言冷语，一句带过 |
| 4–5 | 你心里憋着火：说话刻薄，不留情面，也不解释自己为什么生气 |

The owner's rule is "the lower the bond, the more she explains": the line
that forbids explaining is the one for the closest pair. *(Engine's call:
the split 1–2 / 3 / 4–5.)*

## 7. The `drowsy` tone

### 7.1 When

A turn rolls `drowsy` when all of these hold:

- `tones.drowsy` is above 0;
- it is a 1:1 user-message turn whose plan bears text;
- the turn carries no user action;
- no incident is under way for her at the turn *(engine's call: in the
  middle of something else she is not just awake)*;
- she **just woke** in this session: the newest `asleep_together` or
  `having_sex_together` event this session placed has ended by the turn's
  moment, no more than 12 hours before it, and the session holds no
  assistant row with text written after the event ended.

The roll is against `tones.drowsy`. `1.0` makes every such turn `drowsy`.

"Just woke" is one store read, `EventRepo`'s, shaped like
`ActivityRepo::returned_from`: it is true from the event's end until her
first line with text, whatever that line is. A proactive message, an
event message, or her reply to an action turn uses it up without a
`drowsy` reply; so does a lost roll's reply. An event that ended early
(cut short by another event, by an accepted ask, by a chemistry drop)
counts as one that ended. *(Engine's calls: the 12 hours, and that every
way of ending counts.)*

There is no romance condition and no floor: the tone says she is sleepy,
not that she wants anything. The pair got here through a gate already.

The two kinds are one list in `eros-engine-core`.

### 7.2 The prompt

`drowsy` is built as `erotic` is: the `emotional` template, plus the
`[relationship]` block, plus one line in `[this_turn]`. Everything that
holds for an `emotional` turn holds for it (no history, no profile, no
nudges, the lowest `[reply_length]`).

The line is set by the pair's chemistry tier, as the turn's prompt reads
it, and is the same for both kinds:

| chemistry tier | line |
|---|---|
| 1–2 | 你们一起睡了一觉，你刚醒，脑子还没转起来：说得含糊简短，想到什么说什么 |
| 3 | 你刚在对方身边睡醒，人还迷糊：话少、慢半拍，带着没睡够的黏糊 |
| 4–5 | 你刚醒，还赖在对方身边不想动：声音懒懒的，回得短，带一点撒娇 |

The owner's rule is "the higher the chemistry, the more she plays up to
them", and the order of the three lines is the owner's. *(Engine's call:
the split 1–2 / 3 / 4–5.)*

## 8. Order, configuration and the record

**Order.** `mean` over `drowsy` over `erotic` over `emotional` over
`focused`. Each roll is made only when its conditions hold; the first one
won names the turn and no later one is read. `mean` and `drowsy` roll
only on a turn without a user action, so they never meet `erotic`'s fixed
moments or a listed emotional action; on the turns where they roll they
come before the everyday `erotic` draw. *(Engine's call: `mean` above
`drowsy`, since a cold war colours everything said in it.)*

**Configuration.** `[tasks.chat_companion] tones` gains `mean` and
`drowsy`, each a probability in `[0.0, 1.0]`, validated as the other
three are, each off at `0` or absent, each working without the others.
Task-level.

**The record.** A turn for which a `mean` or a `drowsy` roll was made
writes `metadata.tone` on its assistant rows: `"mean"` or `"drowsy"` when
won; when lost, whatever the later rolls decide, `"default"` if none wins.
A turn for which no roll at all was made writes no key, as today.

**Speech markup.** `tone_sentence` gains one sentence for each tone, so a
line written in it is voiced that way:

- `mean`: `She said this while angry at the other person: cold, clipped, with an edge.`
- `drowsy`: `She said this just after waking up: sleepy, slow, soft.`

`drowsy` does not also carry the `emotional` sentence, as `erotic` does:
"in the grip of a feeling" is not what a sleepy line is. *(Engine's calls:
the two sentences, and that `drowsy` stands alone.)*

## 9. What changes for a live user

Nothing until a deployment sets a key or a card carries one of the four.
A client that lists give items can add the two. A client that reads
`metadata.tone` meets two new values.

## 10. Tests

- `eros-engine-core`: the curve and the need move of each new item; the
  serde names; `count_true` and `uses` with the three tastes; the roll for
  `dessert` under `sugar = false`; the roll for each of `food`, `cuisine`,
  `dessert` under `diet = true`, and no roll for `soup`, `soft_drink`,
  `tea`, `coffee`, nor under `diet = false` or unset; one roll when both
  reasons apply; the tone names and which template each tone is on; the
  tier-to-line tables.
- `eros-engine-llm`: the two keys parse, default to 0, and are range
  checked.
- `eros-engine-server`, prompt: the phrases and effect words of the two
  items; the pick list with the three tastes; a `mean` prompt keeps the
  history and holds its line alone in `[this_turn]`; a `drowsy` prompt has
  no history, has `[relationship]` and its line; each line by tier.
- `eros-engine-server`, turn: the order table of §8 over `decide_tone`;
  a cold-war turn is `mean` and records it; outside a cold war, on an
  action turn, or with the key at 0, the turn is as today; a turn after an
  ended `asleep_together` and after an ended `having_sex_together` is
  `drowsy` and records it; the turn after that one is not; an event still
  under way, one that ended more than 12 hours ago, another incident under
  way, an action turn, each leave the turn as today.
- `eros-engine-store`: the "just woke" read against the cases of §7.1,
  with fixed, microsecond-aligned timestamps.
- `eros-engine-server`, speech: the sentence for each of the two tones.
- `eros-engine-server`, cards: a `"1.2"` card parses; a `"1.3"` one is
  refused.
- `eros-engine-server`, turn: a date's turn during a cold war is `mean`.

## 11. Order of work

1. Code PR into `dev`: items, keys, the two tones, the example configs'
   comments.
2. Docs PR into `dev`: `api-reference`, `life-events-needs`,
   `model-config`, the new `eros-character-v1.2`, and their `.zh` twins
   where they exist; and a migrating guide,
   `docs/migrating/v2-9-1-taste-habits-and-two-tones.md`, as v2.9.0 has
   one for `erotic`. It covers what the wire-level "additive" does not:
   crate users who match on `Tone` or `GivenItem` or build `Tones` or
   `Habits` by hand, clients that read `metadata.tone`, and rollback (the
   previous image refuses to boot on a `tones` table that names `mean` or
   `drowsy`: remove the keys first).
3. An issue on the web client's side for the two items, the four card
   keys and the two tone keys.
4. Release v2.9.1.

## 12. Decisions

Owner's rulings:

- The three tastes get the full `gluttony` treatment; only `sugar` has an
  item.
- `diet = true` blocks `food`, `cuisine` and `dessert` through the habit
  roll; soup and drinks pass.
- `mean` is cold-war only, on turns that may draw a tone, ahead of
  `emotional` and `focused`, built the `focused` way.
- `drowsy` is her first text reply after `asleep_together` or
  `having_sex_together` ended, built the `erotic` way.
- Both tones keep three lines each, `mean` by bond tier, `drowsy` by
  chemistry tier; the six lines are the owner's picks.
- The tone's name is `drowsy`.
- The four keys come with character card 1.2.

Engine's calls, open to reversal: everything marked *(engine's call)*
above, and the cravings' wording 很想吃点甜的 / 咸的 / 辣的 (shown to the
owner with the option picked).

## 13. Left to the builder

- Whether `mean` and `drowsy` reuse `build_focused_prompt` and
  `build_erotic_prompt` with the line passed in, or get builders of their
  own.
- The shape of `decide_tone`'s inputs once there are five rolls.
- The name and exact SQL of the "just woke" read.
- Where the tier-to-line functions live (beside `erotic_line`).
