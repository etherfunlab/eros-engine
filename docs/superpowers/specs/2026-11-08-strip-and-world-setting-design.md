# `strip` action, `strip` event, and the world `setting` — Design

- **Date:** 2026-11-08
- **Status:** Approved
- **Type:** Engine change. No migration. Additive HTTP changes: the chat
  `action` enum and the events route's kind enum each gain `strip`. The
  character card 1.1 gains the `setting` key. `DrawInput` and
  `PersonaTraits` gain fields.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-08-five-actions-two-items-gluttony-card-1-1-design.md`
  (the `photo` picture, card 1.1), `2026-10-05-chat-user-actions-design.md`,
  `2026-10-15-life-events-v2-design.md` (the draw, gates, factors),
  `2026-10-26-event-message-image-design.md` (an event message that carries
  its picture), `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`
  §4.1 (`romance_compatible`). Everything there stands except where this
  document says otherwise.

## 1. Intent

The user can undress the persona, and she can undress for the user. Both
belong only to a pair whose scopes are romance-compatible. Separately, a
persona now says which world she lives in, and the life-event draw keeps
to the events that fit that world.

## 2. The `strip` action

`UserAction::Strip` / wire `strip`, no fields. A physical action: judged,
answered and refused like `kiss`, a refusal drops the chemistry line one
tier, no journal moment.

| | |
|---|---|
| phrase / history marker | 伸手想脱你的衣服 |
| `[用户动作]` | 对方伸手想脱你的衣服 |
| outcome | 你接受了 / 你没有接受 |

### 2.1 Only between a compatible pair

The turn's own `memory_scope` and `affinity_scope` decide. When
`romance_compatible(memory_scope, affinity_scope)` is false, the turn goes
on as usual but is decided before the judge, as the habit roll is: the
`[用户动作]` line ends 「，这次你不会让对方这样做」, the response in effect
is `refuse` whatever the judge answers (the rule engine's accept too), and
the refusal drops chemistry a tier like any refused physical action. The
line states the outcome, not a reason. The audit keeps the judge's own
answer. No roll: incompatible always refuses.

### 2.2 Accepted: she is undressed, and the turn shows it

On an accepted `strip` the turn's clothing is 「衣服被对方脱掉了」, or
「「{outfit}」被对方脱掉了」 when the caller named an outfit. It replaces
whatever the judge wrote in `clothing`, so it reaches the reply prompt's
clothing line and the composer's `[此刻穿着]`. Later turns go back to the
judge, which reads the turn in history.

With an image executor this turn, an accepted `strip` is answered as the
accepted `photo` is: `reply_text_image`, `image_ref: face`, her words and
then one `image_request`. A refused `strip` is text only even when the judge
proposed a picture. `image.force` still wins on either outcome, as on a
photo: the turn is `reply_image`, and the picture follows the turn's
outcome, so on a refusal it shows her clothed. The `photo` override
(`photo_action`) generalizes to both actions, and so does the judge-less
scene fetch. The strip refusal of §2.1 is applied before that override, so
the override sees the response in effect.

## 3. The `strip` event

`EventKind::Strip` / wire `strip`: she undresses for the user and sends a
picture.

| | |
|---|---|
| category | `short` |
| weight / cooldown / lasts | 1.0 / 48 h / 30 min |
| gate | intimacy rung ≥ 3 (`DESIRE_MIN_RUNG`), **and** the session's scopes are romance-compatible |
| label | 刚脱了衣服 |
| cue with a picture | （你刚脱掉衣服拍了一张，发给了对方。） |
| cue without | （你刚脱了衣服，想让对方看看。） |
| picture | `is_photo()`: the event message carries one picture when the request has `image`, as `taking_selfie` does |
| follows | `sudden_desire` → `strip`, and `strip` → `having_sex` (`FOLLOW_FACTOR`) |
| `sex_addict` | scales it as it scales `sudden_desire` |
| setting | both (§4) |

Compatibility comes from the session's newest recorded scopes
(`ChatRepo::latest_scopes`), the source life choices already use; none
recorded reads as not compatible. The route passes the result to the draw
as `DrawInput.romance_compatible: bool`, and `passes_gate` checks it for
`strip`. The picture is composed from her message, as the selfie's is.

Her clothing on a `strip` event is 「刚把衣服脱了」, or 「刚把「{outfit}」脱了」
when the request named an outfit. It takes the place of the outfit as the
event plan's clothing, so it reaches her message's clothing line and the
composer's `[此刻穿着]`, and a named outfit never draws her dressed.

## 4. The world `setting`

`art_metadata.setting`: `"modern"` or `"fantasy"`. Absent, or any other
value, reads as `modern`, so every existing persona keeps today's draw.
`PersonaTraits.setting: Setting { Modern, Fantasy }`.

Each kind fits `modern`, `fantasy`, or both. The draw drops a kind that
does not fit the persona's setting before any weighting, whatever the
request's `kinds` allows. `prefer_events` naming a dropped kind has no
effect.

| fits | kinds |
|---|---|
| modern only | `police_encounter`, `showering`, `taking_selfie`, `gaming`, `watching_match`, `watching_show`, `business_trip`, `hospitalized`, `post_surgery` |
| fantasy only | none yet |
| both | every other kind, `strip` included, and the placed kinds `taking_space` and `fell_out` |

A fantasy persona's draw is therefore a subset of today's; fantasy-only
kinds can be added later as rows in this table.

The key is read for life events only. BYOC does not serve life events, so it
has no effect there yet, like the other life-event keys.

## 5. Character card 1.1

`setting` joins 1.1 beside `gluttony`. 1.1 is not released yet, so this is
the same version: `eros-character-v1.1.md` lists `setting` and its
*Versions* section says 1.1 adds `gluttony` and `setting`. No parse change.

## 6. Goals, non-goals, anti-goals

- **Goal:** `strip` (action) and `strip` (event) are rows in the existing
  tables; the only new rule is the compatibility gate, applied the way the
  habit roll and life choices already apply theirs.
- **Goal:** an existing persona with no `setting` draws exactly as today.
- **Non-goal:** a persistent clothing state across turns.
- **Non-goal:** an intimacy-rung gate on the `strip` action. The judge
  decides from the relationship.
- **Non-goal:** fantasy-only event kinds in this change.
- **Anti-goal:** a `strip` action or event that ever reaches a pair whose
  scopes are not romance-compatible as anything but a refusal or no draw.
- **Anti-goal:** a refused `strip` that adds a picture of its own. A
  caller's `image.force` is the only picture a refusal gets, and it follows
  the refusal.

## 7. Documentation

English and Chinese where both exist: `api-reference` (action table, the
compatibility rule, the picture paragraph and stream shape now naming
`photo` and `strip`, the events kind table and persona settings, the
separation-period list), `affinity-model` (the forced refusal), `model-config`
(the `[用户动作]` suffix), `life-events-needs` (the kind, its gate, follows,
`sex_addict`, the `setting` table), `eros-character-v1.1.md`.

## 8. Left to the builder

- Rust names (`Setting`, `fits`, …), where the per-kind setting table
  lives (a `fn fits(self, setting) -> bool` on `EventKind` is the obvious
  home), test names.
- The name of the pre-judge strip check (beside `refuses_give` in
  `pde.rs`) and how its suffix is threaded into the judge line beside the
  habit-roll suffix.
