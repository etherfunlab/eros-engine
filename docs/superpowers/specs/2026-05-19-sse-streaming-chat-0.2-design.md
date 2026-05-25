# eros-engine 0.2 — SSE Streaming Chat (Spec)

**Status**: design, pending implementation plan
**Target release**: `eros-engine` 0.2.0 (adds SSE), 0.2.x patch window, 0.3.0 (removes sync)
**Audience**: anyone implementing the backend side of the streaming chat upgrade in `eros-engine`
**Companion spec**: `eros-engine-web/2026-05-19-sse-streaming-chat-design.md` — same §1 protocol, different §3 implementation

---

## 0. Background

Chat in `eros-engine` is fully synchronous today. `POST /comp/chat/{session_id}/message` calls `pipeline::run()` which blocks on `OpenRouterClient::execute()` (a monolithic reqwest POST without `stream: true`) and only returns `Json<CompanionReplyResponse>` after the full LLM response has arrived. Time-to-first-content is 3-8 s.

OpenRouter is OpenAI-compatible and natively supports `"stream": true` over `text/event-stream`. The upgrade target is to replace the user-visible chat latency profile: first delta in ~300-600 ms, full reply still in the same total wall-clock, plus the ability to render token-by-token in the client.

There is also a hand-rolled long-poll variant — `send_message_async` (202 + message_id) + `check_pending` GET — that exists alongside the sync handler. It solves "HTTP keepalive timeouts in front of slow LLMs," not "user wants to see tokens stream." Streaming makes it redundant; it is removed as part of 0.2.

## 1. Wire Protocol

### 1.1 Endpoint

```
POST /comp/chat/{session_id}/message/stream
```

**Request headers** (all required unless noted)

```
Authorization: Bearer <jwt>
Accept: text/event-stream
Content-Type: application/json
```

**Request body** (JSON)

```jsonc
{
  "content": "<user message, 1..4096 chars>",
  "client_msg_id": "<UUID (any version) or ULID, 26-36 chars, ASCII printable, no whitespace>"   // required in 0.2 — drives idempotency
}
```

**Response headers on success**

```
HTTP/1.1 200 OK
Content-Type: text/event-stream; charset=utf-8
Content-Encoding: identity
Cache-Control: no-cache, no-transform, private
Connection: keep-alive
X-Accel-Buffering: no
```

### 1.2 Authorization

- JWT must verify. Failure → `401 Unauthorized` (pre-stream, JSON error body).
- JWT subject must be authorized for the requested `session_id`. Failure → `403 Forbidden` (pre-stream).
- Authorization errors **never** appear as in-band SSE `error` frames. Once the first SSE byte has been written, authorization is no longer questioned.

### 1.3 Pre-stream errors (HTTP status, JSON body)

Before the first SSE byte is written, these errors return a normal HTTP error response:

| Status | `code`                  | Trigger                                                            |
|-------:|-------------------------|--------------------------------------------------------------------|
|    400 | `invalid_payload`       | JSON malformed / required field missing / value out of range       |
|    401 | `unauthorized`          | JWT invalid or expired                                             |
|    403 | `session_forbidden`     | JWT user does not own the `session_id`                             |
|    404 | `session_not_found`     | `session_id` does not exist                                        |
|    409 | `duplicate_in_progress` | Same `(session_id, client_msg_id)` is still generating. Response body carries `original_user_message_id`. |
|    422 | `unprocessable`         | content length 0 or > 4096, content-policy violation               |
|    429 | `rate_limited`          | Per-user concurrent stream cap (3) or per-minute cap hit           |
|    500 | `internal`              | Unexpected server error                                            |

After the first SSE frame is written, the same conditions are no longer possible (auth/payload already validated); other failures become in-band `error` frames (§1.6) and terminate the stream.

**Pre-stream error body schema** (all HTTP error statuses above):

```jsonc
{
  "code": "<error code from the table>",
  "message": "<internal-facing log string>",
  "user_message": "<sanitized, safe to display>",
  // 409 only:
  "original_user_message_id": "01J..."
}
```

