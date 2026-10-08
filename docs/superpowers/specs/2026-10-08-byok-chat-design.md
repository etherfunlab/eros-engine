# Bring-your-own-key chat — Design

- **Date:** 2026-10-08
- **Status:** Draft — ready for review
- **Type:** Engine change. One optional request field and one request header
  on the stream send route, two env vars, no migration.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the stream send route, the chat reply chain,
  the chat queue's recovery path, and the LLM client.

## 1. Motivation

Three parties are involved. The **engine** is this repo. A **downstream** is
a product that runs the engine and calls it from its own server. An **end
user** is a person using that product.

An end user can bring their own OpenAI-compatible chat endpoint and API key,
so the companion's reply on a text chat turn is generated on the end user's
account. The downstream stores the end user's endpoints and keys and decides
who may use the feature. It sends the configuration with each turn. The
engine uses it for that turn only and keeps none of it.

## 2. Principles applied

1. **The engine holds the mechanism; the downstream holds the policy.** The
   downstream decides which end users, tiers and personas get BYOK, whether
   an end user may set `output_regex`, and whether a failed BYOK chain falls
   back to the deployment's own models. The engine enforces only what keeps
   the deployment safe: who may send the field, where the engine may connect,
   and which keys it may spend.
2. **No credential at rest.** Endpoint URLs and keys exist only in the memory
   of the turn that uses them. No table, log line, prompt log or audit column
   receives either.
3. **A request without `byok` is unchanged.** Every new path branches on the
   field's presence.

## 3. Scope

BYOK replaces the model selection of exactly one hop: the `chat_companion`
reply on a turn sent to `POST /comp/chat/{session_id}/message/stream`. That
covers tip turns and the text half of `reply_text_image`.

The following stay on the deployment's configured chains and keys on a BYOK
turn: the PDE judge, `chat_input_filter`, `chat_output_filter`,
`chat_vision`, `chat_image_prompt_compose`, `chat_product_qa`, affinity
evaluation and summary, insight and memory extraction, and embeddings. A BYOK
turn is therefore not free for the deployment.

Not covered: the async send route (§9.3), `/open` greetings, voice turns,
image edits.

## 4. Request contract

### 4.1 The `byok` field

`StreamSendRequest` gains an optional `byok`:

