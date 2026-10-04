# Chat user actions — Design

- **Date:** 2026-10-05
- **Status:** Draft — ready for review
- **Type:** Engine change. Public API gains one optional request field, one SSE
  frame and one history field. The PDE judge contract gains one nullable
  field. No migration, no new column, no new chat role.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn (SSE stream and async
  entry points), its decision, its reply prompt and its affinity write.

## 1. Motivation

A user talking to a persona can only send words. This change lets a turn
carry an **action**: handing the persona something (cigarettes, a drink,
legal medicine, in a quantity) or a physical gesture (kiss, hug, touch,
lick, or a free-form custom action). The persona accepts or does not, and
the relationship follows: an accepted action is graded like any other
moment, and a refused one costs the persona's relationship one tier on the
line the action touches.

## 2. Principles applied

1. **The judge decides, the engine folds.** Accept/refuse is a two-value
   PDE verdict field. The tier drop is engine math over the stored line
   scores; the judge never outputs a number.
2. **Three channels** (#332). The verdict reaches the reply prompt as a
   judged conclusion about this turn. Action turns roll no dice: the action
   already fixes what the turn is about, and a die would be a second voice
   on the same channel.
3. **One source per fact.** The action and its effective response live on
   the user row's `metadata.action`. Every model-facing rendering derives
   from it. The decision audit keeps the judge's raw answer, which is a
   different fact (it differs from the effective response whenever the
   fallback ran).
4. **Default-open.** Any action turn without a definite judge refusal is
   accepted. There is no measured fallback that would justify refusing on
   the engine's own authority.
5. **Prohibition by absence.** The reply prompt never contains 「拒绝」:
   `ANTI_REFUSAL_GUARD` (`prompt.rs`) tells the chat model that refusal
   phrasing in its state is data corruption, so the conclusion is worded as
   「你没有接受」.

## 3. API and storage

### 3.1 Request

`StreamSendRequest` (`routes/companion_stream.rs`) gains

```
action: Option<UserActionDto>
```

shared by `POST /comp/chat/{session_id}/message/stream` and
`POST /v2/comp/session/{sid}/message/async`. `UserActionDto` is internally
tagged on `type`, `snake_case`:

| `type` | fields |
|---|---|
| `give` | `item`: `cigarette` \| `alcohol` \| `medicine` (required); `name`: optional string, 1–32 chars; `quantity`: optional integer 1–99, default 1 |
| `kiss`, `hug`, `touch`, `lick` | none |
| `custom` | `text`: required string, 1–100 chars |

```json
{"content": "来，陪我喝点", "client_msg_id": "…",
 "action": {"type": "give", "item": "alcohol", "name": "威士忌", "quantity": 2}}
```

`name` is free text. The engine cannot verify that a named item is legal;
the category bounds what is being handed over, and the persona's own
judgement plus the safety iron rule cover the rest.

### 3.2 Validation

In `validate_payload`:

- `content` may be empty when `action` is present (today: only with a tip
  or an `image_url`).
- `action` together with `tips_amount_usd` → 400. Tip turns skip the PDE;
  action turns need it.
- `name` and `text` are trimmed and must be non-empty after trimming, within
  their length bounds counted in chars, and pass the `validate_prompt_traits`
  character rule (no `is_control` chars, no U+2028/U+2029).
- `quantity` outside 1–99 → 400.

`image_url` and `image` are allowed alongside an action.

### 3.3 Storage

`build_user_row_metadata` writes the validated action under
`metadata.action`, with `quantity` written explicitly on `give`:

```json
"metadata": {"action": {"type": "give", "item": "alcohol", "name": "威士忌", "quantity": 2}}
```

`content` holds the user's own text verbatim, possibly empty. Unlike a tip,
no placeholder text is synthesized: the action's wording is always derived
from `metadata.action` (§5.3).

Once the decision is final (§4.3), the effective response is written back:

```sql
UPDATE engine.chat_messages
SET metadata = jsonb_set(metadata, '{action,response}', to_jsonb($2::text))
WHERE id = $1
```

as a new `ChatRepo` method. `response` is `"accept"` or `"refuse"`. It is
absent while the turn is undecided or when the turn failed before the
decision. A re-driven turn re-decides and overwrites it; the last decision
is the one its reply was written against.

The async path carries `action` through `QueuedTurnParams` the same way it
carries `tips_amount_usd`, so `drive_turn` rebuilds the event with it.

### 3.4 Stream frame and replay

`ProtocolFrame` gains

```
ActionResponse { user_message_id: String, response: ActionResponse }
```

serialized as `{"type":"action_response","user_message_id":"…","response":"refuse"}`.
It is emitted exactly once per action turn, after §4.5's write and before
the first `meta` frame. `replay_stream` emits it in the same position when
the user row carries `metadata.action.response`.

### 3.5 History

`ChatHistoryEntry` (`routes/companion.rs`) and `BffHistoryEntry`
(`routes/bff/companion.rs`) gain

```
action: Option<UserActionView>   // skip_serializing_if none
```

on `user` rows: the action fields plus `response` when present. Same
key-presence contract as `reply_to_message_id`: omitted on rows without an
action.

## 4. Decision

### 4.1 Core types

In `eros-engine-core::types`:

- `UserAction` — `Give { item: GiftItem, name: Option<String>, quantity: u8 }`,
  `Kiss`, `Hug`, `Touch`, `Lick`, `Custom { text: String }`.
- `GiftItem` — `Cigarette`, `Alcohol`, `Medicine`.
- `ActionResponse` — `Accept`, `Refuse`.
- `AffinityLine` — `Bond`, `Chemistry` (in `affinity.rs`).
- `UserAction::line()` — `Give` → `Bond`; every other variant → `Chemistry`.

Core has no `utoipa` dependency, so the server keeps `UserActionDto` /
`UserActionView` as `ToSchema` DTOs with conversions to and from the core
type.

`Event::UserMessage` gains `action: Option<UserAction>`. `ActionPlan` gains
`action_response: Option<ActionResponse>`. Every struct-literal construction
of either (stream, proactive, image edit, tests) fills the new field.

### 4.2 Judge context and verdict

`build_pde_ctx` adds one line on action turns, worded from the persona's
side with the phrase table of §5.1:

```
[用户动作] 对方递给你 2 杯威士忌
```

`PdeVerdict` gains `action_response: Option<ActionResponse>`
(`serde(default)`). `pde_response_format` adds

```
"action_response": {"type": ["string", "null"], "enum": ["accept", "refuse", null]}
```

to `properties` and to `required`.

The reference `filter_prompt` in `examples/model_config.toml` gains:

```
#  "action_response": 上下文里有 [用户动作] 时填 "accept" | "refuse",按你此刻和对方的关系、
#    你的人设和这个动作本身决定收不收、让不让;没有 [用户动作] 就写 null;拿不准也写 null。
```

The guidance deliberately does not tell the judge what a refusal costs. The
judge decides in the persona's place; knowing the penalty would bias it
toward accepting.

### 4.3 Finalisation

| turn | `plan.action_response` |
|---|---|
| no action | `None`; a stray judge value is ignored |
| action, judge said `accept` / `refuse` | the judge's value |
| action, judge said `null`, the judge failed, or the deployment runs the rule PDE | `Accept` |

`plan_for` takes the verdict's `action_response` and the event, and applies
the table. `pde::decide` gains an action-turn branch beside the tip branch
that returns a `ReplyText` plan with `action_response: Some(Accept)`.

### 4.4 Action constraints

On an action turn, `guard_action` turns `Ghost` and `ProductQa` into
`ReplyText`. The persona answers an action; a refusal is played in the
reply, never by going silent. Image actions are unaffected: the judge may
choose `reply_text_image`. The existing ghost vetoes and kill-switch are
unchanged for every other turn.

### 4.5 After finalisation

Once the plan is final (after `guard_action`, `plan_for`, the ghost
kill-switch and the forced-image override) and before the reply branch:

1. Write `metadata.action.response` (§3.3).
2. Emit the `action_response` frame (§3.4).
3. `VerdictAudit` gains `action_response: Option<&str>`, the judge's **raw**
   answer (`null` when it abstained). The audit row is still written only
   when the judge ran. It records what the judge said, so a `null` here next
   to an `accept` on the user row identifies a fallback turn.

A write failure in step 1 is logged and does not fail the turn; the frame
and the reply still go out.

## 5. Prompt and history

### 5.1 `[user_action]`

`build_reply_request` appends a per-turn fragment after `build_prompt`, in
the same place and manner as `tips_reaction_context`, outside the cached
prefix:

```
[user_action]
对方递给你 2 杯威士忌，你接受了。用你自己的话回应。
```

```
[user_action]
对方凑过来想亲你，你没有接受。用你自己的话回应。
```

The event phrase, shared by this fragment, the judge line (§4.2) and the
history marker (§5.3):

| action | phrase |
|---|---|
| `give` | 递给你 {quantity}{unit}{name, else the category word} — unit 支 / 杯 / 片 and category word 烟 / 酒 / 药 for cigarette / alcohol / medicine. 「递给你 2 杯威士忌」「递给你 1 支烟」 |
| `kiss` | 凑过来想亲你 |
| `hug` | 想抱你 |
| `touch` | 伸手想摸你 |
| `lick` | 想舔你 |
| `custom` | {text} |

The fragment and the judge line prefix the phrase with 「对方」, except
`custom`, which renders as 「对方的动作：{text}」 because the user's text
is not guaranteed to read as a verb phrase. Physical phrases say what the
user *wants* to do, because the conclusion may be that it did not happen.
The conclusion is 「你接受了」 or 「你没有接受」.

### 5.2 Dice

On an action turn the roll is skipped and `plan.nudges` stays at its
default, so `[this_turn]` is absent. The roll condition at plan finalisation
becomes "no `reply_mode`, a text-bearing action, and no user action".
`reply_mode` itself is carried as the judge gave it.

### 5.3 History marker and the consumers of user text

One renderer turns `metadata.action` into a marker from the user's side,
placed before the user's own text:

```
（递给你 2 杯威士忌）来，陪我喝点
（凑过来想亲你）
```

`custom` renders as 「（{text}）」. The marker carries no outcome: the
persona's reply that follows is the record of what happened.

Every path that hands a user row to a model uses it:

- the reply model's history: `model_facing_user_text` (after the image
  preamble, if any);
