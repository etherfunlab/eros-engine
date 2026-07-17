# PDE product QA action (`product_qa`): out-of-character product answers, context-isolated

**Date:** 2026-07-17
**Status:** Design approved, ready for implementation plan
**Related:** `docs/superpowers/specs/2026-06-04-llm-based-pde-design.md` (the PDE
judge this action plugs into), `docs/superpowers/specs/2026-07-15-pde-reply-tone-design.md`
(latest verdict-schema evolution), migration `0030_chat_messages_channel.sql`
(the `channel` column this spec extends)

## Summary

Add a new PDE action **`product_qa`**: when the end user asks about the
downstream product itself ("这个 app 是什么？", "怎么收费？", "会员怎么取消？"),
the LLM judge routes the turn to a dedicated **product-QA executor** — an
independent, streaming LLM call on its own `[tasks.chat_product_qa]` model
chain, whose system prompt (`filter_prompt`, operator-authored) carries the
product documentation. The answer is delivered in a plain informational voice
(no persona injection — the companion steps out of character), persisted to
`chat_messages` with a **`channel = 'product_qa'` marker**, and made
**invisible to the companion brain**: short-term context, repetition mining,
conversation signals, dreaming, affinity evaluation, and insight extraction
all skip it. The client sees it fully — live SSE stream, replay, and history.

Hard enablement requirements (ratified):

1. The LLM PDE (`[tasks.pde_decision].filter_prompt`) MUST be enabled — the
   rule engine never produces `product_qa`, so with the judge off the feature
   is unreachable. A configured `[tasks.chat_product_qa]` with the PDE off
   logs a boot WARN and stays inert.
2. `[tasks.chat_product_qa].filter_prompt` is REQUIRED — block present with a
   blank prompt refuses to boot (the `insight_extraction` pattern).

Naming (ratified): action string **`product_qa`** (PDE verdict + wire frame),
task block **`[tasks.chat_product_qa]`** (chat-path executor prefix
convention: `chat_vision` / `chat_image_generation` / `chat_voice`), channel
marker **`'product_qa'`**.

## Background (verified against current code)

- The PDE judge is opt-in via `[tasks.pde_decision].filter_prompt`
  (`resolve_pde`), returns a strict-json_schema verdict
  (`pde_response_format`, `stream.rs:1529`) whose static `action` enum is
  `["reply_text","ghost","reply_image","reply_text_image"]` (`stream.rs:1541`),
  parsed into `PdeVerdict` (`stream.rs:1436`).
- Optional actions are gated by **availability context lines** in the judge
  ctx — `[图片能力] 本轮可发图=是/否` (`build_pde_ctx`, `stream.rs:1789,1818`) —
  plus a pure `guard_action` (`stream.rs:1678`) that degrades unavailable
  proposals to `ReplyText` (never upgrades).
- Idempotency/replay (`upsert_user_message_idempotent`, `chat.rs:578`): a
  retried `client_msg_id` replays the persisted assistant chain, replays
  ghost via the `ghost_decision` flag, or 409s when neither exists. A
  persisted assistant chain is therefore the replay-safety anchor.
- Short-term context: `recent_turn_pairs` / `recent_turn_pairs_before_message`
  (`chat.rs:367,430`) pair `user|gift_user → assistant` rows; orphan user rows
  are dropped. `recent_assistant_contents` (`chat.rs:484`) feeds
  `[avoid_repetition]`. Neither filters on `channel` today.
- Signals: `compute_signals_for_session` (`pipeline/mod.rs:57`) counts
  `role IN ('user','gift_user')` rows for `message_count` /
  `hours_since_last_message`, channel-blind today.
- Dreaming pulls the session log with `channel IS DISTINCT FROM 'voice'`
  (`dreaming.rs:170`).
- `chat_messages.channel` CHECK is `(channel IS NULL OR channel IN ('voice'))`
  (migration 0030). Voice assistant rows use `assistant_action_type='reply'` +
  `channel='voice'` (`chat.rs:894-912`) — the "channel marks the flavor,
  action type stays `reply`" convention this spec follows.
- The voice path (`voice.rs`) never runs the PDE and uses its own `history()`
  window on a separate voice-channel session — untouched by this spec.
- Tip turns skip the judge entirely (rule path) — a tip can never become
  `product_qa`.
- Latest migration is `0033` → this spec ships **one migration `0034`**.

## Goals / Non-goals

**Goals**

1. Judge-routed product answers from an operator-authored knowledge prompt,
   in a plain informational voice (no persona).
2. Zero pollution of the companion brain: context, signals, dreaming,
   affinity, insights never see product-QA turns.
