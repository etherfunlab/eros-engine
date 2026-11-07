# Five actions, two give items, a `gluttony` habit, character card 1.1 — Design

- **Date:** 2026-11-08
- **Status:** Approved
- **Type:** Engine change. No migration. Additive HTTP changes: the chat
  `action` enum gains five values; the `give` item enum gains two; the
  character card accepts `spec_version` `"1.1"`. `Habits` gains a field.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-05-chat-user-actions-design.md` (actions, judge
  response, refusal lines), `2026-10-13-give-categories-design.md` and
  `2026-10-12-given-item-lingering-effects-design.md` (give items and their
  curves), `2026-10-28-outing-indulgence-needs-and-habits-design.md` (habit
  keys, `indulgence`, the habit roll), `2026-10-30-give-needs-romantic-
  choices-and-answer-receipts-design.md` (gives move needs),
  `2026-10-26-event-message-image-design.md` (a message that carries its
  picture), `2026-10-16-byoc-design.md` (the card). Everything there stands
  except where this document says otherwise.

## 1. Intent

Five more things the user can do to the persona, two more things to hand
her, one more habit for a persona to have, and a card version that names
the new key. Each addition is one more row in the tables that already
exist; no new mechanism, with one exception: a photo the persona lets the
user take comes back as a picture.

## 2. Actions

The chat `action` enum (`UserAction`, `UserActionKind`) gains five values.
All five carry no fields.

| `type` | 中文 | kind | the `[用户动作]` line and the history marker | accept | refuse | refusal drops |
|---|---|---|---|---|---|---|
| `pat` | 拍 | physical | 对方伸手想拍拍你 | 你接受了 | 你没有接受 | chemistry |
| `photo` | 拍照 | physical | 对方举起手机想给你拍张照 | 你接受了 | 你没有接受 | chemistry |
| `bluff` | 虚张声势 | social | 对方在你面前虚张声势，想唬住你 | 你信了 | 你没信 | nothing |
| `threaten` | 威胁 | social | 对方威胁你 | 你被吓到了 | 你没吃这一套 | nothing |
| `trick` | 捉弄 | social | 对方想捉弄你 | 你上当了 | 你识破了 | nothing |

### 2.1 Physical: `pat`, `photo`

Judged, answered, refused and recorded exactly like `kiss` through `tease`:
the judge's `action_response`, accept when it did not decide, a refusal
drops the chemistry line one tier, the reply prompt's `[user_action]`
states the outcome with 「你接受了 / 你没有接受」, and neither leaves a
journal moment.

### 2.2 Social: `bluff`, `threaten`, `trick`

The judge still answers `accept` or `refuse`, but the words mean whether
she falls for it. So the judge is told what the two answers mean on that
line, and the reply prompt states the outcome in those words:

```
[用户动作] 对方威胁你（accept：你被吓到了；refuse：你没吃这一套）
```

```
[user_action]
对方威胁你，你没吃这一套。用你自己的话回应。
```

The history marker before the user's text is the bare phrase, as for every
action: 「（威胁你）」「（在你面前虚张声势，想唬住你）」「（想捉弄你）」.

A refusal on a social action drops no affinity line: `refusal_line`
returns `None` for the three, as it does for `tip`. What a threat or a
bluff does to her is the affinity judge's to grade from the turn, as it
grades any other turn; the engine adds no fixed cost.

Undecided means accept, as for every action: with the judge off, failed
or answering `null`, she falls for it. The same fail-open rule the
physical actions have, chosen over a per-action default.

The meaning note is added where the judge's `[用户动作]` line is built in
`stream.rs`, not in `action_subject`, which also feeds the reply prompt's
outcome sentence and the composer text.

No journal moment, no needs movement, no roll before the judge.

### 2.3 The photo comes back as a picture

On a 1:1 text turn whose `photo` is accepted, in a deployment whose image
executor is available this turn (the same condition `guard_action` uses to
keep a judge's `reply_text_image`), the turn's action type is
`reply_text_image` with `image_ref: face`, whatever the judge proposed:
her words first, then one delegated `image_request`, the shape an accepted
selfie event already has. The judge's `aspect_ratio` is kept when it
proposed an image action; otherwise there is none. The caller's `image`
params (`style`, `aspect_ratio`, `prompt_variant`) apply as on any image
turn; `image.force` still wins and still produces `reply_image`, as today.

The override sits where the judge's plan becomes the turn's plan: after
`guard_action` and the ghosting kill-switch, before the forced-image
override, so it applies with the LLM judge, with the judge's fallback and
with the rule engine alike (the rule engine accepts every action). It is
the response in effect that counts: an accepted `photo` whose response was
then changed by nothing else.

The composer needs no new input. Its `[最近场景]` already ends with the
user line carrying the marker and the outcome sentence
(「对方举起手机想给你拍张照，你接受了。」), which says what the picture is:
her, as the user's camera sees her, in what she wears this turn.

A refused `photo`, or an accepted one without an image executor, is a text
reply: on a refused `photo` the override forces `reply_text` even when the
judge proposed an image action, the one place an action turn drops a
judge's picture. Group turns carry no action, so nothing changes there.
The `chat_images` row and the audit source are those of any
`reply_text_image` turn.

The composer's recent-scene transcript is fetched today only on a judge
turn or a forced image; the fetch condition widens to an accepted `photo`
with an image executor, so the rule-engine path composes from the scene
too.

## 3. Give items

`GivenItem` and `GivenItemDto` gain two values.

| `item` | 中文 | group | unit | word | curve | needs moved | effect words, level 1–4 |
|---|---|---|---|---|---|---|---|
| `cuisine` | 料理 | consumable | 道 | 菜 | front (as `food`) | hunger −1, social +1, indulgence +1 | 回味还在 / 吃得挺满足 / 吃得正香 / 吃撑了 |
| `jewelry` | 首饰 | valuable | 件 | 首饰 | none | none | — |

`cuisine` is a proper dish or meal; `food` stays what it is, something to
eat. A dish moves needs as the things she is hooked on do (§4), not as
`food` does: it fills her and loosens her up. `jewelry` is a valuable like
`gem` or `gold`: no lingering effect, no needs movement, and it does not
join her items, since `gift` is the action for something she keeps.

The phrases read 「递给你 1 道菜」 and 「递给你 1 道「红烧肉」」, 「递给你
1 件首饰」 and 「递给你 1 件「珍珠项链」」.

## 4. The `gluttony` habit

A fifth habit key beside `smoking`, `alcoholic`, `drug` and `coffee`,
read the same way (`art_metadata.gluttony`, a boolean, anything else
reads as unset) and in every table the four are in:

| where | `gluttony` |
|---|---|
| `Habits` | gains `gluttony: Option<bool>`; `count_true` counts it |
| `indulgence` drift | counted with the others: three or more `true` is the fastest pace, as today |
| `needs_line`, `indulgence` high | may be picked, last in the pick list: 很想吃顿好的 |
| `Habits::uses` | `cuisine` → `gluttony` |
| the habit roll | a `give` of `cuisine` with `gluttony` explicitly `false` rolls the same 1/3 before the judge; refused, the line ends 「，这次你不会收下」, the response in effect is `refuse`, the give leaves no effect and costs bond a tier |
| the judge's `[角色人格]` | not listed, like the four |
| life events | not read |

`false` means she keeps off good food (忌口、节食); unset means nothing.
`food` is not the habit's item, as `tea` is not `coffee`'s.

## 5. Character card 1.1

`spec_version` `"1.1"` is accepted beside `"1"`; both parse to the same
`Card`, and the engine reads the same keys from either. Nothing about the
document's shape changes: `art_metadata` was already an open object, so a
1.0 card may carry `gluttony` too and the engine reads it. 1.1 is the
version whose documentation lists the key.

Docs: a new `docs/eros-character-v1.1.md` holds the full field tables as
`eros-character-v1.md` does, with `gluttony` in the habit row and a
*Versions* section that says 1.1 adds the key and that `"1"` is still
accepted. `eros-character-v1.md` gains one sentence in its *Versions*
section pointing at 1.1 and is otherwise untouched. `byoc.md` and
`byoc.zh.md` link to 1.1 as the current format.

## 6. Goals, non-goals, anti-goals

- **Goal:** every addition lands as a row in an existing table, in the
  same files the previous additions touched (`types.rs`, `prompt.rs`,
  `needs.rs`, `effects.rs`, `pde.rs`, `companion_stream.rs`, `openapi.json`,
  the docs).
- **Goal:** the photo picture reuses the `reply_text_image` path end to
  end: frames, persistence, audit, image edit.
- **Non-goal:** a new affinity rule for threats or tricks. The judge
  grades the turn.
- **Non-goal:** `food` joining the habit roll, or `gluttony` read by life
  events.
- **Non-goal:** any change to the card's shape, the sealing or the routes.
- **Anti-goal:** a photo turn that emits `image_request` without her
  words first, or one that emits a picture on a refusal.

## 7. Documentation

English and Chinese where both exist.

- `api-reference`: the action table (five rows; the refusal-drops sentence
  gains "a refused social action costs nothing"), the give item table, the
  effects curve table, the habit-roll paragraph, the persona settings
  table, the `photo` picture under the stream shapes, and the
  separation-period sentence that enumerates every action `type`.
- `affinity-model` *Refused actions*: social actions drop nothing.
- `model-config` *pde_decision*: the `[用户动作]` line's meaning note on
  social actions; the habit roll names `cuisine` / `gluttony`.
- `life-events-needs`: the `indulgence` row, drift table, gives-move-needs
  table, persona settings table, the craving words.
- `eros-character-v1.1.md` new, `eros-character-v1.md` *Versions*, `byoc`.
- `examples/model_config.toml` judge prompt comment on `action_response`.

## 8. Left to the builder

- Exact Rust names (`UserAction::Bluff`, `Photo`, …), test names, where
  the social-action meaning note is composed (`action_subject` or the
  judge line builder in `stream.rs`).
- The `reply_text_image` override's placement within the existing plan
  handoff, as long as §2.3's ordering holds.
- Whether `eros-character-v1.1.md` is a copy of v1 with the delta or a
  shorter document that defers to v1 for unchanged tables; the field
  tables must be complete in whichever it is.
