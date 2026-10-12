# Lingering effects of a given item — Design

- **Date:** 2026-10-12
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration. The HTTP API gains one SSE frame and
  one read route; nothing is removed from it. `eros-engine-core` gains a
  module and nothing in it breaks.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn (SSE stream and async entry
  points): its judge context, its reply prompt and the chat image composer;
  the voice turn's prompt; the stream frames of both.
- **Builds on:** `2026-10-05-chat-user-actions-design.md` (*the action spec*)
  and `2026-10-12-persona-items-and-gift-design.md` (*the items spec*).

## 1. Motivation

A `give` action (cigarettes, alcohol, medicine) lasts one turn. The turn that
accepts it renders `[user_action]` 「…，你接受了」; from the next turn on the
persona sees only the history marker 「（递给你 2 杯「威士忌」）」, which says
neither that she took it nor that it is still doing anything.

An accepted `give` now keeps affecting the persona for up to five turns. Each
kind of item follows its own curve: a cigarette is strongest at first and
fades, alcohol builds up, medicine peaks in the middle. The judge, the reply,
the picture and a voice call all see the same state, and the caller receives
it to render.

## 2. Principles applied

1. **One source per fact.** The effect is derived from the user rows'
   `metadata.action` and its `response`. Nothing new is stored on a text
   turn; a re-driven turn derives the same state from the same anchor. A voice
   call freezes its copy in the call's existing bootstrap snapshot, which is
   how a call already holds everything it reads at its start.
2. **Ordinal words, engine math.** The model reads a description word and a
   direction, never a number. The curve tables are engine constants.
3. **Vague on purpose.** The description words say how strong, not what it
   feels like for a given substance. The reply model fills in the rest from the
   item's name, and a name the caller would not want spelled out gets no
   elaboration from the engine.
4. **Default-open.** Turns are the only clock; there is no wall-clock cut-off.

## 3. Turns and the active window

- A **turn** is one user row of a text session: `role = 'user'` and
  `channel IS NULL`. `product_qa` rows do not count (the persona never sees
  them), and voice rows live in voice sessions, so a call never advances the
  state.
- `N = min(quantity, 5)`. A `give` row k turns before the current one (k = 1 is
  the previous user row) is **active** when its stored `response` is `accept`
  and `k ≤ N`. It is then on step k of its curve.
- The turn that accepts a `give` is not step 1: its reply already has
  `[user_action]`. The effect starts on the next turn.
- A refused `give`, one whose `response` is absent (its turn failed before the
  decision, or has not been decided yet on the async queue), and any other
  action produce no effect.
- Effects render on the user's text turns (§6) and on voice turns (§8).
  `/open`, proactive messages and image-edit turns render none.

## 4. Curves

### 4.1 Levels

Four levels, rows indexed by step k = 1…N:

| N | cigarette | alcohol | medicine |
|---|---|---|---|
| 1 | 3 | 1 | 3 |
| 2 | 3 1 | 1 3 | 3 3 |
| 3 | 3 2 1 | 1 2 3 | 1 3 1 |
| 4 | 3 2 2 1 | 1 2 2 3 | 1 3 3 1 |
| 5 | 3 3 2 2 1 | 1 2 2 3 3 | 1 2 3 2 1 |
| quantity > 5 | 4 3 3 2 1 | 1 2 3 4 4 | 1 3 4 2 1 |

Quantity decides how long the effect lasts, and nothing else, except that a
quantity above 5 takes the last row, the only one that reaches level 4.
Stored quantities are 1–99 by validation; a `0` would select no row and
produce no effect.

### 4.2 Direction

One rule over the row, for every item: let the peak be the row's maximum.

| step k | direction |
|---|---|
| before the first peak | `rising` |
| at a peak | `peak` |
| after the last peak | `fading` |

So a cigarette is at its peak first and then fades, alcohol rises and ends at
its peak, medicine rises, peaks and fades. A one-step row is `peak`.

### 4.3 Core

A new module `eros-engine-core/src/effects.rs`:

```rust
#[serde(rename_all = "snake_case")]
pub enum EffectPhase { Rising, Peak, Fading }

#[derive(Serialize, Deserialize, ..)]
pub struct ActiveEffect {
    pub item: GivenItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub quantity: u8,
    pub level: u8,          // 1..=4
    pub phase: EffectPhase,
    pub step: u8,           // k
    pub steps: u8,          // N
}

/// The curve row for an item and a quantity (§4.1). Empty for quantity 0.
pub fn curve(item: GivenItem, quantity: u8) -> &'static [u8];

/// `prior` holds the turns before the current one, newest first: the action
/// each turn carried if it was accepted, else `None`. Returns the active
/// effects, newest first.
pub fn active_effects(prior: &[Option<UserAction>]) -> Vec<ActiveEffect>;
```

The tables are literal `const` arrays, one per item plus the quantity > 5 row,
so any cell can be retuned in place. The module is additive; no existing core
type changes. `ActiveEffect`'s serde shape is persisted in the voice snapshot
(§8) and mirrored by the wire DTO (§9).

## 5. Reading the rows

`ChatRepo` gains

```rust
pub async fn user_action_metadata(
    &self, session_id: Uuid, before: Option<Uuid>, limit: i64,
) -> Result<Vec<Option<serde_json::Value>>, sqlx::Error>
```

returning `metadata` of the newest `limit` turns (§3) of the session, newest
first. With `before = Some(anchor)` only rows strictly before the anchor are
read, anchored the way `history_up_to` is (`(sent_at, id) <` the anchor's,
session-scoped anchor subquery). Every caller passes `limit = 5`.

A shared helper maps the rows through `action_from_metadata` and
`action_response_from_metadata` to `Some(action)` when the response is
`accept`, and folds the list with `active_effects`.

| reader | session | `before` |
|---|---|---|
| text turn (§6) | the turn's session | the turn's user row |
| voice call (§8) | the pair's text session | `None` |
| `GET …/effects` (§9.2) | the path's session | `None` |

`before = None` reads the state a text turn sent now would carry.

On a text turn, `run_stream` reads once `DecisionInput` is built, on a
`UserMessage` turn. A failed read is logged at `warn` and reads as no effects;
the turn goes on. The effects are rendered once (§6.1) and the resulting
`Option<String>` is passed to the judge context, the reply request and the
composer, so all three see the same text.

## 6. Text turn prompt

### 6.1 Rendering

`prompt::body_state(&[ActiveEffect]) -> Option<String>`; `None` when empty.
Each effect renders as `{item}，{word}，{direction}`, joined by 「；」, newest
first.

- **item** — the `give` phrase without its verb: `{quantity} {unit}「{name}」`,
  or `{quantity} {unit}{category word}` without a name (「3 杯「威士忌」」,
  「1 支烟」). It is extracted from `action_phrase`'s `give` arm into a shared
  `given_phrase`, whose output that arm prefixes with 「递给你 」, so the
  existing phrase is byte-identical.
- **word** —

  | level | cigarette | alcohol | medicine |
  |---|---|---|---|
  | 1 | 余味还在 | 有一点酒意 | 药效轻微 |
  | 2 | 还有点飘 | 微醺 | 药效明显 |
  | 3 | 烟劲很足 | 醉意上头 | 药效很强 |
  | 4 | 抽得头晕 | 醉得厉害 | 药劲过猛 |

- **direction** — `rising` 还在往上走, `peak` 正在劲头上, `fading` 在慢慢退.

```
3 杯「威士忌」，微醺，还在往上走；1 支烟，烟劲很足，正在劲头上
```

### 6.2 Reply prompt

`build_prompt` gains `body_state: Option<&str>`. `[character_state]` gains a
line after 「此刻的穿着」:

```
[character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
- 你有的东西：「吉他」
- 此刻的穿着：红色旗袍
- 身上还带着：3 杯「威士忌」，微醺，还在往上走；1 支烟，烟劲很足，正在劲头上
```