- the PDE judge transcript (`JudgeTranscriptAcc::push`) and the input-filter
  transcript (`build_input_filter_transcript`);
- post-process: the `user_msg` that `post_process::run` derives from the
  event feeds the affinity judge, memory and insight extraction, so the
  current turn's marker is folded there once.

The affinity judge's `short_user_msg` gate (`eval_skip_reason`) never skips
an action turn. The input filter rewrites `content` only; on an action-only
turn `content` is empty and the filter skips as it does today. Echo
cancellation is unchanged: identical action-only rows repeated in the
window drop out of history, as identical tip rows do.

## 6. Affinity

### 6.1 Accept

No new mechanism. The affinity judge sees the marker in the user text and
grades the turn on its usual 0–4 scale.

### 6.2 Refuse: one tier down, same relative position

The refused action's line drops one tier. The other line and the
warmth/patience levels follow the judge as usual.

A pure function in `eros-engine-core/src/affinity.rs`, using the same tier
bounds as `tier_index` (`lo = [0, 0.15, 0.35, 0.62, 0.90]`,
`hi = [0.15, 0.35, 0.62, 0.90, 1.0]`):

```
target(L):
  k = tier_index(L)
  k == 1 → 0
  pos    = (L − lo_k) / (hi_k − lo_k)
  target = lo_{k−1} + pos × (hi_{k−1} − lo_{k−1})
  target = min(target, hi_{k−1} − ε)        // L = 1.0 gives pos = 1, which would land back in tier 5
```