`code`, `message`, `user_message` are required for every pre-stream error response. Extra fields per code are allowed; clients must tolerate unknown extras.

### 1.4 SSE wire format

- **Event frame**: a single line `data: <compact-JSON>\n\n`. The JSON must be serialized in compact form (no embedded newlines). One frame per `data:` line.
- **Comment line** (keepalive): `: ping\n\n`, emitted every 15 s. Comment lines are not frames and do not participate in the message state machine.
- No `event:` named fields are used. No `id:` field is emitted (0.2 does not support Last-Event-ID resumption).

### 1.5 Frame types

Every frame's JSON has a required `"type"` discriminator.

#### `meta` — opens a logical message

```jsonc
{
  "type": "meta",
  "message_id": "01J...",                       // ULID, server-assigned, equals the assistant DB row id (when one is created)
  "action_type": "reply" | "ghost" | "gift_reaction",
  "model": "x-ai/grok-4-fast",                  // model id actually selected
  "continues_from": "01J..." | null             // if this message is a fallback continuation, the prior truncated message_id
}
```

> **Update (2026-05-25):** `meta.model` is now optional — omitted when
> `tasks.chat_companion.model_name_display_override` resolves to "hide"
> (`false`/absent, or a map miss with no `default`). See
> `2026-05-25-model-name-display-override-design.md`.

#### `delta` — token chunk

```jsonc
{
  "type": "delta",
  "message_id": "01J...",
  "content": "你好"
}
```

- `reply` and `gift_reaction` produce 1..N `delta` frames between their `meta` and `done`.
- `ghost` produces **zero** `delta` frames. The absence of any `delta` between `meta` and `done` is the ghost signal.

#### `done` — closes a logical message

```jsonc
{
  "type": "done",
  "message_id": "01J...",
  "truncated": false,                           // see §1.6
  "usage": {                                    // OpenRouter-reported; null for ghost
    "prompt_tokens": 312,
    "completion_tokens": 64,
    "total_tokens": 376                         // upstream-reported, do not infer
  } | null,
  "generation_id": "gen-xxx" | null
}
```

#### `final` — closes the whole HTTP stream (session-level signals)

```jsonc
{
  "type": "final",
  "lead_score": 0.71,                           // f64, [0.0, 1.0]
  "should_show_cta": false,                     // bool
  "agent_training_level": 0.42                  // f64, [0.0, 1.0]; affinity-derived
}
```

A normal stream terminates with exactly one `final` (or one terminal `error`, not both).

#### `error` — terminates the stream

```jsonc
{
  "type": "error",
  "code": "upstream_unavailable" | "rate_limited" | "internal" | "timeout",
  "retryable": true | false,                    // advisory
  "message": "internal log message",            // not safe to display
  "user_message": "AI 服务暂时不可用，稍后再试"   // sanitized, safe to display
}
```

- `error` is always the last frame of the stream.
- `error` has no `message_id` field — it is a stream-level signal. Per-message truncation uses `done.truncated:true` instead.
- There is no `error.code = "auth"` (authorization errors are pre-stream HTTP responses).

#### Forward compatibility

Clients must silently ignore frames with unknown `type`. Planned future types include `user_reaction`, `tool_call`, `memory_update_hint`. New types must be designed so that older clients can drop them without disrupting the `meta → delta* → done` state machine for existing types.

### 1.6 Error model: per-message vs per-stream

| Scenario                                                       | Wire signal                                                                                                   |
|----------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------|
| Upstream call fails before any output (4xx, connect error)     | `error` frame, no `meta` ever sent for this attempt                                                            |
| Upstream stream cuts off after partial output                  | Current logical message ends with `done{truncated: true}`. **Then**: if fallback is available, a new logical message with `meta{continues_from: <prev>}` + deltas + done; else, an `error` frame |
| Per-stream hard timeout (120 s)                                | Current message's `done{truncated: true}` + `error{code: "timeout"}`                                          |
| Server panic / middleware error                                | `error{code: "internal"}`, connection closed                                                                  |

