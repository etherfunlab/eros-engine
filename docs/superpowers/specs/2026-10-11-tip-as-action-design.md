# Tip as a user action — Design

- **Date:** 2026-10-11
- **Status:** Draft — ready for review
- **Type:** Engine change, **breaking**. The request field `tips_amount_usd`
  and the BFF history field of the same name are removed; a tip becomes the
  `tip` variant of the user action. One migration rewrites the existing tip
  rows and drops `gift_user` from the role CHECK.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn (SSE stream and async entry
  points), its decision, its reply prompt, its affinity write, and every
  reader of `role = 'gift_user'`.
- **Builds on:** `2026-10-05-chat-user-actions-design.md` (the action
  contract this design extends; referred to below as *the action spec*).

## 1. Motivation

A tip today bypasses the PDE judge: `pde::decide` returns a rule-made
`ReplyText` plan, and the reply model is told a red packet arrived. Nothing
in the decision layer separates a paid tip from the user typing 「给你一百万」.

A tip becomes a user action. The judge sees it as a structured event and
decides whether the persona accepts it, exactly as it decides on a drink or a
kiss. The separate tip path is removed in the same change.

## 2. Principles applied

1. **One shape per fact.** A tip is a `user` row whose `metadata.action` has
   `type = "tip"`. There is no second representation: the existing
   `gift_user` rows are rewritten into this shape, and the role is removed.
2. **The judge decides, the engine folds.** Accept / refuse is the action
   spec's `action_response`; the tip adds no verdict field.
3. **Ordinal in, not numbers.** No prompt carries the amount. The engine
   folds it into two discrete readings: an objective value (「相当于一个汉堡」)
   and a subjective weight (「对你来说算「有点多」的一笔」).
4. **Default-open.** Any tip turn without a definite judge refusal is
   accepted, as for every action.
5. **The engine judges, the caller settles.** A refused tip is reported; what
   happens to the money is the caller's decision.

## 3. API and storage

### 3.1 Request

`UserActionKind` gains `tip`. `UserActionDto` gains

```
amount_usd: Option<f64>   // tip only, and required there
```

```json
{"content": "", "client_msg_id": "…",
 "action": {"type": "tip", "amount_usd": 20}}
```

`StreamSendRequest` loses `tips_amount_usd`, on both
`POST /comp/chat/{session_id}/message/stream` and
`POST /v2/comp/session/{sid}/message/async`. The struct does not deny unknown
fields, so a caller still sending `tips_amount_usd` has it silently ignored.

### 3.2 Validation

In `UserActionDto::to_core`:

- `amount_usd` must be finite and in `(0, MAX_TIP_USD]`, with `MAX_TIP_USD`
  raised from `1_000_000` to `1_000_000_000`. The cap guards against
  malformed input; whether the money exists is the caller's concern.
- `amount_usd` on any other `type`, like `item` on a non-`give` action, is
  ignored.

`validate_payload` drops its tip checks:

- the tip arm of the empty-`content` exemption (an action already exempts);
- `action` together with `tips_amount_usd`;
- `image_url` together with a tip;
- a forced image together with a tip.

A tip therefore combines with `image_url` and `image` (forced or not) like
every other action. The old exclusions existed because a tip row was
`gift_user` and skipped the judge and vision; neither holds any more.

Every remaining rejection stays a 422 `unprocessable`.

### 3.3 Storage

A tip turn is a `role = 'user'` row. `content` is the user's own text,
verbatim and possibly empty; no placeholder is synthesized.
`build_user_row_metadata` writes the action as for any other:

```json
"metadata": {"action": {"type": "tip", "amount_usd": 20.0}}
```

and the action spec's `set_action_response` adds `response` once the decision
is final. `persisted_content_role` is removed; every user row is `user`.

