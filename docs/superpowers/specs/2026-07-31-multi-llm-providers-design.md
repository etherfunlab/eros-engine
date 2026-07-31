# eros-engine — multiple LLM providers via `[providers]` + `model@provider`

Lets a deployment route individual model slugs to its own OpenAI-compatible
endpoints (Venice, a self-hosted proxy, anything speaking
`POST /chat/completions`) alongside the built-in OpenRouter client, by declaring
endpoints in a `[providers]` block and suffixing model slugs with `@name`.

Today every LLM call in the engine goes to one pinned URL
(`https://openrouter.ai/api/v1/chat/completions`, `openrouter.rs:12`) with one
API key. A deployment that wants a second vendor has no seam at all. This design
adds one without changing a single byte of the request the OpenRouter path
already sends.

---

## 0. Decisions (settled during brainstorm)

- **Per-provider API keys come from a naming convention, not from config.**
  `venice` reads `$VENICE_API_KEY`. No secret ever appears in a TOML file, and
  there is no `api_key_env` indirection field. The cost is that provider names
  are constrained (§1) so the env-var name is derivable without mangling.
- **Custom providers receive a strict OpenAI-compatible request.** The four
  OpenRouter-specific body fields and the three attribution headers are dropped
  (§4). Consequence, documented rather than papered over:
  `[defaults].ignore_providers`, `[defaults].provider_sort`, and per-task
  `reasoning` are **inert** on custom providers.
- **The engine never translates model ids across providers.** Whatever precedes
  the `@` goes on the wire verbatim, in that provider's own naming. There is no
  mapping table and no attempt to make a Venice slug look like an OpenRouter
  slug.
- **`\@` escapes a literal `@`; written `"\\@"` in TOML.** `\@` is a *reserved*
  escape in TOML basic strings, so the naive `model = "a\@b"` is a TOML parse
  error at column 11 — before the engine sees the config and with no chance to
  explain itself. The documented form is therefore a normal double-quoted
  string with a doubled backslash. (Verified against `toml 0.8`, the version
  this crate pins.)
- **Exactly one unescaped `@` is permitted.** Two or more refuses to boot. This
  makes `"aaa@bb"` fail with *unknown provider `bb`*, which points the operator
  straight at the escape rather than silently guessing an interpretation.
- **`OpenRouterClient` keeps its name.** It is public API of a published crate
  (`eros-engine-llm`); a rename is a semver break with no functional payoff, and
  is separable from this feature.
- **Model-keyed config tables use bare ids** (§5). `@provider` appears only in
  `model` and `fallback`.
- **The draw endpoint is out of scope and rejected at boot** (§7), pending its
  removal in a follow-up (§11). This is scoped to
  `[tasks.chat_image_generation]` only — `chat_image_prompt_compose` is a
  normal chat-shaped task and supports `@provider`.

---

## 1. The `[providers]` block

```toml
[providers]
venice = "https://api.venice.ai/api/v1/chat/completions"
someprovider = "https://someprovider/api/v1/chat/completions"
```

Parsed into `ModelConfig` as `#[serde(default)] pub providers: HashMap<String, String>`,
sibling to `defaults` and `tasks`.

Under `MODEL_CONFIG_DIR` (multi-file merge, `from_toml_dir`) the block needs no
new merge code — but like `[defaults]` and unlike `[tasks]`, it merges as **one
whole top-level key**: two files each declaring `[providers]` refuse to boot
with the existing duplicate-definition error. All providers live in one file.

**Name (the table key)**

- Must match `^[a-z0-9_]+$`. Rejected at boot otherwise, with a message telling
  the operator to use underscores. The constraint exists because the name is
  uppercased directly into an env-var name; permitting `-` or `.` would require
  a mangling rule, and mangling rules are exactly the kind of implicit magic
  this design is avoiding.
- `openrouter` is **reserved**. Declaring it refuses to boot with a message
  pointing at `OPENROUTER_BASE_URL` (§8). One knob, one way to turn it.

**Value**

The complete URL including path. Posted verbatim — the engine performs no path
joining and never appends `/chat/completions`. What you write is what gets
called.

**API key**

Derived as `<NAME_UPPERCASED>_API_KEY`: `venice` → `$VENICE_API_KEY`.

