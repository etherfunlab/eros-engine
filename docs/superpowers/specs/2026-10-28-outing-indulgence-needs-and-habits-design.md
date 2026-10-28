# Needs: `outing` and `indulgence`, persona habits, and two events — Design

- **Date:** 2026-10-28
- **Status:** Approved
- **Type:** Engine change. No migration. Additive HTTP changes: `NeedsView`
  (the `body_state` frame, `GET …/effects`) gains two keys; the events
  route's kind enum gains two values. `Needs` gains two fields; `carried`,
  `next_change`, `load_needs` and `needs_line` gain parameters.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-13-persona-needs-design.md` (*the needs spec*),
  `2026-10-15-life-events-v2-design.md` (the draw and its factors),
  `2026-10-25-life-events-follow-ons-and-persona-keys-design.md` (the
  persona keys), `2026-10-05-chat-user-actions-design.md` and
  `2026-10-13-give-categories-design.md` (actions and give items).
  Everything there stands except where §4.1 says otherwise.

## 1. Intent

Two more needs give her day a direction: whether she wants to be out or
in, and whether she wants to let loose. Three persona keys say whether she
smokes, drinks or uses weed. They decide how she takes a give of one of
those, how fast the urge to let loose builds, and what it means for her.
Two `mid` events let the first need show up in her day.

The flow stays one-way at the engine level: gives and events reach the
judge's context, the judge rates needs, needs weight the draw. No engine
rule writes a need from an event or a give.

## 2. The needs

| key | 中文 | `low` | `high` |
|---|---|---|---|
| `outing` | 外出 | wants to stay in | wants to go out |
| `indulgence` | 享受 | no urge | wants to let loose: a smoke, a drink, weed |

### 2.1 Core

`Needs` gains `outing` and `indulgence`, after `emotional`. Both are
`#[serde(default)]`, and `NeedLevel` defaults to `Mid`: a rating or voice
snapshot stored before this change reads the two as `mid`. The original
four stay required; an object missing one of them still reads as no
rating.

### 2.2 Drift

| need | habits set to `true` | resting level | period |
|---|---|---|---|
| `outing` | any | `mid` | 8 h |
| `indulgence` | none | `mid` | 8 h |
| `indulgence` | 1 | `high` | 12 h |
| `indulgence` | 2 | `high` | 8 h |
| `indulgence` | 3 | `high` | 4 h |

The more of `smoking`, `alcoholic`, `drug` are `true`, the sooner the urge
comes back after the judge rated it down; `false` and unset count the same
as each other and never lower anything. A persona with no key `true`,
every persona written before the keys existed included, drifts as with no
habit. `carried` and `next_change` take the habits; `load_needs` takes
them from its caller, which already holds the persona (the text turn, the
effects route, the voice bootstrap, the event draw).

## 3. Persona habits

Three `art_metadata` keys beside `gender`, `health`, `is_virgin`:
`smoking`, `alcoholic`, `drug`. Each has three states, like `homesick`:
`true`, `false`, or unset (absent, or not a boolean). The engine never
writes them. In core they are `needs::Habits { smoking, alcoholic, drug }`,
each an `Option<bool>`. A persona written before the keys existed behaves
as today on gives (§3.1) and on drift (§2.2).

They reach three places: the give roll (§3.1), the drift of `indulgence`
(§2.2), and the words of a high `indulgence` in the reply (§3.2). The
judge's persona brief does not list them, so the persona does not talk
about tobacco, alcohol or weed at every turn. A persona that is meant to be
like that says so in her own persona text.

### 3.1 Gives

`Habits::uses(item)` returns the key for the item: `cigarette` →
`smoking`, `alcohol` → `alcoholic`, `herb` → `drug`; `None` for any other
item, and for one of the three whose key is unset.

When the turn's action is a `give` of one of those three and its key is
explicitly `false`, the engine rolls once before the judge runs. With
probability `UNUSED_SUBSTANCE_ACCEPT = 1/3` the roll leaves the turn alone
and the response is settled as today. Otherwise she refuses this give:

- the judge's `[用户动作]` line ends with 「，这次你不会收下」, so its
  `inner_state` is written for a refusal; the line states the outcome and
  no reason;
- the response in effect is `refuse`, whatever the judge answers;
- without the LLM judge, the rule engine's accept becomes `refuse`.