The engine stamps `read_at` on a tip row when it hands the turn to the judge,
as for every text `user` row (migration 0053's writer); the `gift_user`
exemption no longer has a subject.

### 3.4 Core types

`UserAction` gains

```
Tip { amount_usd: f64 }
```

and drops `Eq` (it keeps `PartialEq`). `Event::UserMessage`,
`PersistedUserMessage` and `QueuedTurnParams` lose `tips_amount_usd`; every
struct-literal construction drops the field.

`UserAction::line()` becomes

```
refusal_line(&self) -> Option<AffinityLine>
```

`Give` → `Some(Bond)`; `Kiss`, `Hug`, `Touch`, `Lick`, `Custom` →
`Some(Chemistry)`; `Tip` → `None`.

### 3.5 Stream frame and history

No new frame or field. A tip turn emits the action spec's `action_response`
frame and replays it the same way. `ChatHistoryEntry` and `BffHistoryEntry`
report the tip through their existing `action` field, amount and `response`
included.

`BffHistoryEntry` loses `tips_amount_usd`, and its role documentation loses
`gift_user`. `ChatMessageSlim` loses the `metadata->>'tips_amount_usd'`
projection that fed it.

## 4. Decision

### 4.1 The judge runs

`run_stream` loses its tip special-casing:

- `resolve_pde()` is no longer skipped on tips; the judge runs on a tip turn
  like any other.
- `force_image` no longer excludes tips.
- The vision describe and the input filter lose their tip skips. The input
  filter keeps the action spec's skip for an action turn with blank
  `content`.

`pde::decide` loses its tip branch (and with it the `Tsundere` fallback style
for personas without a `tip_personality`). A tip turn on the rule PDE takes
the action branch: `ReplyText` with `Accept`.

### 4.2 Judge line

`build_pde_ctx` states the tip through the action spec's `[用户动作]` line:

```
[用户动作] 对方给你发了一个红包，相当于一个汉堡，对你来说算「有点多」的一笔
```

The persona block already carries 「打赏人格：…」 when the persona has a
`tip_personality`. The judge's `filter_prompt` is unchanged, and the judge is
still not told what a refusal costs.

### 4.3 Finalisation

As the action spec (§4.3–4.5): the judge's `accept` / `refuse` when it gave
one, `Accept` on `null`, on a judge failure and on the rule PDE; `ghost` and
`product_qa` become `reply_text`; no dice; the response is written to the row,
sent as the frame, and the judge's raw answer is audited.

## 5. Prompt and history

### 5.1 Value and weight

No model-facing text carries the amount. Two independent ladders over
`amount_usd`, both lower-bound inclusive:

**Value noun** (new, `tip_value_noun`). Bounds are in USD, the field's unit.
Each noun is an item whose Canadian retail price, converted at roughly 1.35–1.40
CAD per USD, falls inside its tier. No exchange rate exists in code; the rate
only justifies the choice of noun. The prompt names no country.

| `amount_usd` | noun | Canadian reference |
|---|---|---|
| < 10 | 一杯饮料 | coffee, bubble tea: CAD 3–8 |
| 10 – < 100 | 一个汉堡 | burger combo: CAD 15–20 |
| 100 – < 1k | 一块手表 | wristwatch: CAD 200–1,000 |
| 1k – < 10k | 一部新手机 | flagship phone: CAD 1,400–2,000 |
| 10k – < 100k | 一辆车 | new car: CAD 30k–60k |
| 100k – < 10M | 一套房子 | home: around CAD 700k |
| ≥ 10M | 一个国家 | none; deliberately hyperbolic |

**Weight adjective** (`tip_tier_adjective`, unchanged): 一般 / 有点多 / 超级多
/ 非常夸张 / 近乎不可思议 at the bounds 10, 100, 1k, 10k.

The value is objective and appears wherever the tip is mentioned. The weight
is the persona's feeling and appears only where the persona is addressed
(§5.2).

### 5.2 Renderings

The action spec's three renderers gain the `tip` arm:

| renderer | tip |
|---|---|
| `action_phrase` (history marker, inside 「（）」) | 给你发了一个红包，相当于{noun} |
| `action_subject` (judge line, `[user_action]`) | 对方给你发了一个红包，相当于{noun}，对你来说算「{adjective}」的一笔 |
| conclusion | 你接受了 / 你没有接受 (unchanged) |

`user_action_context` gains `tip_personality: Option<&str>`, read only on a
tip. With a personality the fragment's closing sentence names it:

```
[user_action]
对方给你发了一个红包，相当于一个汉堡，对你来说算「有点多」的一笔，你没有接受。代入你「傲娇」的打赏人格，用你自己的话回应。
```

Without one it closes with 「用你自己的话回应。」 as every action does. The
fragment never contains 「拒绝」.

`[tip_received]`, `tips_reaction_context` and `fmt_amount` are removed.

### 5.3 Consumers

Every consumer the action spec lists (§5.3: the reply history, the judge and
input-filter transcripts, post-process's `user_msg` for affinity / memory /
insight, the image composer's user line) renders a tip through the same
marker, so the amount reaches no model.

The history marker reads 「（给你发了一个红包，相当于一个汉堡）」. Every
`role IN ('user', 'gift_user')` predicate and `"user" | "gift_user"` match
narrows to `user`: among them the reply history, the judge and input-filter
transcripts, `compute_signals_for_session`'s count, and the `/read`
endpoint's predicate, which becomes `role <> 'user'`.

Voice (`render_prev_call`, the voice wire history) and the world story reader
(`story.rs`) read raw `content`. They drop `gift_user` from their role matches
and otherwise treat a tip row as they treat any action-only row today: blank.

## 6. Affinity

- **Accept.** No new mechanism. The affinity judge sees the marker in the
  user text and grades the turn on its usual scale, with no floor.
- **Refuse.** No tier drop. `tier_drop_for` returns
  `action.refusal_line()`, which is `None` for a tip, so `persist_with_event`
  receives `tier_drop = None` and `context.tier_drop` is absent. The turn is
  graded like any other.

The `short_user_msg` gate already never skips an action turn.

## 7. Migration `0066_tip_as_action.sql`

```sql
UPDATE engine.chat_messages
SET role     = 'user',
    content  = CASE WHEN content ~ '^\(打赏 \$[0-9]+(\.[0-9]+)?\)$'
                    THEN '' ELSE content END,
    metadata = (metadata - 'tips_amount_usd')
               || jsonb_build_object('action', jsonb_build_object(
                      'type',       'tip',
                      'amount_usd', metadata -> 'tips_amount_usd',
                      'response',   'accept'))
WHERE role = 'gift_user';

ALTER TABLE engine.chat_messages DROP CONSTRAINT chat_messages_role_check;
ALTER TABLE engine.chat_messages ADD CONSTRAINT chat_messages_role_check
    CHECK (role IN ('user', 'assistant', 'system_error'));

DROP INDEX engine.chat_messages_tips_amount_idx;
```

- Only the synthesized placeholder 「(打赏 $X)」 is cleared; a tip that carried
  the user's own text keeps it.
- `response = 'accept'`: every such turn was in effect accepted.
- `read_at` stays `NULL` on the rewritten rows; NULL reads as "no receipt"
  (0053), and no backfill is made.
- Every `gift_user` row carries `tips_amount_usd` (0027 removed the rest).
- Applied migrations are not edited; 0019's and 0053's comments about tips
  stay as history.

## 8. Data flow, one tip turn

```
request.action {tip}    validate amount → metadata.action on a user row
  → run_stream          Event::UserMessage { action: Tip, .. }; judge resolved
  → PDE judge ctx       [用户动作] 红包，相当于{noun}，对你来说算「{adjective}」的一笔
  → verdict             action_response accept | refuse | null
  → guard / finalise    reply_text; null → Accept; no dice;
                        metadata.action.response; frame; raw audit
  → build_reply_request [user_action] with tip_personality; markers with {noun}
  → post_process        marker in user_msg; tier_drop = None
```

## 9. Where it lives

| change | file |
|---|---|
| `Tip`, drop `Eq`, `refusal_line`, `Event` field removal | `core/src/types.rs` |
| tip branch removal | `core/src/pde.rs` |
| migration | `store/migrations/0066_tip_as_action.sql` |
| slim projection, `gift_user` predicates | `store/src/chat.rs`, `store/src/story.rs` |
| request field removal, DTO, validation, `persisted_content_role`, metadata | `server/src/routes/companion_stream.rs`, `routes/companion_async.rs`, `pipeline/chat_queue.rs` |
| BFF history field | `server/src/routes/bff/companion.rs` |
| judge skip, vision / filter / force-image skips, role matches | `server/src/pipeline/stream.rs` |
| ladders, renderers, fragment, `[tip_received]` removal | `server/src/prompt.rs`, `pipeline/handlers.rs` |
| `tier_drop_for` | `server/src/pipeline/post_process.rs` |
| role matches | `server/src/pipeline/voice.rs`, `pipeline/mod.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |
| docs | §10 |

## 10. Documentation

- A migrating guide in `docs/migrating/`, named for the release version, in
  English: `tips_amount_usd` → `action: {type: "tip", amount_usd}`; readers
  of `role = 'gift_user'` or `metadata.tips_amount_usd` move to
  `metadata.action` with `type = 'tip'`; a caller that accepts actions from
  its clients must strip a client-supplied `tip`, since the engine trusts the
  field as it trusted `tips_amount_usd`; a stale `tips_amount_usd` is
  silently ignored; a refused tip is reported through `action_response` and
  its settlement is the caller's.
- Every doc, example and README passage about tipping, swept by concept (tip,
  打赏, 红包, `gift_user`, `[tip_received]`), not by identifier:
  `docs/api-reference.md` / `.zh.md`, `docs/byok.md` / `.zh.md`,
  `docs/model-config.md` / `.zh.md`, `docs/llm-audit.zh.md`,
  `docs/affinity-model.md` / `.zh.md` / `.ja.md` (refusal costs by action
  type). `examples/personas/*.toml` keep `tip_personality`.

## 11. Rollback and deploy

- The engine image and the caller's switch to `action: {type: "tip"}` ship
  together. A caller still sending `tips_amount_usd` to the new engine has
  its tip ignored: the turn runs as plain text, or is rejected when
  `content` is empty. A caller sending `type: "tip"` to the previous engine
  has the request rejected.
- The migration is forward-only. The previous image on the migrated schema
  reads a tip row as an unparseable action: no marker, so an action-only tip
  shows as a blank user line. It cannot write a tip, because `gift_user`
  fails the CHECK. Rolling back therefore means the previous image, the
  caller's previous version, and one `ALTER` re-adding `gift_user` to the
  CHECK.
- An async tip turn enqueued before the deploy and driven after it runs as a
  plain turn: its queued `tips_amount_usd` is ignored, while the migration
  has already rewritten its row.

## 12. Observability

- The user row holds the tip, its amount and its effective response.
- `companion_decision_events.payload.action_response` holds the judge's raw
  answer on tip turns, which previously wrote no decision row.
- `read_at` on tip rows measures the pre-flight before the judge, as on
  every text user row.
- `PROMPT_LOG_DIR` dumps show the judge line, the `[user_action]` fragment
  and the markers; none contains the amount.

## 13. Testing

- **core.** `Tip`'s serde shape (`{"type":"tip","amount_usd":20.0}`);
  `refusal_line` for every variant, `Tip` → `None`; `decide` on a tip turn
  returns `ReplyText` with `Accept`.
- **prompt.** `tip_value_noun` on both sides of every bound; the tip
  phrase, subject and marker; the fragment with and without a
  `tip_personality`; no 「拒绝」 in the fragment; no `$` and no digit of the
  amount in the judge line, the fragment or the marker; the marker carries
  the noun and not the adjective.
- **validation.** `amount_usd` required on `tip`, finite, `> 0`,
  `≤ 1_000_000_000`; a tip with `image_url` and with a forced image is
  accepted; empty `content` with a tip is accepted.
- **stream.** A tip turn resolves and runs the judge; a `refuse` verdict
  sends a `refuse` frame and writes `response = "refuse"`; `null` and a judge
  failure finalise to `Accept`; no dice on a tip turn.
- **post_process.** A refused tip passes `tier_drop = None`; a refused
  `give` still passes `Some(Bond)`.
- **store.** `#[sqlx::test]` on the migration: a placeholder tip row becomes
  `user` with empty `content` and the `action` object, a tip row with user
  text keeps its text, other metadata keys survive, `tips_amount_usd` is
  gone; the CHECK rejects `gift_user`; the amount index is gone.
- **Removed tests.** Tests of the removed path are deleted or rewritten as
  tip-action tests: `[tip_received]` injection, `gift_user` writes and
  replays, the tip validation exclusions, `fmt_amount`, the BFF
  `tips_amount_usd` field, and tests that insert `gift_user` rows (including
  the 0027 migration test, which cannot insert under the new CHECK). Each is
  listed in the PR. No assertion on a surviving behaviour is weakened.
- `openapi.json` regenerated; the diff shows only the removed request and
  BFF fields, the `tip` kind, `amount_usd`, and the edited descriptions.

## 14. Non-goals

- Settling, refunding or verifying a tip's money.
- Markers in the voice and world story readers.
- Changing `tip_tier_adjective`, the judge's `filter_prompt`, the affinity
  judge prompt, or the action spec's tier-drop rule for other actions.
- An exchange rate or locale-dependent nouns.
- `assistant_action_type = 'gift_reaction'`: it has no writer and stays in
  its CHECK; it belongs to the gift event removed earlier, not to tips.

## 15. Decisions taken

- **A tip is a `UserAction` variant**, over a `give` item (an integer
  quantity and a name do not fit money, and its refusal rule differs) and
  over a separate tip field with its own verdict (two verdict fields and two
  frames for one concept).
- **The judge may refuse a tip; a refusal drops no tier.** The persona's
  answer follows the verdict; the relationship is graded as usual.
- **The four existing tip rows are rewritten, and `gift_user` leaves the
  CHECK**, over keeping a second shape readable or deleting the rows.
- **The removed request field is ignored, not rejected.** A 422 guard for a
  field that no longer exists would outlive its one caller's migration.
- **No amount reaches a model.** The value noun and the weight adjective are
  the engine's discrete readings of it.
- **Canadian prices** pick the nouns; the prompt names no country.
- **Seven value tiers, the top one hyperbolic**; the weight adjective keeps
  its five.
- **`MAX_TIP_USD` = 1,000,000,000**, so the top tier is reachable.
- **Tips may carry images**, forced or not, like every action.