Both axes of the line scale by `target / L`, preserving their ratio; at
`L = 0` nothing moves. Since `target < L`, the factor is below 1 and no axis
can leave `[0, 1]`.

Example: chemistry `0.80` (intimacy `0.90`, tension `0.70`) sits at 64% of
tier 4. It lands at 64% of tier 3, `≈ 0.524`, with intimacy `≈ 0.589` and
tension `≈ 0.458`. At tier 1 the line goes to `0`.

### 6.3 Where it runs

`persist_with_event` gains `tier_drop: Option<AffinityLine>`, computed by
post-process as `Some(action.line())` when `plan.action_response ==
Some(Refuse)`. Inside the existing row lock:

1. time decay;
2. `grade_turn` and `apply_deltas`, the judge's grades applied to both lines
   as today;
3. for the refused line, both axes are set from the **pre-turn, post-decay**
   snapshot by §6.2, and that line's two axes are cleared from
   `pending_deltas`;
4. levels set, endpoints re-derived, `diff_labels`.

The judge's grades for the refused line are therefore discarded rather than
stacked on the drop. The drop is unconditional on a refusal: a skipped or
failed affinity eval still drops the tier.

### 6.4 Event row

- `event_type` stays `'message'`; no CHECK change.
- `context` gains `tier_drop: "bond" | "chemistry"`, so a reader can see why
  the stored `grades` and the line's movement disagree.
