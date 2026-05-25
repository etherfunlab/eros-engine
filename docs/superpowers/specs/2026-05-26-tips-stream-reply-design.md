# eros-engine — Tip-aware streaming reply (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.4.x` dev track; additive request field, no store migration
**Audience**: anyone implementing the `tips_amount_usd` field on the streaming chat path

---

## 0. Background

The frontend is building a tipping ("打赏") feature. After a user tips the
companion, we want the companion to react to the tip in its reply, with the
intensity of the reaction scaled to the dollar amount.

The engine already has a separate, deliberately LLM-free gift path
(`POST /comp/chat/{session_id}/event/gift`, `routes/companion.rs:812`) that
records a `gift_user` row + applies caller-supplied affinity deltas and returns
`reply: None`. It also has a dormant synchronous `GiftReaction` action
(`Event::Gift` → `pde.rs:36` → `build_gift_request`) that is **unreachable in
production** because `run_stream` only ever builds `Event::UserMessage`
(`stream.rs:537`). That `Event::Gift` line is "gift value → reaction" machinery
(amount + item kind + `tip_personality`) and is **out of scope here** — we are
not touching it. Its fate (keep / remove) is a separate later decision.

This spec adds a much smaller capability to the **existing streaming chat
endpoint**: it can carry a tip amount, the engine appends a short
amount-aware fragment to the system prompt, and the rest flows as a normal
chat reply.

---

## 1. Goal / Non-goals

**Goal:** `POST /comp/chat/{session_id}/message/stream` accepts an optional
`tips_amount_usd`. When present:
- the turn is allowed to carry no user text (a tip is a button tap),
- the engine appends a `【刚收到的打赏】` fragment to the reply's system prompt,
  containing both the literal dollar amount and a tier adjective,
- the turn is guaranteed a reply (never ghosted),
- everything else (affinity, persistence mechanics, streaming) behaves exactly
  like a normal chat turn.

**Non-goals / explicit boundaries:**
- **No affinity special-casing.** The normal `predict_reply_deltas`
  (`pde.rs:97`) micro-tweak still runs. We do **not** apply gift-specific
  deltas and do **not** write a `gift` row to `companion_affinity_events`.
- **No `Event::Gift` / `GiftReaction` / `PendingGift` / `tip_personality`.**
  The dormant gift-value reaction line is untouched.
- **No new endpoint.** The change rides on the existing stream endpoint.
- **No new chat role / no store migration.** The tip turn persists as a normal
  `role='user'` row so the existing replay/idempotency machinery is unchanged.
- **No engine-side tier menu.** The frontend's 5 preset buttons
  ($2/$20/$200/$2000/$20000) are a UI detail; the engine accepts an arbitrary
  positive amount and buckets it by order of magnitude.
- Old clients that never send the field are byte-for-byte unaffected.

---

## 2. Design

### 2.1 Request schema

`StreamSendRequest` (`routes/companion_stream.rs:70`) gains:

```rust
#[serde(default)]
pub tips_amount_usd: Option<f64>,
```

The number **is** USD (e.g. `20.0` = $20). We pass dollars because every LLM
understands USD magnitude semantics, so the model itself judges how big the tip
feels — the engine just supplies the number and a coarse adjective.

### 2.2 Validation (`validate_payload`, `companion_stream.rs:98`)

- When `tips_amount_usd` is `Some(a)`:
  - **content may be empty** (skip the non-empty check); all other content
    rules (≤ `MAX_CONTENT_CHARS`) still apply if content is non-empty.
  - require `a.is_finite() && a > 0.0 && a <= 1_000_000.0`. Reject otherwise
    with a `422 unprocessable` pre-stream error (bounds the value and prevents
    a garbage/huge number from polluting the prompt).
- When `tips_amount_usd` is `None`: current rules unchanged (content required).
- If **both** content is empty **and** `tips_amount_usd` is `None`: the existing
  `422 "请输入一条消息"` still fires.

### 2.3 Persistence (reuse `role='user'`)

A standalone tip has no user text, but the stream's idempotency + replay logic
keys on a `role='user'` row (`chat.rs:309`). Rather than add a new role (which
would force changes to `upsert_user_message_idempotent` and the replay path), we
**synthesize a marker content** and persist via the existing idempotent upsert:

```
content = "(打赏 $20)"      // fmt_amount(amount), see §2.6
```

- Replay / idempotency / `user_message_id` wiring is unchanged.
- The marker is human-readable; the frontend may special-render this bubble.
- Trade-off: in history the tip is a `user` message, not a distinct
  `gift_user` row. Acceptable for v1; a distinct role is deferred to the later
  "gift route keep/remove" decision.

If `content` is non-empty (tip riding on a typed message — allowed but not the
primary FE flow), persist the user's actual content unchanged.

### 2.4 Field threading

Add `tips_amount_usd: Option<f64>` to:
- `PersistedUserMessage` (`pipeline/stream.rs`) — set from the request at
  `companion_stream.rs:275`.
- `Event::UserMessage` (`eros_engine_core/src/types.rs:39`) with
  `#[serde(default)]`.