`true` and unset never roll. The response in effect is what the turn
persists and sends (`action_response` frame, `metadata.action.response`,
lingering effects, affinity, the reply's action line); the judge's own
answer stays in its audit payload. A re-driven turn rolls again, as the
judge decides again.

### 3.2 The reply's craving

`indulgence` at `high` names one thing she uses, chosen at random among the
keys that are `true`, so the model wants one thing rather than all three.
With no key `true` it is a general urge (§4.2).

## 4. Judge and reply

### 4.1 Judge

- The `[需求]` line gains `外出=…` on every judged turn, and `享受=高` only
  when `indulgence` is `high`:
  `[需求] 饥饿=高 社交欲=低 体力=中 情感需求=中 外出=中`,
  `[需求] 饥饿=高 社交欲=低 体力=中 情感需求=中 外出=中 享受=高`.
  This narrows the needs spec's rule that the line states every need: the
  absent `享受` reads as "not high", and the reference prompt says so.
  `[需求] 尚未判定` is unchanged.
- `needs` in the verdict stays nullable: `null` is still the judge
  abstaining, and the previous rating keeps drifting. When it is an object,
  the strict `response_format` requires all six keys.
- A verdict object that carries the original four but lacks `outing` or
  `indulgence` (a judge prompt not yet updated, or a non-strict judge that
  leaves out what its line did not show) keeps the levels it was shown for
  the missing ones, instead of resetting them to `mid`.
- The reference judge prompt in `examples/model_config.toml` gains:
  `outing 外出（high=很想出门，low=只想待在屋里）`、
  `indulgence 享受（high=很想放纵一下，来根烟、喝一杯之类；[需求] 行没有享受就是不高）`.

### 4.2 Reply

`needs_line(&Needs, Habits, pick)` renders, in the order hunger, social,
stamina, emotional, outing, indulgence:

| need | `low` | `high` |
|---|---|---|
| `outing` | 只想待在屋里 | 想出门走走 |
| `indulgence` | — | one of 很想抽根烟 / 很想喝一杯 / 很想抽口大麻, or 很想放纵一下 with no key `true` |

`indulgence` renders only at `high`. `pick`, uniform in [0, 1), chooses
among the `true` keys in the order smoking, alcoholic, drug. A text turn
rolls it per turn. A voice call derives it from the bootstrap snapshot's
`created_at`, so the block stays byte-stable for the whole call.

## 5. Events

### 5.1 Two `mid` kinds

| kind | label | weight | cooldown | lasts | cue |
|---|---|---|---|---|---|
| `want_to_go_out` | 想出门 | 1 | 24 h | 1 to 3 h | （你现在很想出门走走。） |
| `want_to_go_home` | 想回家 | 1 | 24 h | 1 to 3 h | （你现在很想回家。） |

Told incidents: not offline, no gate, no rule-out, she keeps replying.
Inside a background the cue opens with it as every incident does
(「你正在住院。你现在很想出门走走。」). They join `prefer_events`'
vocabulary like any `mid` kind and the caller's `kinds` like any kind.

### 5.2 Needs factors

| need level | kinds | factor |
|---|---|---|
| `outing` is `high` | `want_to_go_out` | ×3 |
| `outing` is `low` | `want_to_go_home` | ×3 |
| `indulgence` is `high` | `want_to_go_out`, `irritable` | ×2 |

The `outing` rows join `needs_factor`. The `indulgence` row is its own
second-stage factor so that it multiplies with the others: `outing` and
`indulgence` both `high` weigh `want_to_go_out` ×6; `indulgence` and
`emotional` both `high` weigh `irritable` ×4. The habits reach the draw
only through the drift of `indulgence`.

## 6. Out of scope

- Any engine rule that writes a need from an event or a give.
- The habits in any prompt line other than the craving's words.
- Raising the odds of a give she uses; the judge decides that as today.
- Events about smoking, drinking or weed themselves; follow-on factors for
  the two new kinds.

## 7. Testing

- **core.** Drift of `outing`; drift of `indulgence` for zero to three
  `true` keys, `false` and unset counting alike; `next_change` covers both
  under each. Serde: six keys out; a four-key object parses with both new
  levels `mid`; an object missing one of the original four still fails.
  `Habits::uses`. The give roll: with the key `false`, kept under 1/3 and a
  refusal at and above; with it `true` or unset, another item and another
  action, no refusal. The two kinds: category, wire value, rule, told,
  eligible in every background; each factor row through pinned rolls, and
  the two multiplying.
- **server.** `persona_habits` reads each key as `true`, `false` or
  unset, a non-boolean as unset. `needs_line`: the outing words,
  `indulgence` only at `high`, the general urge with no key `true`, `pick`
  choosing among the `true` keys. The `[需求]` line with `外出` always and
  `享受=高` only at `high`; the schema's six required keys inside a
  nullable object; a verdict lacking the two new keys keeps the levels it
  was shown. A give turn whose roll refuses: the judge's line carries
  「这次你不会收下」, and `refuse` reaches the frame, the row and the reply
  even when the judge answers `accept`; the rule engine refuses too. The
  two cues, alone and inside a background. The voice block's craving is
  the same on every turn of a call. `NeedsView` carries both keys;
  `changes_at` follows the habit-dependent drift.
- **Regression.** New fields on `Needs` and the new parameters change call
  forms. Expected values that change because two more keys or two more
  kinds now exist (serde shapes, the judge line, category counts, the
  `ADDED` list, the label list) are listed in the PR.
- `openapi.json` regenerated.

## 8. Documentation

In a follow-up PR: `docs/life-events-needs{,.zh}.md` (the needs tables,
drift, the habit keys, the give roll, the two kinds, the factor table),
`docs/api-reference{,.zh}.md` (`NeedsView`, the kind list, the keys),
`docs/model-config{,.zh}.md` (the `[需求]` line, the judge's `needs`, the
`[用户动作]` suffix).
