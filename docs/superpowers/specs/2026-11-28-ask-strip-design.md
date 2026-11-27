# The `ask_strip` action — Design

- **Date:** 2026-11-28
- **Status:** Approved
- **Type:** Engine change. No migration. Wire change, additive: one action
  type (`ask_strip`), no fields. No new route, frame, event kind, config
  key or card key.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-08-strip-and-world-setting-design.md` §2 (the
  `strip` action: the romance rule before the judge, the turn's clothing,
  the picture), `2026-11-12-emotional-action-replies-design.md` (the
  listed actions), `2026-11-27-swim-outfit-ladder-and-fish-together-design.md`
  §4 (what she wears set by the chemistry tier),
  `2026-11-08-five-actions-two-items-gluttony-card-1-1-design.md` §2.2
  (the judge told what each answer means).

## 1. Intent

`strip` is the user undressing her. `ask_strip` is its passive form: the
user asks her to undress, and she does it herself. Accepted, she does not
necessarily take everything off: how far she goes is set by the pair's
chemistry.

## 2. Goals, non-goals, anti-goals

**Goals.**

- `ask_strip` is `strip` in every rule but one: the romance rule before
  the judge, the judge deciding, the picture on an accept, the `emotional`
  reply, what a refusal costs (§4).
- On an accept, how far she undresses is one of five rungs, one per
  chemistry tier (§5).
- The judge is told what an accept means at this pair's tier (§6).

**Non-goals.**

- A rung set by anything but the chemistry tier: `is_virgin`, the bond
  tier, her needs and her events are not read. *(Owner's ruling.)*
- A state that lasts. As with `strip`, the turn's clothing is this turn's;
  the next turn she is dressed as the plan says.
- An event. `ask_strip` places none, as `strip` places none.
- A change to `strip`, or to the `strip` life event.

**Anti-goals.**

- An accepted `ask_strip` between a pair that is not romantically
  compatible.
- The rung in two places: one function gives it from the tier.

## 3. Approaches

1. **A new action type beside `strip`** *(taken)*: `ask_strip`, named as
   `ask_dancing` and `ask_massage` are. The two differ in who does it and
   in the outcome, so each keeps its own phrases.
2. A field on `strip` (`by: "her"`). Rejected: one type would have two
   outcomes and two sets of phrases, and stored rows without the field
   would need a default that is the old meaning.

## 4. What it shares with `strip`

| rule | `ask_strip` |
|---|---|
| before the judge | refused when the turn's `affinity_scope` is not romance-compatible, as `strip` is; the judge's line ends 「，这次你不会答应」 |
| otherwise | the judge decides |
| a refusal | costs the chemistry line a tier; text only |
| an accept | with an image executor, her words and then her picture, of her face reference, as `strip` |
| the reply's tone | `emotional`, accepted or refused, while `tones.emotional` is above 0 |
| the `erotic` tone, the passion attribute | not one of them, as `strip` is not |
| events, lingering effects, needs | none |
| group chat, BYOC | wherever `strip` works |

## 5. The rungs

The pair's chemistry tier, as the turn's prompt reads it, sets how far
she goes on an accept:

| chemistry tier | she | the turn's clothing |
|---|---|---|
| 1 | takes off her coat | 自己脱了外套 |
| 2 | strips to her underwear | 自己脱到只剩内衣 |
| 3 | strips to her panties, covering her chest with her hands | 自己脱到只剩内裤，上身用手遮着 |
| 4 | takes everything off, covering herself with her hands | 自己全脱了，用手遮着 |
| 5 | takes everything off and hides nothing | 自己全脱了，不遮不掩 |

*(Owner's pick: five rungs, these five.)*

The turn's clothing is what her reply and the picture read, in place of
the plan's clothing, as `strip`'s 衣服被对方脱掉了 is. At tier 1, when
the turn names an `outfit`, the clothing reads
自己脱了外套，里面还穿着「{outfit}」; from tier 2 up the outfit is off and is
not named. A persona who is not female reads tier 3 as
自己脱到只剩内裤. *(Engine's calls: the five strings, the outfit at tier
1, the tier-3 variant.)*

## 6. The words

| where | words |
|---|---|
| the action | 想让你把衣服脱了 |
| the judge's `[用户动作]` line | 对方想让你把衣服脱了（accept：{rung}；refuse：不脱） |
| her reply's action context | as any action: 你接受了 / 你没有接受. How far she went is the turn's clothing (§5) |

`{rung}` is the tier's words without 自己: 脱了外套 / 脱到只剩内衣 /
脱到只剩内裤，上身用手遮着 / 全脱了，用手遮着 / 全脱了，不遮不掩. The
judge reads what saying yes means for this pair, as it reads what each
answer means for `bluff`, `threaten` and `trick`. *(Engine's calls: the
type's name, these phrases, and telling the judge the rung.)*

## 7. Tests

- `eros-engine-core`: the wire name; `ask_strip` is refused before the
  judge exactly when `strip` is; it shares `strip`'s refusal lines, its
  place in the emotional list and in the picture rule; it has no passion
  attribute; the rung of each tier.
- `eros-engine-server`, prompt: the action's phrase, the judge's line at
  each tier, the five clothing strings, the outfit at tier 1, the tier-3
  variant.
- `eros-engine-server`, turn: an accepted `ask_strip` at tier 2 writes
  自己脱到只剩内衣 as the turn's clothing in the reply and the picture
  request, and records `accept`; an incompatible pair is refused before
  the judge with 「，这次你不会答应」; a refused one changes no clothing.

## 8. Order of work

A code PR into `dev` after the dance styles PR; the docs PR, the web
client's issue and the v2.9.1 release then cover it with the rest.

## 9. Decisions

Owner's rulings: the action is `strip`'s passive form; five rungs by
chemistry tier; the gate is `strip`'s; `is_virgin` is not read.

Engine's calls, open to reversal: the name `ask_strip`; the clothing
strings and the phrases; the outfit named at tier 1 only; the tier-3
variant for a persona who is not female; the judge told the rung; the
pre-judge refusal's ending 「，这次你不会答应」.
