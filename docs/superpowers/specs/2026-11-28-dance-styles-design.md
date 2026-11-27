# Dance styles and the taste keys `dance`, `naturism`, `risky` — Design

- **Date:** 2026-11-28
- **Status:** Approved
- **Type:** Engine change. No migration. Wire change, additive: an
  optional `style` on three action types (`ask_dancing`, `dance_together`,
  `dance_naked_together`), with `name` when the style is `custom`. Three new
  `art_metadata` keys (`dance`, `naturism`, `risky`), each without effect
  when absent, listed by character card 1.2. No new route, frame, event kind or config key.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-19-vehicles-actions-species-design.md` §4
  (`ask_dancing`),
  `2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md`
  (`dance_together`; an event that carries the words of the action that
  placed it), `2026-11-28-dance-massage-and-passion-actions-design.md`
  (`dance_naked_together`; `ask_for_dance` removed),
  `2026-11-27-passion-group-design.md` (an optional `name` on a together
  action), `2026-10-28-outing-indulgence-needs-and-habits-design.md` §3
  (a roll made before the judge, whose outcome the judge is told).

## 1. Intent

The user can say which dance. Today the three dance actions say only that
there is a dance; with a style she, the judge and the picture know it is a
tango, a ballet or a pole dance.

And a persona can love or dislike three kinds of things: dancing, being
naked with the user, taking risks. Each is a card key that makes her take
such an action more readily, or turn it down more often.

## 2. Goals, non-goals, anti-goals

**Goals.**

- `ask_dancing`, `dance_together` and `dance_naked_together` accept an
  optional `style`: one of the 23 of §4, or `custom` with a `name`.
- The style is named wherever the action or its event is named: the
  judge's `[用户动作]` line, her reply's action context, the event's line
  while it lasts, the picture's state, the history routes.
- An action without a style reads, behaves and is stored exactly as today.
- `art_metadata.dance`, `naturism` and `risky` set to `love` make her
  accept an action of their kind two times in three before the judge is
  asked; set to `dislike`, refuse it two times in three (§7).

**Non-goals.**

- A rule on a style. No style is gated on the persona's world, on the
  pair's scopes or on a tier, and the `dance` key does not read the style:
  the judge decides as it does today, with the style in front of it.
  *(Owner's ruling: wording only.)*
- The `dance` key on `dance_naked_together`: whether she takes that one
  turns on more than a love of dancing; `naturism` reads it. *(Owner's
  ruling.)*
- The three keys in the judge's `[角色人格]` block, in any prompt, or in a
  life event's draw.
- `ask_for_dance`. It stays removed; a stored one still reads as a
  `dance_together` without a style.
- A change to what the three actions do: their gates, their events and
  durations, their tones, what a refusal costs, when a picture is shown.
- An outfit that follows the style. The engine sets no clothing for a
  dance; the composer reads the style in the state line and dresses her as
  it sees fit.
- A style on any other action, or on her own asks.

**Anti-goals.**

- A second list of the styles or of their words: one enum with its words
  in `eros-engine-core`, beside the tier labels' words.
- A second list of which action reads which key: one function in
  `eros-engine-core`.
- A user-written name reaching a prompt unquoted.

## 3. Approaches

1. **A fixed list plus `custom`** *(taken)*: `style` is an enum, and
   `custom` carries a free-text `name`. The engine words each style the
   same way for every caller, a client can render a picker from the list,
   and `custom` covers the rest. This is `give`'s shape: a fixed category
   and an optional name.
2. Free text only, as `sex_together`'s `name` is. Smallest, but every
   caller invents its own words for the same dance and the engine cannot
   list them.
3. One action type per style. Rejected: 22 times three action types, for a
   difference that is one word.

## 4. The styles

| `style` | words |
|---|---|
| `ballroom` | 交际舞 |
| `courtly` | 宫廷舞 |
| `sexy` | 性感的舞 |
| `tiktok` | TikTok 舞 |
| `house` | House 舞 |
| `hiphop` | 嘻哈舞 |
| `flamenco` | 弗拉明戈 |
| `oriental` | 东方古风舞 |
| `indian` | 印度舞 |
| `arabic` | 阿拉伯舞 |
| `ballet` | 芭蕾 |
| `jazz` | 爵士舞 |
| `latin` | 拉丁舞 |
| `tango` | 探戈 |
| `cossack` | 哥萨克舞 |
| `austronesian` | 南岛语族的传统舞 |
| `indigenous` | 原住民的传统舞 |
| `african` | 非洲舞 |
| `kpop` | 韩舞 |
| `pole` | 钢管舞 |
| `folk` | 民族舞 |
| `contemporary` | 现代舞 |
| `geisha` | 日本艺伎舞 |
| `custom` | 「{name}」 |

The list and `courtly` for 古雅 are the owner's; `oriental` is East Asian
traditional dance, not belly dance. *(Engine's calls: the keys and the
words.)*

## 5. The request

```json
{ "type": "dance_together", "style": "tango" }
{ "type": "ask_dancing", "style": "custom", "name": "机械舞" }
{ "type": "dance_naked_together" }
```

- `style` is optional on the three types and refused (422) on any other.
- `style: "custom"` requires `name`: 1 to 32 characters after trimming,
  no control characters, as `sex_together`'s plain `name` is checked.
- `name` with any other style, or without a style, is refused (422).
- An unknown `style` is refused (422).

The action is stored on the user row as it was sent, so the history
routes return `style` and `name` with it. A stored row without `style`
reads as an action without one. *(Engine's call: the name is plain text
in and out, as `give`'s is.)*

## 6. The words

The style's words take the place of 舞 in each phrase. A custom name is
quoted with 「」 so that it reads as data.

| where | no style (today) | with a style | custom |
|---|---|---|---|
| `ask_dancing`, the action | 想看你跳支舞 | 想看你跳探戈 | 想看你跳「机械舞」 |
| `dance_together`, the action | 想和你一起跳舞 | 想和你一起跳探戈 | 想和你一起跳「机械舞」 |
| `dance_naked_together`, the action | 想和你一起裸舞 | 想和你一起裸着跳探戈 | 想和你一起裸着跳「机械舞」 |
| `dancing_together`, the event | 和对方一起跳舞 | 和对方一起跳探戈 | 和对方一起跳「机械舞」 |
| `naked_dancing_together`, the event | 和对方一起裸舞 | 和对方一起裸着跳探戈 | 和对方一起裸着跳「机械舞」 |

*(Owner's pick: the style folded into the sentence.)*

The action's phrase is what the judge's `[用户动作]` line, her reply's
action context and every other reader of the action's phrase use. The
event's phrase is what the reply's and the judge's event lines and the
composer's `[此刻状态]` use, for as long as the event lasts and on the
turn that places it.

An event knows its style from the action on the user message that placed
it, as a `custom_together` event knows its words: the reads that fill the
event's words for `custom_together` fill the dance's style for the two
dance kinds. Once that message is gone the event reads without a style.
An accepted `ask_dancing` places no event; its picture reads the style
from the action's phrase, as it reads the request today.

## 7. The taste keys

`art_metadata.dance`, `naturism` and `risky` are each `"love"`,
`"neutral"` or `"dislike"`; absent or any other value reads as `neutral`,
as `outdoorsy` is read. Character card 1.2, which this release
introduces, lists them with the four keys of
`2026-11-28-dessert-soup-taste-habits-mean-and-drowsy-tones-design.md`.
*(Owner's ruling: the three values, `dislike` spelled so.)*

| key | the actions it reads |
|---|---|
| `dance` | `ask_dancing`, `dance_together`, with or without a style |
| `naturism` | every naturism-group together action, `dance_naked_together` included |
| `risky` | every thrill-group together action, and `bet` |

On a 1:1 turn whose action one of the keys reads, the engine rolls once
before the judge runs, unless a rule that refuses before the judge
already refused the action (a fight outside a fantasy world, a water
action during her period, and the rest). The `dislike` side is the habit
roll of a `give` (the same one in three, the same forced refusal); the
`love` side is its mirror:

| the key | the roll | then |
|---|---|---|
| `love` | 2 times in 3 she takes it | the judge's `[用户动作]` line ends 「，这次你会答应」 and the response in effect is `accept`, whatever the judge answers |
| `love` | 1 time in 3 | the turn goes on as usual: the judge decides |
| `dislike` | 2 times in 3 she turns it down | the line ends 「，这次你不会答应」 and the response in effect is `refuse`, whatever the judge answers and with or without the LLM judge |
| `dislike` | 1 time in 3 | the turn goes on as usual |
| `neutral` | no roll | as today |

The line states the outcome, not a reason, and the judge writes its
`inner_state` for it. From there the turn is an ordinary accept or
refusal: an accept places the event and shows the picture as any accept
does; a refusal costs what a refusal of that action costs today (a bet's
costs nothing) and places nothing. The judge's own answer stays in its
audit payload. A re-driven turn rolls again, as the judge decides again.

*(Owner's rulings: the roll before the judge, two in three each way, the
actions each key reads. Engine's calls: that a rolled refusal costs as
any refusal, and the two endings.)*

## 7.1 What changes for a live user

Nothing until a client sends `style` or a card sets `dance`.

## 8. Tests

- `eros-engine-core`: each style's wire name; an action with and without a
  style serialises and reads back; a stored `ask_for_dance` reads as a
  `dance_together` without a style; every property of the three actions
  (event kind, group, refusal lines, the emotional list, the picture rule)
  is the same with a style as without.
- `eros-engine-server`, request: the 422 cases of §5; a `custom` name is
  trimmed and bounded.
- `eros-engine-server`, prompt: the fifteen phrases of §6 and the words of
  every style; a style-less action and event are byte-identical to today.
- `eros-engine-store`: the event reads return the style of a dance event
  and none for any other kind or once the message is gone.
- `eros-engine-core`: the taste roll: `love` accepts below two thirds and
  leaves the rest to the judge, `dislike` refuses from one third up,
  `neutral` never rolls; each action reads its own key and no other;
  actions no key reads never roll.
- `eros-engine-server`, prompt: each key reads its three values, and
  anything else as `neutral`.
- `eros-engine-server`, turn: a rolled accept ends the judge's line with
  「，这次你会答应」 and is `accept` although the judge answered `refuse`;
  a rolled refusal ends it with 「，这次你不会答应」, is `refuse` although
  the judge answered `accept`, and places no event; a rule that refuses
  before the judge wins over a `love`.
- `eros-engine-server`, turn: an accepted `dance_together` with a style
  names it on the judge's line, in the reply's context, in the picture's
  state on that turn and in the event line on the next turn; a refused one
  names it on the judge's line and places nothing.

## 9. Order of work

1. Code PR into `dev`.
2. The docs PR that already waits for v2.9.1 gains the styles:
   `api-reference`, `life-events-needs` and `model-config` (and their
   `.zh` twins), `eros-character-v1.2`, and a section in the v2.9.1
   migrating guide (clients: the optional field; cards: the key; crate
   users: the three variants gain a field).
3. The web client's issue for v2.9.1 lists the styles and the key.
4. Release v2.9.1.

## 10. Decisions

Owner's rulings: all three live dance actions take a style;
`ask_for_dance` stays removed; a style is wording only; the 23 styles and
`custom`; the style folded into the sentence; `oriental` is East Asian
traditional dance; 古雅 is `courtly`, 宫廷舞; the three taste keys have
three levels (`love` / `neutral` / `dislike`), roll before the judge two
in three each way; `dance` reads the two clothed dances, `naturism` the
naturism group, `risky` the thrill group and `bet`.

Engine's calls, open to reversal: the keys and the words of §4; `custom`
plus `name` as the request's shape, with the 32-character bound; plain
text for the name; no outfit rule; the two endings, and that a rolled
refusal costs as any refusal.

## 11. Left to the builder

- How the three `UserAction` variants and the style enum are shaped in
  Rust, as long as the stored and wire JSON is that of §5 and a row
  without `style` still reads.
- How the event reads carry the style to the prompt module.
- How a rolled accept is threaded beside the rules that refuse before the
  judge.