```json
"byok": {
  "providers": {
    "mine": {
      "chat": "https://api.example.com/v1/chat/completions",
      "api_key": "sk-…",
      "headers": { "X-Org": "…" }
    }
  },
  "model": "gpt-x@mine",
  "fallback": ["other-model@mine"],
  "retry_depth": 1,
  "output_regex": [
    { "pattern": "\\s*\\[note:[^\\]]*\\]\\s*$", "replacement": "", "models": ["gpt-x"] }
  ],
  "fallback_to_platform": false
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `providers` | map name → provider | yes | The end user's endpoints. |
| `providers.<name>.chat` | string | yes | Complete chat-completions URL, posted verbatim. |
| `providers.<name>.api_key` | string | yes | Sent as `Authorization: Bearer <api_key>`. |
| `providers.<name>.headers` | map string → string | no | Sent verbatim on every request to this provider. |
| `model` | string \| array \| map | yes | Primary model: fixed, round-robin, or weighted random — the three shapes of `[tasks.*].model`. |
| `fallback` | string \| array | no | Sequential fallback chain. |
| `retry_depth` | integer | no | Fallbacks tried after the primary. Default `2`, as for `chat_companion`. |
| `output_regex` | array of rules | no | Strip rules for this chain's replies (§6.4). |
| `fallback_to_platform` | bool | no | Default `false`. See §6.3. |

Model slugs use the `[providers]` grammar: `<model id>@<name>`, with `\@`
escaping a literal `@` in the id. **Every slug in `model` and `fallback` must
carry a suffix naming a key of `byok.providers`.** A bare slug, an
`@openrouter` suffix, or a name declared only in the deployment's
`[providers]` is rejected. This keeps an end user from spending the
deployment's keys on a model of their choosing.

The BYOK DTOs use `deny_unknown_fields`. Fields that BYOK does not take
(`allow_traits`, `model_name_display_override`, `temperature`, `max_tokens`,
the sampling knobs, `output_filter`, body params) are rejected rather than
silently ignored. On a BYOK turn those settings still come from
`[tasks.chat_companion]` and the request's `tier`, except the display
override, which BYOK hops skip (§6.4).

### 4.2 Validation

All checks run with the other payload checks, before the session is resolved
and before the user row is persisted. A failure is a pre-stream 400
`invalid_payload` whose message names the offending field and never contains
a key, a header value or a URL.

| Item | Rule |
|---|---|
| `providers` | 1–4 entries; names match `^[a-z0-9_]{1,32}$` |
| `chat` | ≤ 2048 bytes; parses as a URL with a host; no userinfo; scheme `https` (`http` also allowed under `BYOK_ALLOW_PRIVATE_NETWORK`); a literal-IP host must pass the address guard (§7.2) |
| `api_key` | 1–1024 bytes; a valid header value |
| `headers` | ≤ 8 entries; valid header names and values, each value ≤ 1024 bytes; `Authorization` and `Content-Type` refused, case-insensitive (the `[providers]` header validator, reused) |
| `model` | non-empty; array and map ≤ 8 entries; map weights finite and > 0 |
| `fallback` | ≤ 8 entries, none empty |
| every slug | parses, and its suffix names a key of `providers` |
| `output_regex` | ≤ 16 rules; `pattern` 1–512 chars; `replacement` ≤ 512 chars; compiles under a 1 MiB size limit and a 1 MiB DFA size limit |

An empty `model` is an error here, unlike in TOML, where it falls through to
the next precedence level: a BYOK chain has nothing to fall through to.

## 5. Admission

`BYOK_CALLER_SECRET` (env, read in `ServerConfig::from_env`) gates the field.

- Unset or blank: BYOK is off. A request carrying `byok` gets 403
  `byok_forbidden`.
- Set: a request carrying `byok` must also carry the header
  `X-Byok-Caller-Secret` with the same value, compared in constant time;
  missing or different → 403 `byok_forbidden`.
- A value shorter than 32 bytes refuses boot.
- Admission runs before the §4.2 checks, so a caller without the secret
  learns nothing about the limits.

The engine authenticates end users by their own bearer token, so an end user
can call the engine directly with the same body a downstream would send. The
secret lives only on the downstream's server; it is what separates "the
downstream sent this" from "the end user sent this". Without it an end user
could bypass every downstream policy listed in §2.

Neither the header nor the field is logged. `TraceLayer::new_for_http` does
not record headers, and the BYOK DTOs implement `Debug` by hand, printing the
provider names and the model shape only.

## 6. Chain execution

### 6.1 Resolution

`resolve("chat_companion", tier)` runs as on every turn. It supplies
`temperature`, `max_tokens`, the sampling knobs, `allow_traits`,
`output_filter` and the platform chain. It also advances the config's
round-robin cursor even when the platform chain goes unused; this shifts that
cursor's sequence and nothing else.

The BYOK primary is selected from `byok.model`:

- **Fixed:** the one slug.
- **Round-robin:** `AppState.byok_rr` is a
  `Mutex<HashMap<Uuid, Arc<AtomicUsize>>>` keyed by `user_id`. The DTO is
  turned into a `ModelSpec::RoundRobin` whose `cursor` is that user's entry,
  so `ModelSpec::select` runs unchanged. Same semantics as config
  round-robin: per process, reset on restart, each replica counting on its
  own. The map holds one small entry per user who sent a round-robin BYOK
  turn since boot.
- **Weighted:** `ModelSpec::Weighted`, unchanged.

The BYOK fallback list drops the selected primary, then is truncated to
`retry_depth`, as in `resolve()`.

### 6.2 The hop list

The reply chain becomes a list of hops, each carrying its slug and its route:

```
[BYOK primary, BYOK fallbacks…]                         route = Byok
++ [platform model, platform fallbacks…]  if fallback_to_platform
                                                        route = Platform
