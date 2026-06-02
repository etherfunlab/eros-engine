# eros-engine — Tip-turn parrot fix + `run_stream` e2e tests (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track. **No migration.**
**Scope**: a localized bug fix (`gift_user` rows dropped from the model prompt) plus
two end-to-end `#[sqlx::test]`s driving `run_stream` with a mocked OpenRouter.
This is **Spec A** of a two-spec split; the larger extraction-pipeline overhaul
(insight events, audit columns, schema fields, prompt precision, config-driven
extraction prompts) is **Spec B**, designed separately.

---

## 0. Background

### The bug (item 6)

When a user sends a **tip** (打赏), the companion's reply **parrots the recent
conversation** instead of responding to the current turn. Reported example
(AI = Yumi): `（轻笑着摇头）我这里是宁波啊。你没看见我刚才在提宁波的图书馆吗？` —
the model re-asserts facts from earlier turns rather than reacting.

It was observed for tips **< $1**, but the root cause is **amount-independent**.

### Root cause (confirmed in code)

1. A tip attached to a user message is routed `ActionType::Reply` (never ghost),
   unconditionally — `crates/eros-engine-core/src/pde.rs:38-54`. No amount branch.
2. The route persists the turn as **`role = 'gift_user'`** with content equal to
   the typed message, or `"(打赏 $X)"` when no text was sent —
   `crates/eros-engine-server/src/routes/companion_stream.rs:364-371`.
3. `assemble_chat_request` builds the model's message list from history but only
   maps `"user"` and `"assistant"` rows; **`gift_user` hits the `_ => continue`
   arm and is dropped** — `crates/eros-engine-server/src/pipeline/handlers.rs:185-189`:
   ```rust
   let content = match msg.role.as_str() {
       "user" => model_facing_user_text(&msg),
       "assistant" => msg.content,
       _ => continue,            // ← gift_user dropped
   };
   ```
4. Because the just-inserted tip turn is a `gift_user` row, the model receives the
   system prompt (with the `[tip_received]` block appended at `handlers.rs:625`) and
   history that **ends at the previous turn** — there is no fresh user turn to answer,
   so the model continues / parrots the thread. Same failure class as the
   `chat_input_filter` motivation (a turn with no model-visible content makes the
   model echo history).

`fmt_amount` (`prompt.rs:273-279`) formats sub-$1 amounts correctly
(`0.50` → `"0.50"`); there is no degenerate-formatting contribution. Small tips
simply surface the bug most often because they usually carry no text.

### Why this is Spec A (with the e2e tests)

The fix is a few lines but touches the core prompt-assembly path, and the repo had
**no end-to-end `run_stream` test** asserting that a turn's content actually
reaches the model (the prior chat-vision work flagged this gap). Bundling the fix
with two e2e tests gives the tip fix a regression guard and closes the flagged
gap in one focused, fast-to-ship spec.

---

## 1. Goals / non-goals

**Goals**
- A tip turn's content (typed message, or the `"(打赏 $X)"` marker for a
  tip-only turn) reaches the chat model, so the companion reacts to the tip
  instead of parroting history.
- Two `#[sqlx::test]`s driving `run_stream` end-to-end against a mocked
  OpenRouter: (a) the tip-fix regression, (b) the chat-vision image path.

**Non-goals (out of scope — deferred to Spec B or later)**
- Any change to the recall `query_text` path (`handlers.rs:505`). For a tip turn
  it falls back to the driving event's content; this is a minor recall nuance, not
  the parrot bug. Spec B owns recall-query work.
- Changing `[tip_received]` wording, tip tiers, or `fmt_amount`.
- The extraction-pipeline overhaul (Spec B).

---

## 2. The fix (item 6)

**File**: `crates/eros-engine-server/src/pipeline/handlers.rs`, `assemble_chat_request`
(@ ~L180-194).

Treat `gift_user` the same as `user`, and emit it to the model under the `"user"`
role (OpenRouter only understands `system`/`user`/`assistant`). Replace the loop
body:

```rust
for msg in history {
    // User + gift_user (tip) rows feed the MODEL-FACING text; tip turns are user
    // turns to the model. Assistant rows always feed `content`.
    let (role, content) = match msg.role.as_str() {
        "user" | "gift_user" => ("user", model_facing_user_text(&msg)),
        "assistant" => ("assistant", msg.content),
        _ => continue,
    };
    messages.push(ChatMessage {
        role: role.to_string(),
        content,
    });
}
```

Notes:
- `model_facing_user_text` on a `gift_user` row returns `effective_user_text`
  (just `content`, since the input filter skips tipped turns and `image_url`+tip
  is rejected, so there is never a vision preamble). So a tip-with-message shows
  the message; a tip-only turn shows `"(打赏 $X)"`.
