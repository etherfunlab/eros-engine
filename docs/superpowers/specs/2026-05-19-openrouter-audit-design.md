# OpenRouter Audit / Usage Passthrough — Design

**Status:** Draft for review
**Date:** 2026-05-19
**Owner:** @enriquephl

## Problem

`eros-engine-llm::openrouter` today is a minimal wrapper: it takes
`model + messages + temperature + max_tokens`, sends them to
`POST /api/v1/chat/completions`, and returns a single `reply: String`.
Three things callers downstream (eros-chat, future analytics jobs) cannot do:

1. **Per-user attribution on OpenRouter's dashboards.** OpenRouter's `user`,
   `session_id`, and `metadata` fields are silently dropped because the wire
   request type doesn't expose them.
2. **Local usage accounting.** The wire response's `usage` block, `id`
   (generation id), and `model` (the model actually served, important when
   fallback kicks in) are silently discarded; callers have no way to record
   tokens / cost per turn.
3. **App-level identification on OpenRouter.** The HTTP client sends no
   `HTTP-Referer` / `X-Title` headers, so OpenRouter's app analytics groups
   our traffic as anonymous.

We want a generic OpenRouter passthrough layer in `eros-engine`. The engine
remains a pure transport: it never interprets these fields, never persists
them, and never decides what hash / metadata shape to use. The caller
(eros-chat or any other deployer) decides.

This mirrors the [prompt-traits injection design](./2026-05-18-prompt-traits-injection-design.md)
precedent: add interfaces, do not add business logic.

## Non-Goals

- **No persistence.** No new column on `chat_messages`, no new `ai_usage`
  table. Each request returns usage in-band; callers decide if/where to
  write it.
- **No content semantics in the engine.** Engine never hashes `user`,
  never inspects metadata keys, never PII-scrubs anything.
- **No usage on the async path's polling response.** `send_message_async`
  accepts inbound `audit` and emits usage via tracing, but
  `/comp/chat/{session_id}/pending/{message_id}` does **not** carry usage.
  Callers needing per-turn audit data must use the sync route.
- **No usage on background paths.** `pipeline::dreaming` and
  `pipeline::post_process` continue to call OpenRouter; their usage surfaces
  only as tracing fields, never as a response.
- **No changes to the gift route.** `event_gift` does not call
  `pipeline::run` / OpenRouter (see route doc-comment), so it has nothing to
  passthrough.
- **No streaming.** Current client is non-streaming; usage capture for SSE
  is out of scope.
- **No per-request override of App-Attribution headers.** App-Attribution is
  deployer-level (one set per deployment) — per-request override would defeat
  OpenRouter's app-level aggregation. Per-user attribution belongs in
  `audit.user`.

## Compatibility

Clients that send neither `audit` nor read `usage` produce wire requests
**byte-for-byte identical** to today's output, and ignore three new optional
fields in the response. Verified by:

- A unit test that round-trips `WireRequest` without audit fields and asserts
  the JSON has no `user` / `session_id` / `metadata` keys.
- The new response fields are `Option<...>` and serde-skipped when `None`.

## High-Level Design

```
HTTP body
   │  audit: { user?, session_id?, metadata? }
   ▼
SendMessageRequest ──► validate (caps below) ──► Event::UserMessage{…, audit}
                                                            │
                                                            ▼
                                          pipeline::run copies → DecisionInput.audit
                                                            │
                                                            ▼
                                  ReplyHandler::make_chat_request consumes
                                                            │
                                                            ▼
                          ChatRequest{…, user, session_id, metadata}
                                                            │
                                                            ▼
                                  OpenRouterClient::execute
                                                            │
                                                            ▼ (default headers always)
                          POST openrouter.ai/api/v1/chat/completions
                                  HTTP-Referer / X-Title (if env set)
                                                            │
                                                            ▼
                          ChatResponse{ reply, usage?, generation_id?, model? }
                                                            │
                                                            ▼
                          CompanionReplyResponse (sync) — usage in body
                          AsyncSendResponse (async) — usage only in tracing
```

## API Surface

### Request body (additive — sync + async share the same body type)

`POST /comp/chat/{session_id}/message` and `POST /comp/chat/{session_id}/message_async`:

```jsonc
{
  "message": "...",
  "prompt_traits": [...],                       // unchanged
  "audit": {                                    // optional, default null
    "user": "u_abc123",                         // optional
    "session_id": "conv_xyz",                   // optional, ≠ URL session UUID
    "metadata": {                               // optional
      "feature": "chat",
      "plan": "pro"
    }
  }
}
```

Why nest the three fields under one object: they share a single semantic
purpose ("audit context for this OpenRouter call") and putting them at the
top level would collide with the URL path's `session_id`.

### Response body (additive — sync only)

`CompanionReplyResponse` grows three optional fields:

```jsonc
{
  "reply": "...",
  "session_id": "...",
  "lead_score": 0.0,
  "should_show_cta": false,
  "typing_delay_ms": 800,
  "agent_training_level": 0.4,
  "usage": { ... },                             // optional, OpenRouter wire block verbatim
  "generation_id": "gen-...",                   // optional, OpenRouter response.id
  "model": "openai/gpt-5.2"                     // optional, model actually served
}
```

All three are `Option`:

- Upstream omits → `null`
- Engine never built a `ChatRequest` for this turn (e.g. pipeline produced
  no reply) → `null`
- `fallback_model` succeeds after primary failed → `model` reflects the
  fallback identifier, `usage` reflects that call

`AsyncSendResponse` and the `/pending` polling response are **unchanged**.

### Validation (engine boundary, mirrors prompt_traits style)

| Field | Limit | On violation |
|---|---|---|
| `audit.user` | `chars().count() ≤ 256` | `400 BadRequest` |
| `audit.session_id` | `chars().count() ≤ 256` | `400 BadRequest` |
| `audit.metadata` top-level key count | `≤ 16` | `400 BadRequest` |
| `audit.metadata` key | regex `^[A-Za-z0-9_.-]{1,64}$` | `400 BadRequest` |
| `audit.metadata` value | JSON `string`, `chars().count() ≤ 512` | `400 BadRequest` |

`user` / `session_id` caps are conservative (OpenRouter docs don't specify;
256 chars holds any reasonable hash without inviting PII). `metadata` caps
match OpenRouter's documented limits (16 keys / 64-char key / 512-char value).
Metadata values are restricted to JSON strings — callers wanting numeric
values stringify (`"123"`). All limits live in one `const` block in
`routes/companion.rs` for future env-driven override.

## Type Plumbing

### `eros-engine-core/src/types.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LlmAudit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}
```

Extend `Event::UserMessage` and `DecisionInput`:

```rust
pub enum Event {
    UserMessage {
        content: String,
        message_id: Uuid,
        #[serde(default)]
        prompt_traits: Vec<PromptTrait>,
        #[serde(default)]
        audit: Option<LlmAudit>,           // NEW
    },
    // other variants unchanged
}

pub struct DecisionInput {
    pub event: Event,
    pub affinity: Affinity,
    pub persona: CompanionPersona,
    pub signals: ConversationSignals,
    pub prompt_traits: Vec<PromptTrait>,
    pub audit: Option<LlmAudit>,           // NEW
}
```

Other `Event` variants get `audit: None` by default; the field is only
populated for `UserMessage`.

### `eros-engine-llm/src/openrouter.rs`

```rust
#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub fallback_model: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub temperature: f32,
    pub max_tokens: u32,
    // NEW: opaque OpenRouter wire passthrough
    pub user: Option<String>,
    pub session_id: Option<String>,
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub reply: String,
    // NEW: opaque OpenRouter wire echo
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct AppAttribution {
    pub referer: Option<String>,   // → HTTP-Referer
    pub title: Option<String>,     // → X-Title
}

impl OpenRouterClient {
    pub fn new(api_key: String, attribution: AppAttribution) -> Self { ... }
}
```

`WireRequest` mirrors `ChatRequest` with `#[serde(skip_serializing_if = "Option::is_none")]`
on the three new fields, so legacy callers (dreaming / post_process) emit
byte-identical bodies. `WireResponse` grows `id`, `model`, `usage`:

```rust
#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
    choices: Vec<WireChoice>,
}
```

`call_once` returns `(reply, id, model, usage)`; `execute` packs them into
`ChatResponse`. On fallback, the fallback call's values are the ones
returned — callers see the model that actually served.

### App-Attribution headers