```

Platform hops are `resolved.model` and `resolved.fallback_model`, already
truncated to the platform `retry_depth`. A BYOK slug always carries a BYOK
suffix, so the two halves never collide. `retries_chat` on the `final` frame
counts hops consumed across the whole list.

### 6.3 Exhaustion

- `fallback_to_platform = false`: when every BYOK hop fails, the turn ends in
  an `Error` frame carrying the last hop's `upstream_status` and
  `provider_code`, and the stream turn goes terminal. The downstream can tell
  the end user that their key or endpoint failed.
- `fallback_to_platform = true`: the walk continues into the platform hops,
  and an exhausted platform half ends as any platform chain does.

An exhausted platform chain serves a pseudo-ghost — a fallback phrase from
`engine.error_handling_config` that reads as an ordinary short reply — when
one is configured, and emits the `Error` frame only when none is. A BYOK-only
chain skips the pseudo-ghost: the phrase would hide from the end user that
their key or endpoint failed. A complete byte-BPE garble met on the chain is
still served repaired, as on any chain; that text came from the end user's
model.

### 6.4 Per-hop behaviour

| | Byok hop | Platform hop |
|---|---|---|
| client | the turn's BYOK client (§7.1) | `state.openrouter` |
| `output_regex` | the BYOK rules | `state.output_regex` |
| `meta.model` | the hop slug's bare id, always | `model_name_display_override`, as today |

BYOK rules compile once per turn into `CompiledRegexRule`s. A rule without
`models` gets the bare ids of every BYOK hop, so `apply_output_regex` and
`StreamScrubber` run unchanged. Execution order, the `pre_filter_content` /
`filter_model = "<regex>"` audit and the empty-bubble behaviour are those of
the config rules.

Garble repair, the empty-reply ghost fallback, `output_filter` and the
`filtered` flag behave as on any turn. `output_filter`, when the tier enables
it, runs on the deployment's key.

### 6.5 Wire compatibility

BYOK hops receive the strict OpenAI-compatible subset that custom
`[providers]` entries receive, with the deployment's `chat_companion`
`temperature`, `max_tokens` and sampling knobs. A provider that rejects one
of those (for example `repetition_penalty`, or `max_tokens` on a model that
wants `max_completion_tokens`) fails the hop with a 400, which the chain
treats like any other upstream failure. BYOK offers no override for these
parameters; see §12.

## 7. Transport

### 7.1 The BYOK client

A per-turn `OpenRouterClient` built from the validated `providers`:

- its `providers` map holds one `ProviderEndpoint` per BYOK provider, with an
  empty `body_rules`;
- its built-in endpoint has an empty key, so a bare slug cannot post;
- it posts through `AppState.byok_http` (§7.2);
- every model string it records for audit is `<id>@byok` (§8.1).

The client is dropped with the turn.

### 7.2 Address guard

`byok_http` is one `reqwest::Client` built at boot:

- redirects disabled;
- `no_proxy()` — a proxy would resolve the host itself and bypass the guard;
- `https_only(true)` unless `BYOK_ALLOW_PRIVATE_NETWORK` is set;
- connect and pool timeouts as for `plain_http`; stream timeouts as for every
  chat hop;
- a custom DNS resolver that resolves the host, drops every address the guard
  refuses, and fails the lookup when none remain. The connection is made to
  an address that passed, so a rebinding DNS answer cannot redirect it.

The guard refuses:

| IPv4 | IPv6 |
|---|---|
| `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `224.0.0.0/4`, `240.0.0.0/4` | `::`, `::1`, `fc00::/7`, `fe80::/10`, `ff00::/8`; `::ffff:0:0/96` and `64:ff9b::/96` judged by their embedded IPv4 |

reqwest does not consult the resolver for a literal-IP host, so §4.2 applies
the same guard to literal IPs at validation. A host refused at connect time
fails that hop as a `transport` gateway error, and the chain advances.

`BYOK_ALLOW_PRIVATE_NETWORK` (truthy) disables the guard and allows `http`,
for self-hosted deployments pointing BYOK at a model on their own network.

## 8. Audit and secret hygiene

### 8.1 What is recorded

Every model string recorded for a BYOK hop is `<id>@byok`. The provider
name the end user chose is not recorded; it means nothing outside one
request.

- `engine.llm_generations.model` — written by `record_generation` with the
  hop's slug — reads `<id>@byok`; `generation_id` is the provider's own id,
  and `usage` carries no `cost` unless the provider sent one.
- `llm_attempts[].model` and `gateway_errors[].model` read `<id>@byok`.
- The prompt log (`PROMPT_LOG_DIR`) prints the chain the turn walks, BYOK
  hops as `<id>@byok`.

That a turn asked for BYOK is recorded once, on its queue row:
`chat_turn_queue.params->'byok'` (§9.1), reached from the reply through
`user_message_id`. Which hop served is the generation's `model`. Nothing on
`chat_messages` repeats either fact.

### 8.2 What is never recorded

URLs, keys and header values, anywhere: table rows, `chat_turn_queue.params`
and `last_error`, tracing, the prompt log, panic messages. Three spots need
code:

- `ProviderEndpoint`'s `Debug` prints `base_url`. It now prints
  `<redacted>` for every endpoint, configured or BYOK; some providers take
  the key in the query string.
- `reqwest::Error`'s `Display` includes the URL. Every `reqwest::Error` from a
  BYOK hop is passed through `without_url()` before it becomes an `LlmError`,
  so the `gateway_errors` message, the warn line and `last_error` cannot
  carry it.