The key is required to exist and be non-empty **only for providers actually
referenced by some model slug**. A declared-but-unreferenced entry needs no key
— that is what lets `examples/model_config.toml` ship a sample `[providers]`
block without breaking boot for everyone who copies it. A referenced provider
with a missing or empty key refuses to boot, matching the existing
`VOYAGE_API_KEY` loud-fail precedent (`main.rs:228`).

---

## 2. Model slug grammar

The grammar applies to the string **after** TOML parsing.

| Slug (post-TOML) | Model id sent | Endpoint |
|---|---|---|
| `x-ai/grok-4.20` | `x-ai/grok-4.20` | built-in OpenRouter |
| `some-slug@venice` | `some-slug` | `venice` |
| `weird\@vendor/m` | `weird@vendor/m` | built-in OpenRouter |
| `weird\@vendor/m@venice` | `weird@vendor/m` | `venice` |
| `a@b@venice` | — | **boot error**: 2 unescaped `@` |
| `@venice` | — | **boot error**: empty model id |
| `x@` | — | **boot error**: empty provider name |

In TOML these are written `"x-ai/grok-4.20"`, `"some-slug@venice"`,
`"weird\\@vendor/m"`, `"weird\\@vendor/m@venice"`.

Implemented as a free function so it is unit-testable without a client:

```rust
pub fn split_model_slug(slug: &str) -> Result<(String, Option<&str>), SlugError>;
```

Returns the unescaped model id and the provider name (`None` ⇒ built-in
OpenRouter). Backslashes not preceding `@` are left alone — only the two-char
sequence `\@` is meaningful, so a trailing lone `\` is part of the model id.

`SlugError` carries the offending slug and the reason. Boot validation (§7)
renders it into the `Result<(), String>` message; `resolve_endpoint` (§3) maps
it to `LlmError::Config`.

---

## 3. Client routing

`OpenRouterClient` gains two fields:

```rust
providers: Arc<HashMap<String, ProviderEndpoint>>,  // name -> { base_url, api_key }
plain_http: reqwest::Client,                        // no attribution headers
```

`plain_http` is built at boot beside the existing `http`, with identical
`connect_timeout` / `pool_idle_timeout` and *no* `default_headers`. It exists
solely to honour "no attribution headers to custom providers": those three
headers are baked into the shared client at construction (`openrouter.rs:690-737`)
and cannot be withdrawn per-request.

Resolution returns a borrowed view:

```rust
struct Endpoint<'a> { url: &'a str, api_key: &'a str, http: &'a reqwest::Client, openrouter: bool }