- Past `gift_user` rows in the history window also become visible now — correct;
  they were silently vanishing before.
- The `[tip_received]` system context (`handlers.rs:617-626`) is unchanged.

**Regression check on existing tests**: confirm no existing test asserts that
`gift_user` rows are *absent* from the assembled prompt; update any that do to the
new (correct) behavior.

---

## 3. e2e `#[sqlx::test]`s (item 0)

Both live in the `crates/eros-engine-server/src/pipeline/stream.rs` test module and
mirror `input_filter_rewrites_meaningless_turn` (@ ~L3588): `wiremock::MockServer`
for OpenRouter, `crate::routes::companion::test_state(pool)` with `openrouter` +
`model_config` overridden, `seed_persona_and_session`, a `ChatRepo` upsert, then
`run_stream(Arc::new(state), user_msg).collect().await` and assertions on frames +
persisted rows. Mocks are routed by `body_string_contains(...)`.

### 3a. Tip-fix regression — `tip_turn_reaches_model_not_parrot`

- Config: `[tasks.chat_companion] model="deepseek/x"` (no filters needed).
- Seed persona/session. Upsert a `gift_user` row:
  ```rust
  chat_repo.upsert_user_message_idempotent(
      session_id, "(打赏 $0.5)", "<26+ char client id>", "gift_user", None).await
  ```
- `PersistedUserMessage { content: "(打赏 $0.5)".into(), tips_amount_usd: Some(0.5),
  image_url: None, .. }`.
- Chat mock matches **only** when the request body contains `"(打赏"` and replies an
  SSE delta `"REPLY"`. Because the mock requires the tip turn's text in the body, a
  `REPLY` delta proves the tip turn reached the model:
  ```rust
  Mock::given(wm_path("/api/v1/chat/completions"))
      .and(body_string_contains("deepseek/x"))
      .and(body_string_contains("(打赏"))   // ← proves the gift_user turn is in the prompt
      .respond_with(/* SSE delta "REPLY" + [DONE] */)
  ```
- Assert: collected `Delta` frames contain `"REPLY"`; no `Error` frame.
  (Pre-fix, the body would not contain `"(打赏"` → mock would not match → the test
  fails, which is the regression guard.)

### 3b. chat-vision path — `vision_turn_folds_description_and_persists`

- Config:
  ```
  [tasks.chat_companion] model="deepseek/x"
  [tasks.chat_vision] model="vis/m" filter_prompt="DESCRIBE"
  ```
- Two mocks:
  - Vision model `"vis/m"` (non-streaming JSON) returns a describe object, e.g.
    `{"description":"一只猫在沙滩","ocr_text":"","people":"","scene":"海边"}`.
  - Chat model `"deepseek/x"` (SSE) matches **only** when the body contains
    `"一只猫在沙滩"` (the folded description) and replies a delta `"REPLY"`.
- Upsert a `user` row (empty content ok) seeded with
  `metadata = {"image_url":"https://x/y.png"}`; `PersistedUserMessage { content:
  "".into(), image_url: Some("https://x/y.png".into()), .. }`.
- Assert:
  - `Delta` frames contain `"REPLY"` (the description reached the chat model).
  - The user row's `metadata.vision.description == "一只猫在沙滩"` (persisted via
    `set_user_image_vision`):
    ```rust
    let meta: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT metadata FROM engine.chat_messages WHERE id = $1").bind(umid)...;
    assert_eq!(meta.unwrap()["vision"]["description"], "一只猫在沙滩");
    ```
  - No `Error` frame.

> Note: `run_stream` builds the `DecisionInput`/`Event` from `PersistedUserMessage`
> (incl. `tips_amount_usd` and `image_url`); these tests exercise that wiring. If a
> helper to construct the test `PersistedUserMessage` does not already exist, build
> it inline as the sibling tests do.

---

## 4. Testing / verification

- `tip_turn_reaches_model_not_parrot` — passes with the fix, would fail without it.
- `vision_turn_folds_description_and_persists` — passes with the existing
  chat-vision pipeline + the fix (unaffected); first e2e coverage of that path.
- Existing `stream.rs` tests stay green (adjust any that assumed `gift_user`
  invisibility).
- PR gate: `cargo fmt` / `clippy --workspace -D warnings` / `test --workspace`
  (DB tests via `.test-env`). No `openapi.json` change (no DTO/route change). No
  migration.

## 5. Files touched

- `crates/eros-engine-server/src/pipeline/handlers.rs` — the `gift_user` mapping in
  `assemble_chat_request`.
- `crates/eros-engine-server/src/pipeline/stream.rs` — two new `#[sqlx::test]`s
  (+ any helper).
