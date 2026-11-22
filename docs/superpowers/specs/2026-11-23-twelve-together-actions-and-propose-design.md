# Twelve together actions and a proposal from the user — Design

- **Date:** 2026-11-23
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: thirteen new
  action types, twelve new event kinds (placed, never drawn).
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md`
  (§5, the together actions; §7, her state in the picture),
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`
  (`sleep_together`, `propose`, the need move of a give),
  `2026-10-28-outing-indulgence-needs-and-habits-design.md` (the habit roll),
  `2026-10-12-given-item-lingering-effects-design.md`,
  `2026-11-08-strip-and-world-setting-design.md` (§2.1, the romance gate),
  `2026-11-12-emotional-action-replies-design.md`.

## 1. Intent

The user can ask her to do twelve more things together, and can propose to
her. Until now only she could ask the user to bed or to marry her; the two
new asks run the other way and count as the same thing having happened.

## 2. Goals, non-goals, anti-goals

**Goals.** Thirteen action types on the chat turn; each together action
behaves as `sauna_together` does today, plus the rules in §5–§7.

**Non-goals.** A new life choice of hers; a gate by tier or by warm turns
on any of the thirteen; a setting (`modern` / `fantasy`) rule on an action;
a journal milestone for a proposal or for sex; a story-director read of
proposals.

**Anti-goals.** Placing her solo kinds (`smoking`, `drinking`, `asleep`, …)
for a together action; a second stored copy of "the pair has been to bed"
or "the pair is engaged"; her asking the user to bed or to marry her after
she already accepted the same ask from the user.

## 3. Approaches

**The event an accept places.** Three ways: one new placed kind per action;
her existing solo kind where one exists (`using_toilet`, `smoking`,
`smoking_weed`, `drinking`, `streaking`, `asleep`); one generic "together"
kind carrying what they do as a payload. The first is taken. A solo kind's
label says she is alone, a row of it resets her own draw cooldown and feeds
the follow-ons, and `asleep` is an offline kind. A generic kind needs a
payload column and breaks one kind, one label.

**"The same as her own ask."** Two ways for an accepted `sex_together` or
`propose` to stop her own `sleep_together` or `propose` ask: write a
`life_choices` row at accept time, or have the "already settled" read look
at the user's accepted action too. The second is taken: the user row's
`metadata.action` is already the record of what was asked and answered, and
a `life_choices` row would be a second copy in a table that holds what she
asked.

## 4. The twelve together actions

No payload, like `kiss`. The DTO, the stored `metadata.action` and the
OpenAPI enum gain the values; the events route's kind enum gains the kinds.

| action `type` | the user (marker) | event kind | her state (label) | lasts |
|---|---|---|---|---|
| `toilet_together` | 想和你一起去上厕所 | `using_toilet_together` | 和对方一起上厕所 | 30 min |
| `smoke_together` | 想和你一起去抽根烟 | `smoking_together` | 和对方一起抽烟 | 30 min |
| `fall_asleep_together` | 想和你一起睡觉 | `asleep_together` | 和对方睡在一起 | 1–4 h |
| `sex_together` | 想和你上床 | `having_sex_together` | 和对方上床 | 1 h |
| `walk_together` | 想和你一起散步 | `walking_together` | 和对方一起散步 | 1 h |
| `smoke_weed_together` | 想和你一起抽大麻 | `smoking_weed_together` | 和对方一起抽大麻 | 30 min |
| `jog_together` | 想和你一起慢跑 | `jogging_together` | 和对方一起慢跑 | 1 h |
| `yoga_together` | 想和你一起做瑜伽 | `yoga_together` | 和对方一起做瑜伽 | 1 h |
| `stargaze_together` | 想和你一起看星星 | `stargazing_together` | 和对方一起看星星 | 1 h |
| `ride_horse_together` | 想和你一起骑马 | `horse_riding_together` | 和对方一起骑马 | 1 h |
| `streak_together` | 想和你一起裸奔 | `streaking_together` | 和对方一起裸奔 | 30 min |
| `drink_together` | 想和你一起喝酒 | `drinking_together` | 和对方一起喝酒 | 1 h |