3. Full client fidelity: live stream, disconnect-retry replay, and history
   all serve the answer.
4. Multi-turn follow-ups ("那多少钱一个月？") work: both the judge and the
   executor see the session's recent product-QA pairs.
5. Strict enablement gates (LLM PDE mandatory; `filter_prompt` mandatory).

**Non-goals**

- Voice-path support (no judge there).
- Rule-engine `product_qa` (LLM-only action).
- Per-tier gating, billing/quota logic, or analytics beyond the existing
  decision-event + usage audits.
- A retrieval/RAG layer — the product knowledge is a static prompt; anything
  fancier is downstream's business.
- Fixing the pre-existing "crash between user-row insert and assistant-row
  insert ⇒ permanent 409" window (unchanged parity with normal replies).

## 1. Decision path

### 1.1 Enablement (three gates)

| Gate | State | Behaviour |
| --- | --- | --- |
| `[tasks.pde_decision].filter_prompt` set | off | Judge never runs ⇒ `product_qa` unreachable. If `chat_product_qa` is configured, log one boot WARN ("chat_product_qa configured but LLM PDE disabled; feature inert"). |
| `[tasks.chat_product_qa]` block present | absent | Feature off. Judge ctx carries no product-QA lines; a hallucinated `product_qa` verdict degrades to `reply_text`. |
| `chat_product_qa.filter_prompt` non-blank | blank | **Refuse to boot** (mirror `insight_extraction`'s required-prompt check). |

### 1.2 Judge side (mirrors the `[图片能力]` pattern)

- `PdeAction` gains `ProductQa`; the static `pde_response_format` enum gains
  `"product_qa"`. Safe superset: old prompts never emit it, and the guard
  degrades it when unavailable.
- **Only when the task is fully enabled**, `build_pde_ctx` injects:
  - `[产品咨询] 本轮可答产品问题=是` — availability line. Not rendered at all
    when the feature is off (old deployments: zero prompt drift, zero token
    cost).
  - `[最近产品咨询]` — the session's most recent **3** product-QA pairs
    (`channel='product_qa'`), so elliptical follow-ups route correctly.
    Omitted when there are none.
- One store fetch per turn (feature-on only):
  `recent_product_qa_pairs(session_id, before_message_id, limit=3)` — a
  channel-filtered sibling of `recent_turn_pairs_before_message` (same
  pair-walk, `AND channel = 'product_qa'`). The result is **reused** for the
  executor's context when the verdict lands on `product_qa` (no second
  fetch).
- `guard_action` gains a `product_qa_available: bool` parameter (sibling of
  `image_executor_available`): `ProductQa` passes through when available,
  degrades to `ReplyText` otherwise. Downgrade-only, as ever.

### 1.3 Action plumbing

- `ActionType` (core) gains `ProductQa`. `pde::plan_for` gains a `ProductQa`
  arm: zero `affinity_deltas`, zero `energy_cost`, Neutral style, hints
  ignored — this turn is out-of-character, nothing downstream consumes the
  plan beyond routing (the arm skips post-process entirely).
  `ActionType::is_text_reply()` stays **false** for `ProductQa` (it is not a
  companion reply; the eval gate must not fire).
- The verdict's `inner_state` / `tone` are ignored on `product_qa` turns
  (there is no persona prompt to fold them into).

### 1.4 Execution arm (approach A — independent stream arm)

`run_stream`'s action match gains a `ProductQa` arm, sibling of `Ghost`. It
short-circuits the entire companion chain: **no** vision, **no**
input-filter, **no** persona prompt assembly, **no** output-filter, **no**
post-process (affinity evaluation + insight extraction), **no** dreaming
participation.

Executor call (streaming, modelled on the lean voice generator):

- Model chain: `[model] + fallback` from `[tasks.chat_product_qa]`,
  `retry_depth`-truncated, walked sequentially on transport failure.
- Messages: `system = filter_prompt` (product docs + answering rules),
  `user =` recent product-QA pairs (§1.2, formatted as `用户: …` / `回答: …`
  lines) + the current question.
- Streams deltas to the client as they arrive;
  `log_openrouter_usage("chat_product_qa", …)` on completion.

## 2. Persistence, context isolation, wire protocol

### 2.1 Persistence (persist-but-mark; ratified over "don't persist")

The original ask was "don't write the answer to `chat_messages`"; the
ratified design persists it **marked** instead — replay/idempotency and
client history then work for free, and the *intent* (don't pollute the chat
context) is enforced by query-side exclusion (§2.2).

- **Migration `0034`**: extend the `chat_messages.channel` CHECK to
  `(channel IS NULL OR channel IN ('voice','product_qa'))`.
  `assistant_action_type` stays `'reply'` (voice-row convention; its CHECK is
  untouched).