- The line alone renders the block, as the outfit line does.
- `CHAT_NOISE_CANCELLATION_DISABLED` does not suppress it; like the items,
  wardrobe and outfit lines it is not a `character_insights` line.
- No effects ⇒ a prompt byte-identical to today's.

`build_reply_request` gains the same parameter and passes it through. The
stream path passes the rendered state; every other caller (`/open`, proactive,
image edit, tests) passes `None`.

### 6.3 Judge context

`build_pde_ctx` gains `body_state: Option<&str>` and, when present, adds

```
[身体状态] 3 杯「威士忌」，微醺，还在往上走；1 支烟，烟劲很足，正在劲头上
```

after the `[近期图片]` line and the product-QA lines, before `[用户动作]`. The
reference `filter_prompt` is unchanged: the line explains itself, and the
judge already weighs the whole context when it decides on an action or an
image.

## 7. The picture

- `compose_user_payload` gains a `[身体状态]` slot after `[此刻穿着]`; empty
  renders `（无）`, like every slot.
- `render_compose_payload` and `build_delegated_image_prompt` gain
  `body_state: Option<&str>`; the stream path passes the rendered state.
- `compose_inputs_json` records it as `body_state` (`""` when none).
- `POST /persona/{instance_id}/image/compose` passes `None` on both of its
  modes; its slot renders `（无）`.