`sleep_together` is already the wire name of her ask to go to bed, so the
literal sleep is `fall_asleep_together` and the user's ask to go to bed is
`sex_together`. *(Engine's call.)*

**Everything `sauna_together` does holds for all twelve** (spec 2026-11-20
§5):

- The judge decides. A refusal drops chemistry. *(Engine's call: the eight
  outings included, as for `sauna_together`.)*
- Accepted, the engine places the kind in the write that stores the answer:
  a running incident ends, a background keeps going, once per user row.
- Accepted inside a running date, no event is placed; the kind is her state
  for that turn's picture only.
- An accepted one is a picture turn when the image executor is available;
  a refusal never shows one. The composer is told the kind as her state.
- The twelve kinds are `SPECIAL`: weight 0, `Neutral` place, fit both
  settings, never told (a label, no cue), no cooldown. None is offline:
  during `asleep_together` she still answers. *(Owner's ruling for
  `asleep_together`: not offline, 1 to 4 hours, the span of her own
  `asleep`.)*
- Category `short`, except `asleep_together`, which is `mid`.

### 4.1 Ruled out before the judge

| action | refused before the judge when |
|---|---|
| `sex_together`, `streak_together` | the turn's scopes are not romance-compatible, as for `strip` |
| `toilet_together` | the persona is a machine (`robot`, `android`) |
| `smoke_together`, `smoke_weed_together`, `drink_together` | the habit roll refuses (§5) |
| the other six | never |

A refusal ruled here drops chemistry like any other. No tier floor and no
warm-turn rule on any of them. *(Owner's ruling for
`sex_together`; engine's call for the rest.)*

### 4.2 Tone

`sex_together` and `streak_together` reply in the `emotional` tone when the
deployment has it on, accepted or refused. The other ten do not. *(Engine's
call.)*

### 4.3 The story director

`having_sex_together`, `streaking_together`, `using_toilet_together` and
`asleep_together` stay out of the story director's `life_events`, as
`bathing_together` and `showering_together` do. The other eight go in, as
`sauna_together` does. *(Engine's call.)*

## 5. The three habit actions count as a give of one

`smoke_together`, `smoke_weed_together` and `drink_together` have her take
one `cigarette`, one `herb`, one `alcohol`. Wherever an accepted give of
that item at quantity 1 is read for her, the action reads the same
*(owner's ruling: exactly as a give)*:

- **The roll.** When the persona's key for the item (`smoking`, `drug`,
  `alcoholic`) is explicitly `false`, the action is refused before the
  judge unless a roll lands under `UNUSED_SUBSTANCE_ACCEPT` (one in three).
  An unset or `true` key rolls nothing.
- **The need move.** Accepted, the needs stored on the row are
  `after_give` for that item: `social` and `indulgence` one level up from
  the baseline.
- **The lingering effect.** Accepted, the row counts in
  `effects::active_effects` as that item at quantity 1. *(Engine's call.)*

Nothing else of a give carries over: a refusal drops chemistry (§4), not
bond, and no journal moment is written.

## 6. `sex_together` is her `sleep_together`, asked the other way

- **Her own ask.** `sleep_together` is settled for a session, and so never
  asked, when the session holds a `sleep_together` choice that is
  `accepted` or `ignored` (today's rule) **or** a user row whose action is
  `sex_together` with the answer `accept`. The read is of the user row, in
  the session or in a date's sub-session of it, so an accept inside a date
  settles it too.
- **Her ask already pending.** A `sex_together` turn resolves nothing, as
  every action turn: her pending ask stays as it is, to be answered or to
  expire, and is not asked again once the user row settles it.
- **Virginity.** The draw's `ever_had_sex` is true when the instance has
  any `having_sex`, `just_have_sex` **or** `having_sex_together` row. An
  accept inside a date places no row and so leaves it as it was.
- **Follow-on.** `having_sex_together` leads to `pregnancy_scare` as
  `having_sex` and `just_have_sex` do. *(Engine's call.)*
- A refusal drops chemistry, as the user's decline of her ask does.
- The user may ask again after an accept; the judge decides each time.

## 7. `propose`

A plain action, no payload: marker 向你求婚. Not a together action: no
event, no forced picture.

- **Who decides.** The judge. No rule before it: no romance gate, no tier
  floor. *(Owner's ruling: the judge decides; the romance gate applies to
  `sex_together`. Engine's call that `propose` carries none, as her own
  `propose` reads tiers and not scopes beyond the trigger's.)*
- **Refused.** Nothing drops. *(Owner's ruling.)*
- **Accepted.** Her own `propose` is settled for the session the way §6
  settles `sleep_together`: a user row whose action is `propose` with the
  answer `accept` counts, and her pending ask is left as it is.
- **Tone.** `emotional` when the deployment has it on, accepted or refused.
  *(Engine's call.)*
- The user may propose again, after a refusal or an accept; the judge
  decides each time.

## 8. Order of work

One pull request at a time, each into `dev`:

1. The code: §4–§7.
2. Docs: `api-reference` (action types, event kinds, adult-themes list),
   `life-events-needs` (the placed kinds, the habit actions, virginity,
   the follow-on), and a migrating note for the release.

A web-side issue lists the thirteen action types and the twelve kinds.

## 9. Migrating notes to write

- Thirteen action types; an accepted together action places an event and
  may show a picture.
- Twelve event kinds a client must be ready to read.
- A persona that accepted the user's `sex_together` or `propose` no longer
  asks the same herself.

## 10. Left to the builder

- Rust names; where the kinds go in `SPECIAL`.
- Whether the lists that name the together actions today (the picture
  turn, the placed kinds, `is_told`) are keyed on `together_kind()` instead
  of spelled out.
- How the action maps to its item for §5 (one function on `UserAction`).
- When the settled read of §6–§7 runs relative to the tier and warm-turn
  checks, and whether it needs an index.