- The user's question row is inserted before the judge runs (channel NULL).
  On a guarded `product_qa` verdict, a new
  `mark_user_message_product_qa(user_message_id)` UPDATE stamps
  `channel='product_qa'` on it (idempotent, mirrors
  `mark_user_message_ghosted`).
- The answer persists as a normal assistant row: `role='assistant'`,
  `assistant_action_type='reply'`, `channel='product_qa'`,
  `user_message_id` linked, OpenRouter audit trio (model/usage/generation_id)
  stored.

### 2.2 Context-isolation invariant

> **A non-NULL `channel` row is invisible to the companion brain and fully
> visible to the client.**

Four reader changes, all to `AND channel IS NULL`:

1. `recent_turn_pairs` + `recent_turn_pairs_before_message`
   (`[recent_conversation]`; also the judge's shared transcript source — the
   judge's *companion* transcript loses product-QA rows automatically, and
   gets them back only via the dedicated `[最近产品咨询]` block).
2. `recent_assistant_contents` (`[avoid_repetition]`).
3. Dreaming's session-log pull (currently `IS DISTINCT FROM 'voice'` —
   equivalent for voice sessions, which contain only voice rows, and now also
   excludes `product_qa`).
4. `compute_signals_for_session` (`message_count` /
   `hours_since_last_message`) — product questions don't advance the ghost
   message-count floor or relationship-depth signals. (Plan must verify the
   voice path doesn't call this on voice sessions; believed text-path-only.)

`history()` (client-facing history + voice generator window) is **not**
filtered — the client keeps seeing the Q&A. Voice sessions never contain
`product_qa` rows (no judge on the voice path).

### 2.3 Replay / idempotency (free)

A retried `client_msg_id` finds a non-empty assistant chain → the existing
Replay branch streams the stored answer verbatim. The replay frame
synthesizer checks the chain rows' `channel == 'product_qa'` and reports the
Meta action as `product_qa` (not `reply`). No new outcome variant, no 409
regression.

### 2.4 Wire protocol

- `FrameActionType` gains `ProductQa` → serialized `"product_qa"`.
- Frame order matches a normal reply: `Meta(action=product_qa, model)` →
  `Delta*` → `Done(generation_id)` → `Final`. Downstream clients can render
  an "official info" bubble; clients must tolerate unknown action-type
  values (flagged as a client-contract addition in the API docs).
- The history projection should expose `channel` so clients can style
  historical product-QA rows (plan verifies whether it already does).

## 3. Config

`examples/model_config.toml` gains (commented out — default OFF):

```toml
# ── 产品问答 (chat_product_qa) ──────────────────────────────────────────────
# OPT-IN。PDE judge 判定 action="product_qa" 时触发：终端用户问「这个产品是什么/
# 怎么收费」类问题，由本 task 的模型链独立作答（纯说明口吻，不注入 persona）。
# 回答落库但 channel='product_qa'，对伴侣上下文/记忆/好感度完全不可见；
# 客户端实时流 / 断线重放 / 历史记录照常可见。
# 硬性前提：LLM PDE（[tasks.pde_decision].filter_prompt）已启用，否则本块被
# 忽略（boot WARN）。filter_prompt REQUIRED：块存在而 prompt 空白 ⇒ 拒绝启动。
#[tasks.chat_product_qa]
#model        = "anthropic/claude-haiku-4.5"
#fallback     = ["google/gemini-3.1-flash-lite"]
#retry_depth  = 1
#temperature  = 0.3
#max_tokens   = 800
#reasoning    = { enabled = false }
#filter_prompt = """
#你是 XX 产品的官方说明助手。以下是产品资料：
#…（产品定位、功能、价格、会员、退订方式等）…
#只根据资料作答；资料没有的信息明确说不知道，不编造。语气友好简洁，不扮演角色。
#"""
```

- New resolver `resolve_product_qa() -> Option<ResolvedProductQa>` mirroring
  `resolve_pde`'s shape (model / fallback truncated to retry_depth /
  temperature / max_tokens / reasoning / filter_prompt). `None` when the
  block is absent; **boot refusal** (not `None`) when present with a blank
  `filter_prompt`.
- `COMPAT_FIXTURE` gains a `chat_product_qa` entry (additive).
- The sample `pde_decision.filter_prompt` comment gains a routing paragraph
  for `product_qa`, mirroring the image-capability wording: ctx carries
  `[产品咨询] 本轮可答产品问题=是` only when available; never choose
  `product_qa` when the line is absent (it degrades, wasting tokens); route
  product/pricing/membership questions there instead of answering in
  character.

## 4. Failure handling