- A provider may echo the key in an error body. On a BYOK hop every verbatim
  occurrence of the hop's `api_key` in `llm_attempts[].message` is replaced
  with `<redacted>`.

## 9. Queue and recovery

### 9.1 What the queue row carries

The stream route persists `QueuedTurnParams` before generation, as today.
`QueuedTurnParams` gains

```rust
#[serde(default)]
pub byok: Option<QueuedByok>,   // QueuedByok { fallback_to_platform: bool }
```

It records that the turn asked for BYOK and nothing about the endpoints, the
models or the key. The BYOK configuration itself is threaded from the
handler into the drive task in memory: `drive_to_exhaustion` gains a
`byok: Option<Arc<ByokTurn>>` argument. The stream handler passes the
turn's; the worker's `drive_turn` passes `None`.

### 9.2 A BYOK turn reaching the worker

The worker drives a stream turn only through crash recovery, after the reaper
releases a claim older than `GEN_TIMEOUT + CLAIM_STALE`. The configuration
died with the process. When `params.byok` is present:

- `fallback_to_platform = false`: no generation. `drive_turn` returns a new
  `TurnOutcome::Unservable("byok_unavailable")`, which `settle_turn` makes
  terminal in any mode: the queue row goes `failed` with
  `last_error = 'byok_unavailable'` and the usual `system_error` row is
  written. A retry could not succeed, so the ladder is skipped.
- `fallback_to_platform = true`: the turn is driven on the platform chain,
  the outcome the request asked for when BYOK cannot serve.

### 9.3 The async route

`POST /v2/comp/session/{session_id}/message/async` shares
`StreamSendRequest`. A body carrying `byok` gets 400 `invalid_payload`
("byok is not accepted on the async endpoint"): its turn is generated by the
worker, which never holds the configuration.

## 10. Where it lives