- `effective_deltas`, `effective_line_deltas`, `label_changes`,
  `state_before` and `state_after` record the drop with no further change.
- What was refused is not copied: `user_message_id` joins to the user row's
  `metadata.action`.

## 7. Data flow, one action turn

```
request.action          validate (exclusive with tips) → metadata.action on the user row
  → run_stream          Event::UserMessage { action, .. }
  → PDE judge ctx       [用户动作] line
  → verdict             action_response accept | refuse | null
  → guard_action        ghost / product_qa → reply_text
  → plan_for            response = verdict, else Accept
  → finalise            no dice; write metadata.action.response; frame action_response; audit raw
  → build_reply_request [user_action] fragment; history markers
  → post_process        marker in user_msg; affinity eval not short-gated;
                        persist_with_event(tier_drop = line on refuse)
```

## 8. Where it lives

| change | file |
|---|---|
| `UserAction`, `GiftItem`, `ActionResponse`, `Event` / `ActionPlan` fields | `core/src/types.rs` |
| `AffinityLine`, tier-drop target and axis scaling | `core/src/affinity.rs` |
| `decide` action branch | `core/src/pde.rs` |
| `set_action_response` | `store/src/chat.rs` |
| `tier_drop` in `persist_with_event` | `store/src/affinity.rs` |
| request field, validation, metadata, queue params | `server/src/routes/companion_stream.rs`, `routes/companion_async.rs`, `pipeline/chat_queue.rs` |
| history field | `server/src/routes/companion.rs`, `routes/bff/companion.rs` |
| judge line, verdict, schema, guard, finalisation, frame, replay, audit, transcripts | `server/src/pipeline/stream.rs` |
| `[user_action]` fragment, phrase and marker renderers | `server/src/prompt.rs`, `pipeline/handlers.rs` |
| `user_msg` marker, short gate, `tier_drop` | `server/src/pipeline/post_process.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |
| docs | `docs/api-reference.md` / `.zh.md`, `docs/affinity-model.md` / `.zh.md` / `.ja.md`, `docs/model-config.md` / `.zh.md`, `examples/model_config.toml` |

## 9. Configuration and rollback

No new flag. A deployment on the rule PDE accepts every action. A judge
whose `filter_prompt` does not mention `action_response` still sees the
`[用户动作]` line and, under `structured_output`, a schema that requires the
field: whatever it answers on an action turn is a real verdict, and `null`
falls back to accept (§4.3). Clients that never send `action` see no
change. Rollback is the previous image; rows written
with `metadata.action` stay readable as plain user rows.

## 10. Observability

- The user row answers what was done and what was decided.
- `companion_decision_events.payload.action_response` answers what the judge
  said; `null` beside an effective `accept` marks a fallback.
- `companion_affinity_events.context.tier_drop` marks the turns whose line
  was dropped, and the row's state columns show by how much.
- `PROMPT_LOG_DIR` dumps show the rendered `[user_action]` and markers.

## 11. Testing

- **core.** The tier-drop target at both bounds of every tier, at `1.0`, in
  tier 1 and at `0`; axis scaling preserves the ratio and stays in `[0, 1]`;
  `UserAction::line()`; `decide` on an action turn returns `ReplyText` with
  `Accept`.
- **store.** `#[sqlx::test]`: on a refused turn the line lands on the
  target computed from the pre-turn snapshot, the other line follows the
  judge, the line's `pending_deltas` are cleared and `context.tier_drop` is
  set; `set_action_response` writes the key and preserves the other
  metadata keys.