`run_stream` (`stream.rs:537`) sets the field on the foreground
`Event::UserMessage` from `user_msg.tips_amount_usd`. The background
`event_bg` construction (`stream.rs:688`) passes `None`. Existing `match` arms
that use `..` need no change; the few **construction** sites in tests
(`handlers.rs:1226`, `post_process.rs:730`/`:749`) get `tips_amount_usd: None`.

### 2.5 PDE guard — tip always replies, never ghosts

`pde::decide` checks ghost at step 2 (`pde.rs:46`), before the reply default at
step 5 (`pde.rs:85`). A tip routed as a normal `UserMessage` could therefore be
ghosted — taking money and going silent. Add a deterministic rule at the **top**
of `decide`:

```rust
// 0. Tip on a user message — always reply, never ghost. reply_style and
//    affinity deltas are identical to a normal reply; the only effect is
//    bypassing the ghost branch below.
if let Event::UserMessage { tips_amount_usd: Some(_), .. } = &input.event {
    return ActionPlan {
        action_type: ActionType::Reply,
        reply_style: ReplyStyle::Neutral,
        affinity_deltas: predict_reply_deltas(input),
        energy_cost: ENERGY_COST_REPLY,
        context_hints: vec![],
    };
}
```

This keeps "treat like a normal chat" (same style, same predictive deltas)
while guaranteeing a reply.

### 2.6 Prompt fragment + bucketing

New helper in `prompt.rs`, sibling to (but independent of)
`gift_reaction_context`:

```rust
fn tip_tier_adjective(amount_usd: f64) -> &'static str {
    match amount_usd {
        a if a < 10.0    => "一般",
        a if a < 100.0   => "有点多",
        a if a < 1000.0  => "超级多",
        a if a < 10000.0 => "非常夸张",
        _                => "近乎不可思议",
    }
}

pub fn tips_reaction_context(amount_usd: f64) -> String {
    format!(
        "\n\n【刚收到的打赏】\n用户刚刚给你发了一个 ${} 美元的红包，对你来说算「{}」的一笔。\n请在回复中自然地回应这份心意，不要照搬本指令原文。",
        fmt_amount(amount_usd),
        tip_tier_adjective(amount_usd),
    )
}
```

`fmt_amount`: whole numbers render without decimals (`$20`, not `$20.0`);
fractional amounts render with two decimals (`$5.50`). Same helper used for the
§2.3 marker content.

Bucketing (log10 order-of-magnitude, covers any positive amount; each FE preset
lands squarely in its bucket):

| Tier | Range (USD) | Preset | Adjective |
|---|---|---|---|
| 1 | `< 10` | $2 | 一般 |
| 2 | `10 – 99` | $20 | 有点多 |
| 3 | `100 – 999` | $200 | 超级多 |
| 4 | `1000 – 9999` | $2000 | 非常夸张 |
| 5 | `≥ 10000` | $20000 | 近乎不可思议 |

Wiring: in `build_reply_request` (`handlers.rs:415`), read `tips_amount_usd`
from `input.event`; after `build_prompt(...)` returns `system_prompt`
(`handlers.rs:496`), if `Some(amount)`, append `tips_reaction_context(amount)`.
`build_prompt`'s signature is **not** changed — the tip logic stays isolated to
the reply builder.

### 2.7 Data flow

```
FE taps tip ($20)
  → POST /comp/chat/{sid}/message/stream { tips_amount_usd: 20, client_msg_id }
  → validate_payload (content may be empty; 0 < amount ≤ 1_000_000)
  → upsert_user_message_idempotent(role='user', content="(打赏 $20)")
  → run_stream → Event::UserMessage { tips_amount_usd: Some(20.0), .. }
  → pde::decide → rule 0 → Reply (normal deltas, ghost bypassed)
  → build_reply_request → system_prompt += tips_reaction_context(20)
  → SSE stream (same channel as a normal reply) → Done
```

---

## 3. Testing

- **pde**: `UserMessage` with `tips_amount_usd: Some(_)` → `Reply`; forced even
  when ghost signals would otherwise fire; deltas equal `predict_reply_deltas`.
- **prompt**: bucket boundaries (`$9.99`→一般, `$10`→有点多, `$99`→有点多,
  `$100`→超级多, `$999`→超级多, `$1000`→非常夸张, `$10000`→近乎不可思议);
  `fmt_amount` integer vs fractional (`$20`, `$5.50`); fragment contains both the
  amount and the adjective.
- **validate_payload**: tip present allows empty content; `amount ≤ 0`,
  non-finite, `> 1_000_000` each rejected; content + tip both absent → 422.
- **stream integration** (mock OpenRouter): a tip request streams to `Done`, and
  the assembled system prompt contains the `【刚收到的打赏】` block.

---

## 4. Open items / deferred

- Distinct `gift_user` role for tip turns (needs idempotent-upsert + replay
  changes) — deferred to the gift-route keep/remove decision.
- Affinity movement / `companion_affinity_events` recording for tips — out of
  scope; if wanted later, the existing `event/gift` route or a new rule can own
  it.
- `Event::Gift` / `GiftReaction` / `tip_personality` reaction line — untouched.