`OpenRouterClient::new` builds `reqwest::Client` with `default_headers`:

```rust
let mut headers = HeaderMap::new();
if let Some(ref r) = attribution.referer {
    if let Ok(v) = HeaderValue::from_str(r) {
        headers.insert("HTTP-Referer", v);
    }
}
if let Some(ref t) = attribution.title {
    if let Ok(v) = HeaderValue::from_str(t) {
        headers.insert("X-Title", v);
    }
}
let http = reqwest::Client::builder().default_headers(headers).build()?;
```

Invalid header values (non-ASCII, control chars) are silently dropped at
construction with a `tracing::warn!` — the client still works without the
header. App-Attribution is best-effort; we don't want a malformed env var to
crash boot.

**Header names ship as the current OpenRouter canonical spec:**
`HTTP-Referer` and `X-Title`. They live in named `const` at the top of
`openrouter.rs` so a future rename is a one-line change.

**Forward-compat policy:** When OpenRouter ships new names, the migration
is: rename `HEADER_REFERER` / `HEADER_TITLE` to the new canonical values,
and (if OpenRouter's transition window requires it) add the previous
names as additional legacy aliases sent alongside. Until that day, we
ship one name per role to avoid silent double-counting.

### `eros-engine-server/src/state.rs`

`AppState` construction reads two new env vars:

```rust
let attribution = AppAttribution {
    referer: std::env::var("OPENROUTER_APP_REFERER").ok().filter(|s| !s.is_empty()),
    title: std::env::var("OPENROUTER_APP_TITLE").ok().filter(|s| !s.is_empty()),
};
let openrouter = Arc::new(OpenRouterClient::new(api_key, attribution));
```

Both unset → today's behaviour (no headers sent).

### `eros-engine-server/src/pipeline/mod.rs`

After step 5 (where `prompt_traits` is copied), copy `audit` the same way:

```rust
let (prompt_traits, audit) = match &event {
    Event::UserMessage { prompt_traits, audit, .. } => {
        (prompt_traits.clone(), audit.clone())
    }
    _ => (Vec::new(), None),
};

let input = DecisionInput {
    event: event.clone(),
    affinity,
    persona,
    signals,
    prompt_traits,
    audit,
};
```

### `eros-engine-server/src/pipeline/handlers.rs`

`ReplyHandler::make_chat_request` (and any other handler that produces a
`ChatRequest` from `DecisionInput`) reads `input.audit` and copies the three
fields onto the `ChatRequest`:

```rust
let audit = input.audit.as_ref();
ChatRequest {
    model,
    fallback_model,
    messages,
    temperature,
    max_tokens,
    user: audit.and_then(|a| a.user.clone()),
    session_id: audit.and_then(|a| a.session_id.clone()),
    metadata: audit.and_then(|a| a.metadata.clone()),
}
```

The handlers must also return `ChatResponse` (or at least its three new
fields) up to the route so the sync path can surface `usage`. The current
return shape from `pipeline::run` propagates `reply`; that signature grows
to carry `usage` / `generation_id` / `model` alongside.

### `eros-engine-server/src/pipeline/dreaming.rs` and `pipeline/post_process.rs`

No code change beyond using `ChatRequest::default()` to spread defaults for
the three new optional fields when constructing `ChatRequest` literally:

```rust
let req = ChatRequest {
    model: ...,
    fallback_model: ...,
    messages: ...,
    temperature: ...,
    max_tokens: ...,
    ..Default::default()                // NEW — leaves user/session_id/metadata = None
};
```

Their `ChatResponse.usage` / `generation_id` / `model` are read for tracing
fields only (see Observability) — they are not returned to any caller.

### `eros-engine-server/src/routes/companion.rs`

1. Add `LlmAuditDto` with `Deserialize + ToSchema`:

   ```rust
   #[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
   pub struct LlmAuditDto {
       #[serde(default)]
       pub user: Option<String>,
       #[serde(default)]
       pub session_id: Option<String>,
       #[serde(default)]
       pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
   }
   ```

2. Extend `SendMessageRequest`:

   ```rust
   pub struct SendMessageRequest {
       pub message: String,
       #[serde(default)]
       pub prompt_traits: Option<Vec<PromptTraitDto>>,
       #[serde(default)]
       pub audit: Option<LlmAuditDto>,           // NEW
   }
   ```

3. Add helper:

   ```rust
   fn validate_llm_audit(dto: Option<LlmAuditDto>) -> Result<Option<LlmAudit>, AppError> { ... }
   ```

   Enforces the caps in the Validation table.

4. Both `send_message` and `send_message_async` call the helper before
   `pipeline::run`, pass the validated `Option<LlmAudit>` into
   `Event::UserMessage`.

5. Extend `CompanionReplyResponse` with three `Option` fields:

   ```rust
   pub struct CompanionReplyResponse {
       pub reply: String,
       pub session_id: Uuid,
       pub lead_score: f64,
       pub should_show_cta: bool,
       pub typing_delay_ms: u64,
       pub agent_training_level: f64,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub usage: Option<serde_json::Value>,             // NEW
       #[serde(skip_serializing_if = "Option::is_none")]
       pub generation_id: Option<String>,                // NEW
       #[serde(skip_serializing_if = "Option::is_none")]
       pub model: Option<String>,                        // NEW
   }
   ```

   `send_message` populates them from `pipeline::run`'s extended return.
   `send_message_async` returns `AsyncSendResponse` as before — unchanged.

## Observability

`pipeline::run`'s existing `tracing::info!` line gains audit + usage fields.
Tags only, never raw values:

```
audit_user_present       = bool   // true iff audit.user is Some
audit_session_present    = bool
audit_metadata_keys      = ?      // ["org_id", "feature", ...] — keys only, no values
generation_id            = ?      // upstream's response.id
model                    = ?      // upstream's response.model
prompt_tokens            = u64?   // parsed from usage.prompt_tokens if present
completion_tokens        = u64?   // parsed from usage.completion_tokens if present
total_tokens             = u64?
cost                     = f64?   // parsed from usage.cost if present
```

Token / cost fields are parsed best-effort from the opaque `usage` JSON
(`u64::try_from(json.get("prompt_tokens"))`). If parsing fails the field is
omitted from the log line; no error surfaces. This keeps the engine's
"engine does not interpret" stance — the parse is purely for logs, the
opaque JSON still rides through to the response.

## OpenAPI

`SendMessageRequest`, `CompanionReplyResponse`, and the new `LlmAuditDto`
derive `ToSchema`. The repo has a CI snapshot drift check — implementation
must regenerate the snapshot or CI fails.

## Tests

### `eros-engine-llm/src/openrouter.rs` (unit + wiremock)

- `wire_request_omits_audit_fields_when_none` — serialize `ChatRequest`
  with `user/session_id/metadata = None`; assert JSON has no such keys.
  Guards byte-identical legacy behaviour.
- `wire_request_includes_audit_fields_when_set` — non-`None` cases pass
  through verbatim.
- `wire_response_parses_id_model_usage` — fixture body with
  `{ "id": "gen-...", "model": "...", "usage": {...} }` → `ChatResponse`
  carries them.
- `wire_response_handles_missing_id_model_usage` — fixture omitting the
  fields → all three `None`, no error.
- `client_sends_app_attribution_headers_when_set` — wiremock; construct
  client with `AppAttribution { referer: Some, title: Some }`, fire one
  call, assert headers present in captured request.
- `client_omits_app_attribution_headers_when_default` — opposite case.
- `client_drops_invalid_attribution_value` — `referer: Some("bad\nvalue")`
  → header skipped at construction, client still functional.

### `eros-engine-core/src/types.rs` (unit)

- `llm_audit_serde_roundtrip_full` — all three sub-fields populated.
- `llm_audit_serde_roundtrip_empty` — `{}` → all `None`.
- `event_user_message_defaults_audit_to_none` — body missing `audit`
  deserialises to `audit = None`.

### `eros-engine-server/src/routes/companion.rs` (route, sqlx::test)

- `send_message_accepts_missing_audit_field` — extends an existing positive
  test with assertion that the resulting wire request would have no audit
  fields (mock OpenRouter).
- `send_message_rejects_oversized_audit_user` — 257 chars → 400.
- `send_message_rejects_too_many_metadata_keys` — 17 keys → 400.
- `send_message_rejects_invalid_metadata_key_regex` — `"Bad Key!"` → 400.
- `send_message_rejects_non_string_metadata_value` — `{"k": 123}` → 400.
- `send_message_returns_usage_when_upstream_provides_it` — mock OpenRouter
  returns `usage` + `id` + `model`; assert `CompanionReplyResponse` carries
  them.
- `send_message_returns_null_usage_when_upstream_omits` — mock returns no
  `usage`; assert response has `usage: null` (i.e. field omitted in JSON).
- `send_message_async_accepts_audit_without_returning_usage` — async route
  with audit succeeds, response is unchanged `AsyncSendResponse`, no usage.
- `send_message_does_not_persist_audit` — submits an audit object, asserts
  no row in any table contains the user / session_id / metadata strings.

## Documentation

- `docs/api-reference.md` and `docs/api-reference.zh.md`: extend
  `SendMessageRequest` table with `audit`; extend `CompanionReplyResponse`
  with three optional fields; add `LlmAuditDto` table with the validation
  caps.
- New `docs/llm-audit.md` (+ `.zh.md`): one-page reference. Frames the
  feature as **"a generic OpenRouter passthrough — caller decides what to
  send, engine echoes what comes back"**. Sections:
  - Inbound passthrough (`audit` object, caps)
  - Outbound usage (sync only, opaque JSON)
  - App-Attribution headers (env vars, deployer-level)
  - Privacy note (engine never hashes, never persists, caller's
    responsibility to scrub PII from `user` and metadata values)
- `.env.example`: add commented lines for `OPENROUTER_APP_REFERER` and
  `OPENROUTER_APP_TITLE`.
- `README.md`: no change (internal API affordance, not a headline feature).

## Risks / Open Questions

1. **`pipeline::run` return shape grows.** Today it returns a `reply` string
   (wrapped in some `Option<PipelineResponse>`). Carrying `usage` /
   `generation_id` / `model` up to the route means extending this return
   type. Risk: all internal callers of `pipeline::run` need updating.
   Mitigation: search-and-replace is mechanical; the type extension is
   additive.
2. **`metadata` value type restriction.** OpenRouter's docs don't strictly
   require string values, but allowing arbitrary JSON would force the
   validator to enumerate types. Choosing "strings only" simplifies the
   contract; callers stringify numbers. Open question for review: do we
   need numeric values?
3. **Token / cost parsing in tracing.** Parsing the opaque `usage` JSON for
   tracing fields means the engine has *some* knowledge of OpenRouter's
   usage shape. This is acceptable because the parse is best-effort and
   the field is logged-only — it does not alter the response body. If
   OpenRouter renames `prompt_tokens` to something else, our log fields
   would silently disappear; we don't break.
4. **Header name evolution.** Engine pins to OpenRouter's current canonical
   names (`HTTP-Referer`, `X-Title`) via `const` at the top of
   `openrouter.rs`. If OpenRouter renames either header, today's names
   become legacy: rename the const to the new canonical value, and add
   a parallel legacy alias only if OpenRouter's transition window
   requires it. Until then, single-name is intentional to prevent
   double-counting.
5. **Per-request override of App-Attribution headers.** Not in scope.
   Justification: App-Attribution exists for app-level aggregation;
   per-request override defeats it. Per-user attribution belongs in
   `audit.user`.
6. **Fallback model & generation id correspondence.** If primary fails and
   fallback succeeds, the returned `generation_id` / `model` / `usage` all
   come from the fallback call. The caller can detect a fallback by
   comparing returned `model` against the model they requested.

## Acceptance Criteria

- [ ] `cargo test -p eros-engine-llm -p eros-engine-server -p eros-engine-core` green
- [ ] OpenAPI snapshot regenerated; CI drift check green
- [ ] Request with no `audit` field produces wire body byte-identical to main
- [ ] Sync `send_message` with mocked OpenRouter returns `usage`,
      `generation_id`, `model` non-null
- [ ] Async `send_message_async` ignores usage (response unchanged) but logs
      `total_tokens` via tracing
- [ ] `OPENROUTER_APP_REFERER` + `OPENROUTER_APP_TITLE` unset → no
      attribution headers on outbound request; both set → headers present
- [ ] No new rows in any DB table when `audit` is sent — verified by a
      route-level test asserting raw audit strings are absent from
      `chat_messages.content` and all JSONB columns