- **validation.** Exclusive with tips; empty `content` allowed with an
  action; the `name` / `text` / `quantity` bounds and the character rule.
- **stream.** A verdict with and without `action_response` parses; the
  schema lists it in `required`; ghost and product_qa become `reply_text` on
  an action turn; `null`, a judge failure and the rule PDE each finalise to
  `Accept`; non-action turns ignore a stray value; the frame goes out once,
  before the first `meta`, and replays; the audit records the raw answer;
  no dice on an action turn.
- **prompt.** Each phrase in the table; the two conclusions; no 「拒绝」
  anywhere in the fragment; the history marker before the user text and
  after the image preamble.
- **post_process.** An action-only turn is not `short_user_msg`; `user_msg`
  carries the marker; `tier_drop` is passed only on a refusal.
- **Regression.** New fields on `Event::UserMessage`, `ActionPlan` and
  `persist_with_event` change the form of existing call sites. No existing
  assertion is weakened or changes its expected value.
- `openapi.json` regenerated; the snapshot diff shows only the request
  field, the action / response schemas it introduces, and the history
  field.

## 12. Non-goals

- Voice turns, edit turns and `/open`.
- A downstream-configurable action catalogue.
- Any effect of `quantity` on affinity; it is context for the judge and the
  reply only.
- An outcome in the history marker.
- Changing the affinity judge prompt, the tier bounds, or any existing
  tuning knob.

## 13. Decisions taken

- **The PDE judge decides, before the reply**, over letting the reply model
  decide and classifying it afterwards (the verdict would arrive seconds
  after the reply and misread hedged replies) and over an engine threshold
  table (it would hard-code what the persona thinks).
- **Fallback is accept.** No measured fallback justifies refusing.
- **The judge is not told the cost of a refusal.**
- **Give → Bond, physical → Chemistry**, including `custom`.
- **Accept is graded by the affinity judge** with no floor and no fixed
  table.
- **A refusal lands at the same relative position one tier down**; tier 1
  goes to `0`. Chosen over the lower tier's top edge (one good turn undoes
  it) and its floor (a single refusal can erase most of a tier 4 line).
- **The refused line ignores the judge's grades** for the turn rather than
  taking the drop on top of them.
- **Fixed categories with an optional free-text name**, over a pure enum
  and over a downstream catalogue.
- **Metadata, not a new role.** A new role would widen the role CHECK and
  every `role IN ('user','gift_user')` filter for no behavioural gain.
- **The audit keeps the judge's raw answer**; the effective response has
  exactly one home, the user row.