- `DEFAULT_COMPOSE_PROMPT` names the new section among its inputs ("the
  character's bodily state right now") and adds: when that section is not
  `（无）`, let it show in the character's expression, complexion and posture.
  The image-edit composer is unchanged.

## 8. Voice

A call carries the state the persona was in when it was placed, and holds it
for the whole call.

- `VoiceBootstrap` gains `effects: Vec<ActiveEffect>` (`serde(default)`, so a
  marker frozen before this change reads as none).
- `assemble_bootstrap` gains a third part, loaded alongside the other two:
  the pair's text session is the session of the affinity row
  `AffinityRepo::load_latest_for_pair` returns (the row the call already reads
  its relationship line from); its effects are read with `before = None`
  (§5). No affinity row ⇒ no text session ⇒ no effects, and the part
  succeeds.
- A failed read fails the part: as with the other two, the turn injects what
  it has, the snapshot is not frozen, and the next turn retries.
- `render_bootstrap` renders, after `[上次通话]`,

  ```
  [身体状态]
  3 杯「威士忌」，微醺，还在往上走；1 支烟，烟劲很足，正在劲头上
  ```

  with the same `body_state` renderer, and the block counts as non-empty when
  only this part is present. The effects are stored structured, not
  rendered, because the frame (§9.1) needs them structured; the renderer is
  pure, so the prefix stays byte-stable for the call.
- Every voice turn emits the frozen state in a `body_state` frame (§9.1)
  before its first `delta`.
- The call does not advance the state: voice rows are not turns (§3), and the
  first text turn after the call reads the same rows the call did, plus any
  text sent during it.

## 9. Returning the state

### 9.1 SSE frame

`ProtocolFrame` gains

```
BodyState { user_message_id: Uuid, effects: Vec<ActiveEffect> }
```

```json
{"type": "body_state", "user_message_id": "…",
 "effects": [{"item": "alcohol", "name": "威士忌", "quantity": 3,
              "level": 2, "phase": "rising", "step": 2, "steps": 3}]}
```

- It reports the state the turn's reply was written with. The turn that
  accepts a `give` sends no frame for it; the next turn does.
- Sent only when `effects` is non-empty, once per turn: on the text stream
  after `action_response` (if any) and before the first `meta`; on a voice
  turn before the first `delta`.
- An idempotent replay of a text turn recomputes it from the rows, anchored at
  the replayed user row, and re-sends it in the same position.
- Clients must already tolerate frame types they do not know.

### 9.2 `GET /v2/comp/session/{session_id}/effects`

```json
{"effects": [{"item": "cigarette", "quantity": 1,
              "level": 3, "phase": "peak", "step": 1, "steps": 1}]}
```

- The state a text turn sent now would carry (`before = None`): for a page
  that opens on a conversation, and for async callers, which receive no
  frames. `{"effects": []}` when none.
- Right after a turn that accepted a `give`, this route already shows the
  give at step 1 while that turn's frame showed nothing: the frame describes
  the reply just written, the route the persona as she is now.
- Gate as `POST /v2/comp/session/{session_id}/open`, through
  `resolve_text_turn`: `404 session_not_found`, `403 session_forbidden`,
  `409 wrong_channel` for a voice session, and whatever else that gate
  rejects today.
- `EffectView` is the `ToSchema` DTO mirroring `ActiveEffect`, shared by the
  route and the documented frame shape.

## 10. Data flow, one `give`

```
turn T     give alcohol ×3   → judge accept → set_action_response: accept
           (no body_state frame; GET …/effects now shows step 1)
turn T+1   run_stream        user_action_metadata(session, Some(T+1), 5)
           active_effects    alcohol, N=3, k=1 → level 1, rising
           body_state        「3 杯「威士忌」，有一点酒意，还在往上走」
           → frame           body_state {level 1, rising, step 1/3}
           → PDE ctx         [身体状态] …
           → reply prompt    [character_state] - 身上还带着：…
           → (image turn)    composer [身体状态] …
   call    first voice turn  bootstrap freezes step 2 (level 2, rising)
           every voice turn  [身体状态] in the prompt; body_state frame
turn T+2   k=2 → level 2 微醺, rising (the call did not advance it)
turn T+3   k=3 → level 3 醉意上头, peak
turn T+4   k=4 > N → no effect, no frame
```

## 11. Where it lives

| change | file |
|---|---|
| curves, direction, `ActiveEffect`, `active_effects` | `core/src/effects.rs` (new), `core/src/lib.rs` |
| `user_action_metadata` | `store/src/chat.rs` |
| `given_phrase`, `body_state`, the `[character_state]` line, the fold helper | `server/src/prompt.rs` |
| `body_state` through `build_reply_request` | `server/src/pipeline/handlers.rs`, its callers |
| the read and fold, judge line, composer slot, inputs audit, `BodyState` frame and its replay | `server/src/pipeline/stream.rs`, `routes/companion_stream.rs` |
| bootstrap part, `[身体状态]` block, voice frame | `server/src/pipeline/voice.rs` |
| `GET …/effects`, `EffectView` | `server/src/routes/` (beside `/open`), `routes/mod.rs` |
| compose endpoint passes `None` | `server/src/routes/persona.rs` |
| composer prompt | `llm/src/model_config.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |

## 12. Documentation

Swept by concept (give, 递给你, effect, 身体状态, `[character_state]`, composer
slots, stream frames, voice bootstrap):

- `docs/api-reference.md` / `.zh.md`: under the `give` action, that an
  accepted give keeps affecting the persona for `min(quantity, 5)` turns, with
  the curves of §4.1; the `body_state` frame in the frame list and the frame
  sequences of the text and voice streams; the `GET …/effects` route.
- `docs/model-config.md` / `.zh.md`: the composer's slot list gains
  `[身体状态]`; the inputs the default compose prompt names.
- `docs/deploying.md` / `.zh.md`: the `CHAT_NOISE_CANCELLATION_DISABLED`
  parenthetical lists the new line among those that still render.
- `docs/llm-audit.md` / `.zh.md`, wherever `chat_images_events.inputs` keys are
  listed: `body_state`.
- Wherever the voice bootstrap's parts are described: the third part.

No migrating guide: the HTTP change is additive.

## 13. Rollback and deploy

No migration. Rolling back is the previous image: effects stop rendering, the
frame stops, and the route 404s. A voice marker frozen by this version keeps
its `effects` key, which the previous image ignores. The caller adopts the
frame and the route after the engine ships.

## 14. Observability

- A text turn's state is a pure function of stored rows, so any turn's effects
  can be recomputed from `chat_messages` with the turn's user row as anchor.
- A call's state is in `chat_sessions.metadata.voice_bootstrap.effects`.
- `chat_images_events.inputs.body_state` records what reached the picture.
- `PROMPT_LOG_DIR` dumps show the `[character_state]` line, the judge context
  and the voice prompt.

## 15. Testing

- **core.** Every cell of §4.1 (`curve`), including quantity 0, 6 and 99; the
  direction of every step of every row; `active_effects`: k = N is active and
  k = N + 1 is not, `None` entries and non-give actions skipped, several
  effects newest first, `step` and `steps`, quantity > 5 lasting five turns;
  `ActiveEffect`'s serde shape.
- **store** (`#[sqlx::test]`). `user_action_metadata`: newest first, user rows
  with `channel IS NULL` only, the limit; with an anchor, strictly before it
  and ignoring newer rows; without one, the newest rows.
- **prompt.** `given_phrase` with and without a name; `action_phrase`'s give
  output unchanged; every word and direction; the 「；」 join; the
  `[character_state]` line after the outfit, alone rendering the block, kept
  under the noise-cancellation flag; no effects ⇒ byte-identical prompt.
- **stream.** The judge line only when present and before `[用户动作]`; the
  composer slot with a state and as `（无）`; `compose_inputs_json` records
  `body_state`; a turn after an accepted give carries the state into the judge
  context and the reply request and sends one `body_state` frame after
  `action_response` and before the first `meta`; a turn after a refused give,
  and the accepting turn itself, send none; a replay re-sends it.
- **voice.** The bootstrap freezes the pair's text-session effects at the
  first turn and reuses them on later turns; no affinity row ⇒ none; a failed
  read leaves the snapshot unfrozen; `[身体状态]` renders after `[上次通话]`
  and alone; an old marker without `effects` reads as none; every turn sends
  the frame before its first `delta` when non-empty; voice rows do not count
  as turns.
- **routes.** `GET …/effects`: the state with the newest row at k = 1, `[]`
  when none, 403, 404, `409 wrong_channel`.
- **Regression.** New parameters on `build_prompt`, `build_reply_request`,
  `build_pde_ctx`, `render_compose_payload`, `compose_inputs_json` and
  `compose_user_payload`, and the new `VoiceBootstrap` field, change the form
  of existing call sites. No assertion is weakened. Composer-payload
  assertions that now include the `[身体状态]` slot, and `compose_inputs_json`
  snapshots that now include `body_state`, change their expected value; each
  is listed in the PR.
- `openapi.json` regenerated; the diff shows only the route and `EffectView`.

## 16. Non-goals

- A wall-clock cut-off.
- Quantity raising the peak beyond the quantity > 5 row.
- A flat curve: it is the shape a future `give` category would take, and no
  such category exists yet.
- Effects on `/open`, proactive and image-edit turns.
- Effects stacking into one figure per item; each give renders on its own.
- The state on history entries or on stored message rows.
- Any effect on affinity or on the accept/refuse decision beyond what the judge
  makes of the `[身体状态]` line.

## 17. Decisions taken

- **Derived per turn from history**, over storing remaining turns on the
  session (a second source that drifts on re-drives, the async queue and
  deletions) and over reusing the reply path's 20-row history (the judge and
  composer run without it, and 20 rows do not always reach five user turns).
- **The accepting turn is not step 1.**
- **Turns are the only clock.**
- **Four levels**, the fourth only for a quantity above 5.
- **A description word plus a direction**, over level words alone (the reply
  model sees one turn at a time and cannot tell rising from fading) and over
  description words alone.
- **Direction from the row's peak**, one rule for every item.
- **The judge and the picture see the state**, not only the reply.
- **A call carries the state it was placed in and does not advance it**,
  frozen in the bootstrap snapshot.
- **The state reaches the caller as a frame and a read route**, over a copy on
  the message rows. The frame reports the turn's own state; the route reports
  the state now.