| change | file |
|---|---|
| address guard, guarded resolver, guarded client builder, `@byok` audit slug, size-limited regex compile | `crates/eros-engine-llm/src/byok.rs` (new) |
| BYOK client constructor; `@byok` echo label; URL stripping; key scrubbing in error text | `crates/eros-engine-llm/src/openrouter.rs` |
| `Debug` redaction of every endpoint's URL | `crates/eros-engine-llm/src/provider.rs` |
| `ModelSpec::select` made public; header-pair check shared by `[providers]` and BYOK | `crates/eros-engine-llm/src/model_config.rs` |
| request DTOs, admission, validation, `ByokTurn`, the hop list, the per-user cursors, `QueuedByok` | `crates/eros-engine-server/src/byok.rs` (new) |
| field on `StreamSendRequest`; admission and validation wiring; threading into the drive task | `crates/eros-engine-server/src/routes/companion_stream.rs` |
| `QueuedTurnParams.byok`; 400 on the async route | `crates/eros-engine-server/src/routes/companion_async.rs` |
| per-hop client / rules / display; BYOK-only exhaustion skips the pseudo-ghost | `crates/eros-engine-server/src/pipeline/stream.rs` |
| `drive_to_exhaustion` argument; `TurnOutcome::Unservable`; recovery behaviour | `crates/eros-engine-server/src/pipeline/chat_queue.rs` |
| `BYOK_CALLER_SECRET`, `BYOK_ALLOW_PRIVATE_NETWORK`; `AppState.byok` | `crates/eros-engine-server/src/state.rs`, `main.rs` |
| `@byok` chain labels | `crates/eros-engine-server/src/prompt_log.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |
| docs | `docs/byok.md` / `.zh.md` (new); `docs/api-reference.md` / `.zh.md`; `docs/model-config.md` / `.zh.md`; `docs/llm-audit.md`; `.env.example`; README feature lists and `examples/*.toml` checked |

`docs/byok.md` carries a security section stating that a BYOK turn sends the
whole reply prompt — persona definition, recalled memories, insights, world
state — to an endpoint the end user controls, and that choosing which
personas admit BYOK is the downstream's decision.

## 11. Configuration and rollback

Two env vars. `BYOK_CALLER_SECRET` turns the feature on; without it every
`byok` field is refused and nothing else changes. `BYOK_ALLOW_PRIVATE_NETWORK`
is for self-hosted networks and tests.

No migration. Rolling the image back leaves `params.byok` markers on queue
rows. The older engine ignores the unknown key, so a BYOK turn it recovers is
driven on the platform chain regardless of `fallback_to_platform`; draining
the queue before rolling back avoids that.

## 12. Testing

- **address guard** (unit, llm crate). Every range in §7.2 refused, including
  `::ffff:10.0.0.1` and `64:ff9b::a00:1`; public v4 and v6 addresses pass;
  `BYOK_ALLOW_PRIVATE_NETWORK` passes everything.
- **BYOK client** (unit). A bare slug and an `@openrouter` slug fail to post;
  `Debug` of a BYOK endpoint contains neither URL nor key; a transport error
  from a BYOK hop has no URL in its `Display`; a provider message echoing the
  key is recorded redacted.
- **validation** (unit, server). Each limit in §4.2 at and past its bound; a
  bare, `@openrouter` and undeclared-name slug each rejected; an unknown
  field rejected; a literal private IP and `http` rejected without the
  opt-in; an over-size regex rejected; no error message contains the key.
- **admission** (unit). Secret unset, header missing, header wrong → 403;
  header right → admitted; short secret fails boot.
- **hop list** (unit). BYOK only; BYOK + platform; primary removed from the
  fallback list; truncation on both halves; round-robin advances per user
  and independently across users.
- **routes** (`#[sqlx::test]` + wiremock, `BYOK_ALLOW_PRIVATE_NETWORK` on
  except where noted):
  - a BYOK turn: the BYOK mock sees `Authorization: Bearer <end-user key>`
    and the declared headers; the OpenRouter mock sees nothing; the
    generation's `model` is `<id>@byok`; the queue row's `params.byok` is
    `{"fallback_to_platform": false}`; a BYOK regex rule strips; with a
    display override configured, `meta.model` is the bare id;
  - BYOK chain fails, no fallback: `Error` frame with the mock's status, and
    no pseudo-ghost although the test database seeds fallback phrases;
    terminal failure;
  - BYOK chain fails, `fallback_to_platform`: the OpenRouter mock serves; the
    configured display override and config rules apply on that hop;
  - 403 without the header; 400 on the async route;
  - recovery: a claimed BYOK turn released by the reaper goes `failed` with
    `byok_unavailable` (no fallback), or is served by the platform chain
    (fallback);
  - **no credential at rest:** after the turns above, every `chat_messages`,
    `chat_turn_queue` and `llm_generations` row of the test session, rendered
    to text, contains neither the key nor the URL;
  - guard on: a provider at `https://localhost:<port>/…` fails its hop as a
    `transport` gateway error.
- `openapi.json` regenerated; the diff shows only the `byok` field, its
  schemas and the header.

## 13. Non-goals

- Credential storage, encryption, key validation, UI, per-tier or per-persona
  admission, billing — all downstream.
- BYOK on the async route, `/open`, voice, image edits, or any auxiliary
  task.
- BYOK body params (e.g. switching reasoning off) and BYOK sampling
  overrides.
- `allow_traits` or a display override supplied through BYOK.
- Persisting the round-robin cursor.

## 14. Decisions taken

- **Stream route only, configuration in memory**, over a downstream callback
  the engine queries for credentials (an extra hop per turn and a new failure
  surface) and over a sealed credential blob stored in `params` (key material
  at rest). The cost is a BYOK turn caught by a deploy, which fails or falls
  back per its own flag.
- **`fallback_to_platform` per request, default `false`**, over always
  failing (the downstream could only resend, duplicating the user row) and
  always falling back (an end user with a dead key would spend the
  deployment's money without knowing their key is dead).
- **A caller secret header**, over an HMAC-signed `byok` block (canonical
  serialization on both sides for no gain while the secret never leaves the
  downstream's server) and over a plain on/off switch (direct calls would
  bypass downstream policy).
- **A per-turn client over a BYOK field on `ChatRequest`**, which would thread
  a per-request concept through every task's call path, and over a separate
  HTTP client, which would duplicate stream parsing, garble repair and
  failure classification.
- **Every BYOK slug names a BYOK provider**, so BYOK can never spend the
  deployment's keys.
- **BYOK hops skip the display override** and always show the real id; the
  override is the deployment's presentation of its own models.
- **One `@byok` audit label** regardless of the provider name.
- **"This turn asked for BYOK" lives only on the queue row**, which every
  stream turn already has, over a second copy in the reply row's metadata.
- **A BYOK-only chain never serves the fallback phrase**: the end user must
  be able to tell that their own key or endpoint failed.
- **An in-process round-robin cursor per user**, matching config round-robin,
  over a per-session count: the session's message count moves by two per
  turn, so it cannot drive a two-model rotation.