### 1.7 Client mid-stream disconnect

When the client TCP/HTTP connection closes mid-stream:

- The server **does not** abort in-flight OpenRouter calls. They run to completion.
- All logical messages that reach `done` are persisted and run through post-process using the accumulated full text.
- `final` values that would have been emitted are still computed and persisted; the next history fetch sees them.

**Protocol requirement (not implementation detail)**: the server implementation MUST ensure persistence, post-process, and `final`-derived state updates outlive the HTTP response stream. Typically this means each persistence step is dispatched to a task that does not share lifetime with the SSE generator (e.g. `tokio::spawn`). Without this guarantee, §1.8 (reconcile-via-history) and §1.10 (replay) silently degrade.

### 1.8 Connection dropped before `final`

If the stream closes (TCP error, abort) without a terminal `final` or `error`:

- Logical messages that received their `done` are valid.
- `final`-only fields (`lead_score`, `should_show_cta`, `agent_training_level`) are unchanged on the client; reconcile via the existing `GET /comp/chat/{session_id}/history` endpoint on next session enter.

### 1.9 Resource limits (hard, enforced from 0.2)

| Dimension                                  | Limit            |
|--------------------------------------------|------------------|
| Concurrent active streams per user         | 3                |
| Per-stream hard timeout                    | 120 s            |
| Per-logical-message `completion_tokens`    | 4096             |
| Per-frame size                             | 16 KiB           |
| Per-stream total frames                    | 4096             |
| `client_msg_id` idempotency window         | 24 h             |
| User `content` length                      | 1..4096 chars    |

Exceeding pre-stream → HTTP 429. Exceeding mid-stream → `error{code: "timeout"}` or `error{code: "rate_limited"}`.

### 1.10 Idempotency semantics

The dedup key is `(session_id, client_msg_id)` within a 24 h window.

| Scenario                                                                                  | Server behavior                                                                                                                                                                                                                                                              |
|-------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| First time seeing this key                                                                | Normal processing.                                                                                                                                                                                                                                                           |
| Replay: original request finished (all assistant messages persisted)                      | `200 text/event-stream`, **replay** from DB. For each persisted assistant message (in original order), synthesize: `meta` (with original `action_type`, `model`, and `continues_from` chain) + a single `delta` carrying the full text + `done` (with original `truncated` flag, persisted `usage`, persisted `generation_id`). Conclude with one `final` computed from **current** session state (not snapshotted). No OpenRouter call is made. |
| Replay: original was a **ghost** (no assistant rows persisted)                            | `200 text/event-stream`. Synthesize `meta(action_type="ghost", message_id=<new ULID, not persisted>)` + `done(truncated:false, usage:null, generation_id:null)` + one `final`. To distinguish ghost-replay from "duplicate user_msg row exists but has no linked assistant yet" (= 409 race), the persistence layer MUST mark the user message row with a `ghost_decision` flag when ghost was chosen — see §2.5. |
| Race: original request still generating                                                   | `409 duplicate_in_progress` pre-stream JSON with `original_user_message_id`. Client should poll history.                                                                                                                                                                     |

The persistence layer must support this — see §2.5.

### 1.11 Browser implementation constraints (clarification for `eros-engine-web`)

- Native `EventSource` does not support POST or custom Authorization headers.
- Clients should use `@microsoft/fetch-event-source` or an equivalent fetch + ReadableStream parser.
- The protocol does not rely on automatic reconnect. Retries are handled via idempotency (§1.10), not via SSE reconnect semantics.
- **Protocol requirement**: clients MUST disable any library-level auto-reconnect. With `@microsoft/fetch-event-source` this means the `onerror` handler MUST throw (which terminates the connection); returning a number would re-establish the connection and violate this protocol.

### 1.12 Terminology

- **Event frame**: a JSON object on a `data:` line, with one of the five `type` values defined above.
- **Comment line**: an SSE `:` line, only used for keepalive (`: ping`).
- **Logical message**: a `meta` + zero or more `delta` + one `done`.
- **Stream / burst**: one HTTP SSE response = 1..N logical messages + one terminal frame (`final` or `error`).