fn resolve_endpoint(&self, slug: &str) -> Result<(String, Endpoint<'_>), LlmError>;
```

**Three** call sites switch from `self.base_url` / `self.api_key` / `self.http`
to the resolved endpoint: `execute_vision` (920), `call_once` (1175),
`execute_stream_as` (1287).

`execute_image_inner` (1062) deliberately does **not**. It keeps posting to
`self.base_url` with `self.api_key` and passes the whole slug through as the
model id. The reason is `effective_image_chain` (`model_config.rs:1819`), whose
first candidate is `req_model` — a **client-supplied, per-turn** string. Routing
the draw endpoint through `resolve_endpoint` would let any client send
`req_model = "x@venice"` and reach an arbitrary configured provider, bypassing
the boot validation in §7 entirely. Left as-is, that input lands at OpenRouter
as a nonsense model id and returns 400 — the same failure any other junk
`req_model` already produces.

**Why these three points.** Fallback chains are walked in two different places:
`execute` / `execute_vision` iterate candidates internally, while the streaming
path iterates in the **server** (`pipeline/stream.rs:549`, `voice.rs:179`),
handing `execute_stream_as` one model id per attempt. These `.post()` sites are
the only place both shapes meet — the point where a model *string* becomes a
*request*. Resolving there means mixed chains
(`model = "a@venice"`, `fallback = ["b"]`) work with no coordination code, and
`pipeline/stream.rs` needs no change to its call form.

The `if self.api_key.is_empty()` guard currently at the head of the three
switching methods moves into `resolve_endpoint`, where it checks *that
endpoint's* key. `execute_image_inner` keeps its existing guard on
`self.api_key`, consistent with staying pinned to the built-in endpoint.

---

## 4. Wire shape per endpoint

| Field | OpenRouter | Custom provider |
|---|---|---|
| `model`, `messages`, `temperature` | sent | sent |
| `top_p`, `frequency_penalty`, `presence_penalty` | sent | sent |
| `max_tokens`, `stream`, `user`, `response_format` | sent | sent |
| `session_id` | sent | **dropped** |
| `metadata` | sent | **dropped** |
| `reasoning` | sent | **dropped** |
| `provider` (ignore / sort) | sent | **dropped** |
| `HTTP-Referer`, `X-OpenRouter-Title`, `X-OpenRouter-Categories` | sent | **not sent** |

Three body builders, so three edit sites:

- `call_once` and `execute_stream_as` **share `WireRequest`** (1157 and 1266;
  the comment at 1262 exists precisely to stop those two from drifting). Add
  `WireRequest::for_endpoint(self, ep) -> Self`, which sets the four fields to
  `None` for a custom endpoint. All four already carry
  `skip_serializing_if = "Option::is_none"`, so `None` alone removes them from
  the wire — no separate serialization path.
- `execute_vision` builds a `serde_json::Value` and then injects
  `body["provider"]` (912-917). That injection becomes conditional.
- `execute_image_inner` is untouched. §7 keeps *config-supplied* draw slugs
  suffix-free; a client-supplied `req_model` may still carry an `@`, which
  passes through verbatim as a (nonsense) model id per §3 — never interpreted
  as routing.

**Known weakness, closed by test rather than by types.** Nothing in this design
*forces* a future OpenRouter-specific field to be added to the drop list. The
guard is a serialization test asserting that a custom-endpoint body's key set is
a **subset** of the allow-list `{model, messages, temperature, top_p,
frequency_penalty, presence_penalty, max_tokens, stream, user, response_format}`.
Subset, not equality — `top_p`, `user`, `response_format` are `Option` and
`stream` is skipped when false, so a strict equality assertion would fail on
correct output. A newly added un-dropped field is not in the allow-list, so the
subset assertion still catches it. This also covers the `json!`-built vision
body, which a type-level scheme would not.

---

## 5. Model-keyed config tables use bare ids

Three tables key off "which model is this", independent of routing:

| Table | Consumer |
|---|---|
| `display` (`DisplayOverride`, `model_config.rs:270`) | `stream.rs:535/1071/1148` |
| `output_regex.models` (`OutputRegexRule.models`) | `apply_output_regex` |
| `output_filter.trigger` | `should_filter(model_id, …)`, `stream.rs:1210` |

All three receive the **unescaped, suffix-stripped** model id. Operators write
`display = { "grok" = "小灰" }`, never `"grok@venice"`. One rule to remember:
`@` appears only in `model` and `fallback`.

`display` is the load-bearing case rather than a consistency nicety:
`DisplayOverride::Bool(true)` returns the real model id straight to the client,
so an unstripped slug would leak the deployment's provider topology to end
users.

Change sites — all in `stream.rs`, all the same one-line shape (resolve the
bare id before passing `model_id`): the three `d.display(…)` calls
(535/1071/1148), the two `apply_output_regex(…)` calls (712/1162), the
`should_filter(…)` call (1210), and the streaming scrubber constructor
`StreamScrubber::new(&state.output_regex, model_id)` (527) — the scrubber
selects `output_regex` rules by model id, so it is the same model-keyed
semantics as `apply_output_regex`. One more sits on the replay path: the
idempotent-retry Meta frame feeds the **persisted** `row.model` — which §6
stores as the full slug — into `d.display(…)` (stream.rs:3926), so it strips
the same way. Since several of these cluster per attempt, the implementation
should compute the bare id once per candidate iteration, not per call.
(`voice.rs` and the product-QA loop consume none of these tables — audit
only, which stays on the full slug.)

---

## 6. Audit columns

`OpenRouterCallMeta { generation_id, model, usage }` feeds
`companion_insights_events` / `companion_affinity_events`, and `generation_id`
is the join key into OpenRouter's own logs from `eros-audit`.

- **Streaming: unchanged.** `stream.rs:744/1034/1289` already persist
  `model_id.clone()` — the config slug verbatim, not the response echo. An
  OpenRouter row stays byte-identical; a custom row is already
  `<slug>@venice`. Zero conversion, zero ambiguity, zero regression risk.
- **Non-streaming:** `call_once` returns `parsed.model` (the response echo). For
  a custom endpoint it returns
  `Some(format!("{}@{}", parsed.model.unwrap_or(bare_id), provider))`.
  OpenRouter is untouched.
- **`generation_id`:** stored verbatim, whatever the provider returned. No
  NULLing and no schema change. The `model` column already says `@venice`, so a
  failed `eros-audit` join explains itself from the row.
- The garble-salvage path (`execute:868-880`) already fills the candidate string,
  which carries the suffix. Unchanged.

Model naming is *not* normalized across providers (§0), so a custom row's model
segment is that vendor's own slug. That is intended.

---

## 7. Boot validation

Follows the established `pub fn validate_*(&self) -> Result<(), String>` shape
(cf. `validate_prompt_variants`, `model_config.rs:2040`), with `anyhow::bail!` at
the call site in `main.rs`.

**Literal full scan.** Walk `[defaults].fallback_model` plus every task's and
every tier's `model` and `fallback`, visiting **each individual candidate
string** — every element of a round-robin array and every key of a weighted
table. For each: grammar is valid (§2), the named provider is declared in
`[providers]`, and its `<NAME>_API_KEY` is present and non-empty.

Scanning literally rather than via `resolve()` is required, not stylistic: a
weighted table picks at random per call, so validating only what `resolve()`
happens to return would let a provider with no key lie dormant until some
unlucky draw at 3am.

**Provider table checks.** Names match `^[a-z0-9_]+$`; `openrouter` is not
declared; every URL is non-empty.

**The draw endpoint.** Any `@` in `[tasks.chat_image_generation]`'s own `model`
or `fallback` refuses to boot, with a message explaining that the draw endpoint
uses OpenRouter's `modalities` extension (`build_image_body`,
`openrouter.rs:395`, sends `modalities: ["image"]` plus top-level
`width`/`height` and reads back `choices[].message.images[].image_url.url` — not
OpenAI-compatible in either direction).

This restriction is scoped to the **draw endpoint**
(`POST /comp/chat/{session_id}/image/stream`), which is the only consumer of
that block. It does not touch the rest of the image path: the chat stream never
draws, it emits an `image_request` frame, and
`[tasks.chat_image_prompt_compose]` is an ordinary chat-shaped task that
supports `@provider` like any other.

**A literal check is complete here — no inheritance to chase.**
`[defaults].fallback_model` is consumed only by `resolve()`
(`model_config.rs:1277`, `1285`), the text-task resolver. The draw path never
touches it: `resolve_image_gen` (1657) reads only `task_cfg.fallback`, and
`effective_image_chain` (1819) composes only `req_model` + `r.model` +
`r.fallback_model`. A global text fallback therefore cannot leak into a draw
chain, so scanning `[tasks.chat_image_generation]`'s own two fields is
exhaustive for config-supplied slugs.

The one slug this check cannot cover is `req_model`, which arrives per-turn
from the client rather than from config. That is handled structurally instead,
by keeping the draw endpoint off `resolve_endpoint` entirely (§3).

**Runtime.** After a full boot scan an unknown provider is unreachable in
theory, but `execute_stream_as` receives a `&str` from the server, so
`resolve_endpoint` still returns `LlmError::Config` on a miss. That advances the
candidate chain rather than panicking, which is also what makes a mixed chain
degrade correctly when a whole vendor is down.

---

## 8. Environment

| Variable | Required | Meaning |
|---|---|---|
| `OPENROUTER_BASE_URL` | no | Overrides the built-in OpenRouter endpoint. Empty string treated as unset, matching the existing `OPENROUTER_APP_*` handling (`main.rs:216-226`). |
| `<NAME>_API_KEY` | per §1 | Key for the `[providers]` entry `name`. |

Boot wiring goes in `main.rs` after `model_config` loads and before the client
is built (~358), joining the existing chain:

```rust
// Validation and construction are separate calls, in this order.
// validate_providers borrows only the config; build_providers reads the
// environment and can therefore assume every referenced key exists.
if let Err(msg) = model_config.validate_providers() { anyhow::bail!(msg); }
let providers = model_config.build_providers();   // -> HashMap<String, ProviderEndpoint>

let openrouter = Arc::new(
    OpenRouterClient::new(openrouter_key, attribution)
        .with_openrouter_base_url(std::env::var("OPENROUTER_BASE_URL").ok().filter(|s| !s.is_empty()))
        .with_providers(providers)
        .with_ignore_providers(model_config.defaults.ignore_providers.clone())
        .with_provider_sort(model_config.defaults.provider_sort.clone()),
);
```

Two naming notes:

- `validate_providers` owns **every** rule in §7 — grammar, name constraints,
  the reserved `openrouter` key, declared-ness, and the referenced-provider key
  requirement (it needs `self.tasks` to know what "referenced" means, which is
  why it lives on `ModelConfig` rather than in `main.rs`). `build_providers`
  performs no checks; it runs only after validation passed.
- The new builder is `with_openrouter_base_url`, *not* `with_base_url` — the
  latter already exists as the associated constructor
  `with_base_url(api_key, attribution, base_url)` used by tests
  (`openrouter.rs:689`) and stays as-is. The new one is a consuming builder
  taking `Option<String>`, where `None` keeps the pinned default.

---

## 9. Testing

- `split_model_slug` unit tests: no `@`; one `@`; `\@` escape; escape and
  suffix together; two unescaped `@`; empty model id (`@venice`); empty provider
  (`x@`); trailing lone backslash.
- Boot validation: a red and a green case per rule in §7, including the
  `chat_image_generation` literal check and the weighted-table full scan. Also
  a green case asserting `chat_image_prompt_compose` **accepts** `@provider`,
  so the draw-endpoint restriction can never widen to the compose task by
  accident.
- **Wire allow-list lock** (§4): custom-endpoint body key set ⊆ allow-list.
- **Draw endpoint stays on OpenRouter** (§3): a client-supplied
  `req_model = "x@venice"` posts to the OpenRouter base URL with `x@venice` as
  the model id — never to Venice. Locks the one path boot validation cannot
  reach.
- wiremock integration: two mock servers; assert the custom request reaches the
  right URL with the right bearer token, carries **none** of the three
  attribution headers, and omits **all four** OpenRouter-only body fields.
- Mixed chain: primary `@venice` fails → fallback falls back to OpenRouter and
  succeeds.
- **Regression lock:** with no `[providers]` and no suffixes anywhere, the
  OpenRouter request body and headers are byte-identical to today.

---

## 10. Documentation

- `.env.example`: `OPENROUTER_BASE_URL` and a `VENICE_API_KEY` sample, both
  commented out.
- `examples/model_config.toml`: a `[providers]` section covering the `"\\@"`
  spelling, the bare-id rule for `display` / `output_regex` / `trigger`, the
  naming constraint, and the fields that go inert on custom providers.
- `README.md` / `README.zh.md` / `README.ja.md`: synced per existing practice.

---

## 11. Out of scope / follow-up

**Removing the draw endpoint** — a separate PR after this one lands. After it,
the engine *composes image prompts but never calls a generation API*: the chat
stream emits an `image_request` frame carrying the composed prompt, and the
consumer calls whichever image vendor it likes.

Rationale, as stated by the maintainer:

1. Driving image generation was never a primary capability of the engine, and
   carrying it costs more maintenance than it returns.
2. Image API shapes differ per vendor and are actively churning — OpenRouter
   itself has since shipped a new image API — so this is not a surface worth
   generalizing behind the `[providers]` abstraction.

Scope of that PR:

| Removed | Kept |
|---|---|
| `[tasks.chat_image_generation]` | `[tasks.chat_image_prompt_compose]` — becomes the engine's only image output |
| `POST /comp/chat/{session_id}/image/stream` | PDE actions `reply_image` / `reply_text_image` |
| `execute_image`, `execute_image_inner`, `build_image_body`, `plan_attempts` | the `image_request` frame |
| `ImageGenRequest` / `ImageGenResponse` / `ImageGenError`, `ResolvedImageGen`, `effective_image_chain` | `ImageReplyParams`, `image.prompt_variant` (PR #201) |
| the draw-only `default_style` / `aspect_ratio` / `resolution` fields | |

The chat stream already never draws — availability of the image PDE actions
stopped depending on `[tasks.chat_image_generation]` some time ago
(`docs/model-config.md:352`), so removing the block does not touch the PDE
actions or the compose task.

**Not addressed here:** per-provider timeouts or retry policy; non-chat
endpoints (embeddings stay on Voyage); any provider-specific extension
parameters such as Venice's `venice_parameters`; and renaming `OpenRouterClient`
(§0).