- **Executor chain exhausted / empty**: do **not** degrade to the companion
  Reply path — the companion doesn't know the product facts, and improvising
  is exactly what this feature exists to prevent. Use the existing
  `error_handling` fallback-text mechanism; whatever fallback text is emitted
  **must persist with `channel='product_qa'`** so replay/idempotency hold.
  (Plan verifies the fallback-persistence mechanics.)
- **Judge failure**: rule-engine fallback (unchanged); the rule engine never
  produces `product_qa`.
- Guard degrades a `product_qa` proposal to `reply_text` whenever the task is
  unavailable.

## 5. Audit

- `companion_decision_events`: the existing write covers it —
  `proposed_action='product_qa'`, `action` records the post-guard final
  action. The `action` column has no CHECK; zero migration.
- Executor usage: `log_openrouter_usage("chat_product_qa", …)`; the assistant
  row stores the audit trio.

## 6. Testing

- **model_config**: block absent → `None`; block present + blank prompt →
  boot refusal; PDE off + block present → WARN and inert; compat fixture.
- **guard**: `ProductQa` available → passes; unavailable → `ReplyText`.
- **ctx builder**: `[产品咨询]` line + `[最近产品咨询]` block rendered only
  when enabled; block omitted when no prior pairs.
- **Isolation invariant**: `channel='product_qa'` rows invisible to
  `recent_turn_pairs*`, `recent_assistant_contents`,
  `compute_signals_for_session`, and the dreaming log pull; still visible to
  `history()`.
- **store**: `recent_product_qa_pairs` pair-walk;
  `mark_user_message_product_qa` idempotence.
- **E2E stream**: judge → `product_qa` → frame order
  (`Meta(product_qa)`/`Delta`/`Done`/`Final`), both rows marked, post-process
  not run; follow-up turn's judge ctx contains the first Q&A pair.
- **Replay**: retried `client_msg_id` replays the stored answer with
  `Meta(product_qa)`.
- **Failure**: executor exhausted → fallback text emitted AND persisted with
  the channel marker.

## 7. Docs

- `docs/model-config.md` + `.zh`: the new task block, the three enablement
  gates, the isolation semantics.
- `docs/architecture.md` + `.zh`: PDE action table gains `product_qa`;
  `channel` semantics updated ("voice | product_qa; non-NULL = out of
  companion context").
- `docs/api-reference.md` + `.zh`: new `product_qa` frame action type +
  history `channel` field; note clients must tolerate unknown action types.
- `README` action/table mentions if any.

## 8. File-by-file change list

| File | Change |
| --- | --- |
| `crates/eros-engine-store/migrations/0034_chat_messages_channel_product_qa.sql` | **New** — extend `channel` CHECK to `('voice','product_qa')`. |
| `crates/eros-engine-core/src/types.rs` | `ActionType` gains `ProductQa`; `is_text_reply()` stays false for it. |
| `crates/eros-engine-core/src/pde.rs` | `plan_for` gains `ProductQa` arm (zero deltas, zero energy, hints ignored). |
| `crates/eros-engine-llm/src/model_config.rs` | `ResolvedProductQa` + `resolve_product_qa()`; blank-prompt boot refusal; `COMPAT_FIXTURE` entry. |
| `crates/eros-engine-server/src/main.rs` (or boot path) | Boot WARN when `chat_product_qa` configured with PDE off; boot refusal on blank prompt. |
| `crates/eros-engine-server/src/pipeline/stream.rs` | `PdeAction::ProductQa` + schema enum; `build_pde_ctx` availability line + `[最近产品咨询]`; `guard_action(product_qa_available)`; `ProductQa` stream arm (streaming executor, skip whole companion chain); `FrameActionType::ProductQa`; replay Meta mapping. |
| `crates/eros-engine-server/src/pipeline/mod.rs` | `compute_signals_for_session` gains `AND channel IS NULL`. |
| `crates/eros-engine-server/src/pipeline/dreaming.rs` | Log pull filter → `AND channel IS NULL`. |
| `crates/eros-engine-store/src/chat.rs` | `recent_turn_pairs*` / `recent_assistant_contents` gain `AND channel IS NULL`; new `recent_product_qa_pairs`; new `mark_user_message_product_qa`; assistant insert accepts the channel marker. |
| `examples/model_config.toml` | Commented `[tasks.chat_product_qa]` block; `pde_decision` sample-prompt routing paragraph. |
| `docs/model-config.md`/`.zh`, `docs/architecture.md`/`.zh`, `docs/api-reference.md`/`.zh`, `README*` | Per §7. |
| `openapi` artifacts | Regenerate if the frame action enum is part of the published schema. |