## 2. Backend architecture

### 2.1 Scope of changes

```
HTTP layer            [new]  routes/companion::stream::send_message_stream
                      [del]  routes/companion::send_message_async
                      [del]  routes/companion::check_pending
                      [keep] routes/companion::send_message            (until 0.3)
Pipeline layer        [new]  pipeline::run_stream
                      [keep] pipeline::run                              (until 0.3)
LLM client layer      [new]  OpenRouterClient::execute_stream
                      [keep] OpenRouterClient::execute                  (until 0.3)
Persistence layer     [chg]  user message upsert supports idempotency
                      [chg]  batch insert of N assistant messages per burst
                      [chg]  post_process accepts Vec of messages
Middleware            [new]  SseHeadersLayer (response header injector)
```

### 2.2 LLM client layer (`eros-engine-llm`)

Add a streaming method alongside the existing sync `execute`:

```rust
impl OpenRouterClient {
    pub async fn execute(&self, req: ChatRequest) -> Result<ChatResponse, LlmError>;  // existing

    pub async fn execute_stream(
        &self,
        req: ChatRequest,
    ) -> Result<BoxStream<'static, Result<DeltaChunk, LlmError>>, LlmError>;
}

pub struct DeltaChunk {
    pub content: Option<String>,        // token slice; usually None on the terminal chunk
    pub finish_reason: Option<String>,  // "stop" | "length" | None
    pub usage: Option<UsageBlock>,      // present on the terminal chunk
    pub generation_id: Option<String>,
    pub model: Option<String>,
}
```

Implementation notes:

- Send the OpenRouter request body with `"stream": true`.
- Parse the response body with `eventsource-stream` (do not call `.json().await`).
- Deserialize each `data: {...}` line into an OpenAI-compatible delta envelope; yield one `DeltaChunk` per upstream frame.
- A `data: [DONE]` line or EOF closes the stream.
- Connection / parse errors yield `Err(LlmError::Stream { .. })` and close the stream.
- **`execute_stream` is per-model.** Fallback orchestration lives in the pipeline layer (§2.3), not here.

Workspace `Cargo.toml` additions:

```toml
eventsource-stream = "0.2"
futures-util       = "0.3"
tokio-stream       = "0.1"
async-stream       = "0.3"
```

### 2.3 Pipeline layer (`pipeline::run_stream`)

```rust
pub fn run_stream(
    state: Arc<AppState>,
    session_ctx: SessionContext,
    user_msg: PersistedUserMessage,
) -> impl Stream<Item = Result<ProtocolFrame, PipelineError>>;

pub enum ProtocolFrame {
    Meta  { message_id: Ulid, action_type: ActionType, model: String,
            continues_from: Option<Ulid> },
    Delta { message_id: Ulid, content: String },
    Done  { message_id: Ulid, truncated: bool, usage: Option<UsageBlock>,
            generation_id: Option<String> },
    Final { lead_score: f64, should_show_cta: bool, agent_training_level: f64 },
    Error { code: ErrorCode, retryable: bool, message: String, user_message: String },
}
```

Pseudo-code (one accepted form; exact structure may vary):

