# Hot spring and fighting together, a challenge and a bet — Design

- **Date:** 2026-11-23
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: four new
  action types, two new event kinds (placed, never drawn).
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-11-23-twelve-together-actions-and-propose-design.md` (the together
  actions), `2026-11-08-five-actions-two-items-gluttony-card-1-1-design.md`
  §2.2 (the social actions), `2026-11-08-strip-and-world-setting-design.md`
  §4 (`setting`).

## 1. Intent

The user can ask her to soak in a hot spring or fight together, and can
challenge her or propose a bet. Fighting together exists only in a fantasy
world.

## 2. Goals, non-goals, anti-goals

**Goals.** Two together actions on the `sauna_together` path; two social
actions on the `bluff` / `threaten` / `trick` path; the first action limited
by the persona's world.

**Non-goals.** A winner for a challenge or a bet; a payload on either; a
`422` for an action the persona's world lacks.

**Anti-goals.** Costing the relationship for a fight a modern persona cannot
have: the world lacks it, she is not turning the user down.

## 3. Approaches

One way fits: hot spring and fighting together are together actions, and a
challenge and a bet are social actions, each reusing its existing path. A
generic action with a payload was ruled out in the previous design (one kind,
one label). For a challenge the owner weighed three outcomes (none, an
engine roll, the judge's verdict) and picked none: the conversation plays it
out. For a modern persona's fight the owner weighed a refusal that drops
chemistry, one that drops nothing, and a `422`, and picked a refusal that
drops nothing.

## 4. The actions

| action `type` | marker | accepted | refused | before the judge |
|---|---|---|---|---|
| `hot_spring_together` | 想和你一起泡温泉 | places `hot_spring_together` | drops chemistry | nothing |
| `fight_together` | 想和你一起去战斗 | places `fighting_together` | drops chemistry | refused when the persona's `setting` is not `fantasy`; that refusal drops nothing |
| `challenge` | 向你发起挑战 | `[用户动作]` accept: 你应战了 | refuse: 你没应战; drops nothing | nothing |
| `bet` | 想和你打个赌 | `[用户动作]` accept: 你答应打这个赌 | refuse: 你没答应; drops nothing | nothing |

None carries a payload: what the challenge or the bet is, the user says in
the turn's text. *(Owner's rulings: no winner, no payload, no drop on a
refused challenge; bet follows challenge.)*

**The two together actions** behave as `sauna_together` does in every other
respect: an accept places the kind in the write that stores the answer (no
event inside a date), the accepted turn is a picture turn, the judge is told
「这次你不会答应」 for a refusal ruled before it. *(Engine's calls: no
romance gate on the hot spring, as for the sauna; not emotional.)*

**A modern persona's `fight_together`** (`setting` absent reads `modern`):
refused before the judge, places nothing, shows no picture, and drops no line
whatever `refusal_line` says for a refused `fight_together` otherwise.
*(Owner's ruling.)*

**Challenge and bet** are social actions: `refusal_line` is `None`, the
`[用户动作]` line spells out both answers, no picture, not emotional.
*(Engine's call: not emotional.)*

## 5. The two kinds

| kind | label | category | lasts | fits |
|---|---|---|---|---|
| `hot_spring_together` | 和对方一起泡温泉 | `short` | 1 h | both settings |
| `fighting_together` | 和对方并肩作战 | `mid` | 1–2 h | `fantasy` only |

Both are `SPECIAL`: weight 0, no cooldown, `Neutral` place, never told, not
offline. Both reach the story director. *(Engine's calls: every value in
this table except what the owner named.)*

## 6. Order of work

One pull request at a time into `dev`: the code, then the docs. The docs add
both to the unreleased `v2-8-1` migrating guide rather than a new one. A
web-side issue lists the four action types and the two kinds.

## 7. Left to the builder

- Where the world check lives (beside `together_ruled_out`, taking the
  persona's traits) and how the drop is skipped for that one refusal
  (`post_process::tier_drop_for` decides the drop today).
- Rust names.