```rust
async_stream::stream! {
    let decision = decide_action(&state, &session_ctx).await?;

    match decision.action_type {
        ActionType::Ghost => {
            let msg_id = Ulid::new();
            yield Frame::Meta  { message_id: msg_id, action_type: Ghost, .. };
            yield Frame::Done  { message_id: msg_id, truncated: false, usage: None, .. };
            // Ghost does NOT persist an assistant DB row (see §2.5).
        }
        ActionType::Reply | ActionType::GiftReaction => {
            let mut produced: Vec<(Ulid, String, ActionType)> = Vec::new();
            let mut continues_from: Option<Ulid> = None;
            let models = state.model_router.fallback_chain(decision.action_type);

            for (idx, model) in models.iter().enumerate() {
                let msg_id = Ulid::new();
                let mut acc = String::new();
                let mut last_usage = None;
                let mut last_gen_id = None;
                let mut truncated = false;

                yield Frame::Meta { message_id: msg_id, action_type, model: model.clone(), continues_from };

                let req = build_chat_request(&session_ctx, &decision, model);
                match state.openrouter.execute_stream(req).await {
                    Ok(mut s) => {
                        while let Some(chunk) = s.next().await {
                            match chunk {
                                Ok(c) => {
                                    if let Some(content) = c.content {
                                        acc.push_str(&content);
                                        yield Frame::Delta { message_id: msg_id, content };
                                    }
                                    if c.usage.is_some()         { last_usage = c.usage; }
                                    if c.generation_id.is_some() { last_gen_id = c.generation_id; }
                                }
                                Err(_) => { truncated = true; break; }
                            }
                        }
                    }
                    Err(_) => { truncated = true; }
                }

                // Persist this logical message BEFORE yielding done (see §4.3 risk register).
                persist_assistant_message(&state, &session_ctx, msg_id, &acc, action_type, truncated).await?;

                yield Frame::Done { message_id: msg_id, truncated, usage: last_usage, generation_id: last_gen_id };
                produced.push((msg_id, acc.clone(), action_type));

                if !truncated || acc.is_empty() {
                    break;  // success or zero output → don't fallback
                }
                if idx == models.len() - 1 {
                    yield Frame::Error { code: UpstreamUnavailable, .. };
                    return;
                }
                continues_from = Some(msg_id);
            }

            let signals = compute_final_signals(&state, &session_ctx, &produced).await?;
            yield Frame::Final { lead_score: signals.lead_score,
                                  should_show_cta: signals.should_show_cta,
                                  agent_training_level: signals.agent_training_level };

            tokio::spawn(post_process::run(state.clone(), session_ctx, produced));
        }
    }
}
```

Key decisions encoded above:

- **Persist before `done`**: each logical message is inserted into the DB before the `done` frame is yielded to the client. This guarantees a `done`-completed message that the client sees is durable. (Previous draft put persistence after all `done`s, before `final`; revised after audit.)
- **`final` after all `done`s but before post-process**: keeps the wire ordering clean and ensures `lead_score` / `should_show_cta` reflect the just-persisted messages.
- **Post-process spawned, not awaited**: matches the current sync pipeline behavior.
- **Cancellation**: when the client drops the SSE response, `axum` drops the stream, which drops the generator. In-flight `OpenRouterClient::execute_stream` requests **do not** auto-cancel (this is §1.7 behavior). The persistence and post-process steps must complete; if they happen inside the generator, they will be cut off on drop. Therefore: persistence is wrapped in `tokio::spawn` for fire-and-forget durability of each `done`-completed message, and the generator yields only after the spawn is queued.

### 2.4 HTTP layer (`routes/companion/stream.rs`)

```rust
#[utoipa::path(
    post,
    path = "/comp/chat/{session_id}/message/stream",
    request_body = StreamSendRequest,
    responses(
        (status = 200, description = "SSE event stream", content_type = "text/event-stream"),
        (status = 400, body = ErrorResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, body = ErrorResponse),
        (status = 409, body = DuplicateInProgressResponse),
        (status = 422, body = ErrorResponse),
        (status = 429, body = ErrorResponse),
    ),
)]
pub async fn send_message_stream(
    State(state): State<Arc<AppState>>,
    AuthUser(user): AuthUser,
    Path(session_id): Path<Uuid>,
    Json(req): Json<StreamSendRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    validate_payload(&req)?;                                       // → 400 / 422
    authorize_session(&state, &user, session_id).await?;           // → 403 / 404
    enforce_concurrent_streams_limit(&state, &user).await?;        // → 429

    let (user_msg, replay) = upsert_user_message(&state, session_id, &user, &req).await?;
    if let Some(replay_msgs) = replay {
        return Ok(Sse::new(replay_stream(replay_msgs)));           // §1.10 replay path
    }

    let ctx = SessionContext::load(&state, session_id, user.id).await?;
    let proto = pipeline::run_stream(state.clone(), ctx, user_msg);

    let sse = proto.map(|frame| {
        let json = serde_json::to_string(&frame).expect("frame serialization is infallible");
        Ok::<_, Infallible>(Event::default().data(json))
    });

    Ok(Sse::new(sse).keep_alive(
        KeepAlive::new().interval(Duration::from_secs(15)).text("ping"),
    ))
}
```

Middleware: a new `SseHeadersLayer` injects `X-Accel-Buffering: no` and `Cache-Control: no-cache, no-transform, private` on responses from this handler (and any future SSE handlers). Content-Type is set automatically by axum's `Sse`.

### 2.5 Persistence layer

The data layer must support these capabilities. Concrete column names, migration SQL, and indexing strategy are deferred to the implementation plan (the canonical schema lives in `eros-engine`'s own crates and migrations).

**Required capabilities:**

1. **Idempotency on user message insert**
   - The user message row must carry the caller-supplied `client_msg_id`.
   - Within a 24 h window, `(session_id, client_msg_id)` must be unique for `role = user`.
   - On conflict during insert: do not error out at the DB layer alone — the application must detect the conflict, look up the existing user message and its linked assistant messages, and decide between replay (§1.10) and 409.

2. **Fallback continuation lineage**
   - An assistant message must be able to reference a previously-persisted assistant message via `continues_from_message_id` (nullable).
   - This need not be enforced as a hard foreign key if it complicates writes; a soft reference is acceptable.

3. **Ghost decisions do not create assistant rows, but ARE marked on the user row**
   - A ghost outcome (`action_type = ghost`) yields no assistant row. The `meta.message_id` for a ghost is a protocol-level identifier only and is not persisted.
   - The user message row MUST record a flag (e.g. `ghost_decision: bool`, or an equivalent enum on the row) capturing that the AI chose to ghost. Without this flag, replay (§1.10) cannot distinguish "ghost outcome" from "race: still generating," and the 409-vs-replay decision becomes ambiguous.

4. **Batch-friendly assistant insert**
   - A burst inserts 1..N assistant messages (typically 1; 2 when a single fallback fires). The hard cap on streaming-fallback depth is an open item (§5). The persistence helper should accept a small `Vec` rather than requiring per-message calls.

5. **24 h idempotency cleanup**
   - The deduplication window is 24 h. Older entries may be GC'd or simply ignored by partial-index predicates. The exact mechanism is plan-stage.

### 2.6 Post-process changes

The existing `post_process::run` takes a single assistant message. Update it to take `Vec<(Ulid, full_text, ActionType)>`:

- For each entry in the vec, run the existing `write_turn` and `extract_insights` and `persist_affinity` steps in their current form (these are per-message operations).
- `refresh_lead_score` is invoked **once** at the end, not per message.
- The sync `pipeline::run()` codepath still produces one message and wraps it into a single-element vec when calling `post_process::run`. This is the only behavior change needed in the sync path during 0.2.

### 2.7 Removals in 0.2

- `routes/companion.rs::send_message_async` and its DTOs (`AsyncSendResponse`, `CompanionReplyPayload`)
- `routes/companion.rs::check_pending` and its DTO (`PendingCheckResponse`)
- The route table entries for both
- Any "pending replies" persistence used by these endpoints (verify presence at implementation-plan stage; drop migration if present)
- OpenAPI regeneration removes these endpoints from `openapi.json`

### 2.8 Kept in 0.2 (removed in 0.3)

- `routes/companion.rs::send_message` (the sync handler) and its DTOs (`CompanionReplyResponse`, etc.)
- `pipeline::run` (the sync orchestrator)
- `OpenRouterClient::execute` (the sync LLM client method)
- `agent_training_level` and `typing_delay_ms` fields in `CompanionReplyResponse`

## 3. Migration plan

### 3.1 Release sequence

```
0.1.x (current)        sync /message + long-poll async/check_pending
   ↓
0.2.0                  + /message/stream (SSE)
                       + execute_stream + run_stream
                       + idempotency persistence
                       − send_message_async + check_pending
                       (sync /message + execute + pipeline::run kept)
                       OpenAPI bump: minor
   ↓
0.2.x patches          2-4 week production observation window
                       (gate on metrics in §3.3)
   ↓
eros-engine-web        switches default client to streamMessage
release                (sync path becomes the internal fallback flag)
   ↓
0.3.0                  − sync /message handler
                       − CompanionReplyResponse / Payload
                       − pipeline::run sync
                       − OpenRouterClient::execute sync
                       − typing_delay_ms field
                       OpenAPI bump: major (breaking)
```

Ordering constraints:

- 0.2.0 must ship before web integration. Otherwise web has no `/message/stream` to hit.
- Web must switch before 0.3.0. Otherwise web loses its sync backend with no streaming client ready.
- 0.3.0 does not block on the web release calendar-wise, but in practice should land ≥ 1 week after web's switch is stable in production.

### 3.2 Task dependency graph

```
T1   workspace Cargo.toml: + eventsource-stream / async-stream / futures-util / tokio-stream
T2   OpenRouterClient::execute_stream + unit tests (mock upstream)
T3   ProtocolFrame enum + serde roundtrip tests
T4   pipeline::run_stream (reply / ghost / gift_reaction / fallback paths)
T5   persistence: idempotency capability + upsert_user_message
T6   persistence: batch insert helper
T7   post_process::run signature change to Vec
T8   HTTP handler send_message_stream + SseHeadersLayer
T9   delete send_message_async / check_pending / route entries / DTOs
T10  utoipa annotations + regenerate openapi.json
T11  integration tests: reqwest-driven SSE frame sequence assertions
T12  Fly.io NRT deploy + Cloudflare buffering verification

Dependencies: T1 → T2; T3 independent; (T2, T3) → T4; T5 independent; T4 & T6 & T7 cooperate;
T4 → T8 → T10; T11 after T8; T12 last.
```

### 3.3 Production gate metrics (must pass before 0.3)

| Metric                                                      | Target              | Source                                                      |
|-------------------------------------------------------------|---------------------|-------------------------------------------------------------|
| P50 time-to-first-delta (TTFT)                              | < 700 ms            | Server-side: handler entry → first delta yield               |
| P95 TTFT                                                    | < 1500 ms           | same                                                        |
| Unexpected stream termination rate (non-user)               | < 1 %               | Server-side: streams without client close and without `final` / `error` |
| Idempotency replay correctness                              | 100 %               | Integration tests + production sampling                     |
| Ghost path emits zero `delta` frames                        | strict 0            | Integration test assertions + production anomaly check      |
| Fallback trigger rate                                       | < 5 %               | Server-side: trun­cated done → new meta                       |
| OpenRouter monthly bill delta                               | ≤ +10 %             | Billing dashboard                                            |

### 3.4 Risk register

| #  | Risk                                                                                                                | Severity | Mitigation                                                                                                                                                          |
|----|---------------------------------------------------------------------------------------------------------------------|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| R1 | Cloudflare buffers SSE on `erosnx.etherfun.net`, killing TTFT improvement                                           | High     | Day-one verify: `curl -N` should see `: ping` every ~15 s. If buffered, add CF Page Rule (Cache Level: Bypass) and Workers `cf.cacheTtl=-1`.                       |
| R2 | OpenRouter models have heterogeneous streaming behavior (DeepSeek coarse, Claude Haiku 4.5 fine, Grok 4 fast mid)   | Medium   | `execute_stream` normalizes to `DeltaChunk`. Pipeline does not see upstream framing differences.                                                                    |
| R3 | Fly.io idle-connection timeout (default 60 s) kills long streams                                                    | Medium   | 15 s keepalive comments cover this; also set `[[services]].timeout = 180s` in `fly.toml`.                                                                            |
| R4 | Postgres partial-unique-index predicate `created_at > now() - 24h` is not allowed (`now()` is not IMMUTABLE)        | Medium   | Application-level pre-check before INSERT, or BRIN index + periodic cleanup. Implementation-plan choice.                                                            |
| R5 | Client disconnect → zombie upstream calls → uncontrolled OpenRouter spend                                            | Medium   | §1.9 hard limits bound damage (3 concurrent / 120 s / 4096 tokens). Alert on `OpenRouter completion_tokens/min` from week 1.                                        |
| R6 | Fallback UX looks like a bug: first bubble truncates, second appears with no explanation                            | Medium   | Frontend keeps the truncated bubble visible and shows the second one immediately after; product may add a "fallback bubble merge" patch in 0.2.x if it lands poorly. |
| R7 | Server panic mid-stream → already-`done`-yielded frames lose their persistence chance if persistence is too late    | High     | §2.3 mitigates this: persist each logical message **before** yielding its `done`. The client never sees a `done` for a message the server failed to persist.        |
| R8 | Long-poll deletion breaks an unknown client                                                                          | Low      | Grep eros-engine-web / frontend repos / blog snippets. Frozen `eros-app` v1.4.1 verified not to consume these endpoints.                                            |
| R9 | Cross-team drift between engine and web on `client_msg_id` format / `message_id` type                                | Medium   | This spec defines both: `message_id` is a ULID encoded as a 26-char Crockford Base32 string; `client_msg_id` is any UUID (any version) or ULID, 26-36 chars, ASCII printable, no whitespace. Both sides cite this spec. |

### 3.5 Rollback

- If §3.3 gates fail post-deploy of 0.2.0 (TTFT > 3 s sustained 30 min, or unexpected termination > 5 %, or OpenRouter bill anomaly): Fly.io rollback to the prior 0.1.x image.
- During the 0.2.x window, any streaming-only bug is independently rollback-able: web can switch back to `/message` via a feature flag because the sync handler is untouched.
- Once 0.3.0 ships and removes sync, the rollback floor is "redeploy a 0.2.x image."

### 3.6 Documentation updates

- `eros-engine/docs/api-reference.md` and `.zh.md`: add a full `/message/stream` section; mark `/message` deprecated.
- `eros-engine/crates/eros-engine-server/openapi.json`: auto-regenerated from utoipa annotations.
- `eros-engine/README.md`: update curl examples to the streaming form.
- This spec moves from `eros-docs/docs/superpowers/specs/eros-engine/` into `eros-engine/docs/superpowers/specs/` as the canonical copy when implementation starts (this is part of CLAUDE.md's "per-project specs live in each repo's own `docs/superpowers/`").

## 4. Out of scope

- Last-Event-ID / SSE reconnect resumption (no `id:` field in 0.2).
- Server-pushed unsolicited streams (will use WebSocket if introduced for matching / blind-box realtime).
- Multi-device session synchronization.
- Client-side persistence of un-sent messages.
- Compression of SSE bodies (`Content-Encoding: identity` is fixed).
- Last-mile streaming for non-companion endpoints (matching, gifts) — separate spec when those endpoints exist.

## 5. Open items for implementation plan

These are deliberate gaps for the plan stage to resolve, not unanswered design questions.

- The exact `engine.*` schema columns / index strategy for idempotency and fallback lineage.
- Whether a "ghost decision audit log" table is added now or deferred to a later release.
- Concrete `LlmError` enum changes for streaming-specific failure modes.
- Whether `SseHeadersLayer` becomes a generic axum `Layer` or is folded into the handler.
- Mock-OpenRouter fixture infrastructure for integration tests.
- **Maximum streaming-fallback depth.** The sync pipeline's model fallback chain can be N deep (`grok-4-fast` → `deepseek-v3.2` → `grok-4-mini` per CLAUDE.md). Streaming exposes each fallback as a visible bubble, so depth > 2 may produce a poor UX ("AI sends 3 consecutive partial replies"). Decide the cap at plan time. Strong default suggestion: 2 (= 1 primary + 1 fallback).
