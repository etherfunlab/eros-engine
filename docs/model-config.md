# Model config

[English](model-config.md) · [中文](model-config.zh.md)

LLM model selection for the engine lives in a TOML file loaded at server start. Per-task model + parameters, with optional per-tier overrides on top.

## Where it lives

- Default path: `examples/model_config.toml` (relative to the working directory). The file under `examples/` is an illustrative template — adapt it to your own needs (or point `MODEL_CONFIG_PATH` at your own file).
- Override: `MODEL_CONFIG_PATH` environment variable (single file), or `MODEL_CONFIG_DIR` (directory mode, below). The two are mutually exclusive — setting both is a boot error.
- Loaded once at server start by `eros-engine-server/src/main.rs` (`resolve_config_source` → `ModelConfig::from_toml_file` / `from_toml_dir`). For library embedders, `ModelConfig::load()` in `crates/eros-engine-llm/src/model_config.rs` does the same resolution with the same default path (`examples/model_config.toml`).
- Held as `Arc<ModelConfig>` in `AppState`; shared across all chat / post-process turns
- The server also calls `dotenvy::dotenv()` at startup, so `cp .env.example .env` works for the quickstart without an explicit `source .env`

## Directory mode

`MODEL_CONFIG_DIR` points at a directory whose `.toml` files are merged into one config at boot — for splitting a large config by section, not for layering:

- Only the directory's top level is read (no recursion); dotfiles and non-`.toml` entries are skipped. A directory with no `.toml` files is a boot error.
- Files are parsed independently and merged; load order is filename byte order (order can't change the result — duplicates are errors — it only keeps messages deterministic).
- `[defaults]` and each `[tasks.<name>]` must come from exactly one file. A section defined twice fails boot naming both files: `model_config merge failed: [tasks.chat_companion] in chat.toml already defined in base.toml`. There is no override or precedence between files.
- On success the server logs the merged file list: `model_config: loaded from dir` with the directory, filenames, and count.
- The published Docker image bakes `MODEL_CONFIG_PATH` (`docker/Dockerfile`). To use directory mode in that image, clear it alongside your dir: `-e MODEL_CONFIG_PATH= -e MODEL_CONFIG_DIR=/etc/eros/model.d` (an empty value counts as unset).

Example split:

```toml
# defaults.toml
[defaults]
fallback_model       = "x-ai/grok-4-mini"
fallback_temperature = 0.5

# chat.toml
[tasks.chat_companion]
model = "provider/chat-model"

# extraction.toml
[tasks.insight_extraction]
model = "provider/extract-model"
[tasks.memory_extraction]
model = "provider/extract-model"
```

## Schema

```toml
[defaults]
fallback_model       = "x-ai/grok-4-mini"   # used when a task has no model and no per-task fallback
fallback_temperature = 0.5
fallback_max_tokens  = 200

[tasks.<name>]
model        = "<provider>/<model-id>"      # required; also accepts an array (round-robin) or table (weighted) — see "Primary model selection"
fallback     = "<provider>/<model-id>"      # optional secondary model
temperature  = 0.85                         # optional, falls back to defaults.fallback_temperature
max_tokens   = 600                          # optional, falls back to defaults.fallback_max_tokens
allow_traits = ["tag_a", "tag_b"]           # optional, prompt-trait allow-list (three-state)
description  = "free-form note"             # optional, documentation only — not consumed by code

[tasks.<name>.tiers.<tier>]
model        = "<provider>/<model-id>"      # optional, overrides task-level model for this tier
fallback     = "<provider>/<model-id>"      # optional, overrides task-level fallback for this tier
allow_traits = ["tag_a"]                    # optional, overrides task-level allow_traits for this tier
```

Field details:

| Field | Type | Required | Notes |
|---|---|---|---|
| `defaults.fallback_model` | `String` | no | Hard fallback if the task config provides no model. If still missing, code uses the compiled-in default `x-ai/grok-4-mini`. |
| `defaults.fallback_temperature` | `f64` | no | Same precedence; compiled-in default `0.5`. |
| `defaults.fallback_max_tokens` | `u32` | no | Same precedence; compiled-in default `200`. |
| `defaults.ignore_providers` | `Array<String>` | no | OpenRouter provider slugs to exclude from routing on **every** task. Each entry must carry the `@openrouter` suffix (`"some-bad-provider-slug@openrouter"`) — a bare slug, any other `@<provider>` suffix, or malformed `@` grammar refuses to boot. The suffix is stripped before the wire: sent as `provider.ignore` (bare slug) on each outbound OpenRouter call only; custom providers and Voyage never receive it. `allow_fallbacks` remains `true` so the model is still served by any healthy provider. Use this when a specific provider returns garbled output for a model (e.g. undecoded byte-BPE text — issue #84). Find the offending slug via the OpenRouter generation API. Empty or absent means no exclusion. |
| `tasks.<name>.model` | `String` \| `Array<String>` \| `Table<String,f64>` | yes | Primary model. String = fixed; array = round-robin; table = weighted random. See "Primary model selection". |
| `tasks.<name>.fallback` | `String` | no | Secondary model used by `OpenRouterClient` if the primary call fails. |
| `tasks.<name>.temperature` | `f64` | no | Per-task sampling temperature. No per-tier override. |
| `tasks.<name>.max_tokens` | `u32` | no | Per-task token cap. No per-tier override. |
| `tasks.<name>.allow_traits` | `Array<String>` | no | Prompt-trait allow-list for this task (three-state: absent = no gating; `[]` = drop all traits; `["a","b"]` = whitelist). Used when no matching tier block is found. |
| `tasks.<name>.tiers.<tier>` | sub-table | no | Per-tier overrides. May set `model`, `fallback`, and/or `allow_traits`. Does not override `temperature` or `max_tokens`. |
| `tasks.chat_companion.input_filter` | `bool` \| `f64` | no | Global trigger for the user-input rewrite filter. Task-level only on `chat_companion` (no per-tier override). `false`/absent = off, `true` = every turn, `0.8` = ~80% of turns (a number outside `[0.0, 1.0]` is rejected). See "`input_filter`". |
| `tasks.<name>.description` | `String` | no | Documentation field, ignored by code. |

### `[providers]` — custom chat/embeddings endpoints (opt-in)

```toml
[providers]
venice = { chat = "https://api.venice.ai/api/v1/chat/completions" }
mixed  = { chat = "https://x/v1/chat/completions", embeddings = "https://x/v1/embeddings" }
local  = { embeddings = "http://127.0.0.1:8080/v1/embeddings" }

[providers.proxy]           # TOML section form works too
chat    = "https://proxy.internal/v1/chat/completions"
headers = { "X-Team" = "companion", "X-Env" = "prod" }
```

Each entry is a table with up to three keys — `chat` (an OpenAI-compatible
chat-completions URL), `embeddings` (an OpenRouter-compatible embeddings
URL — see [`https://openrouter.ai/docs/api_reference/embeddings`](https://openrouter.ai/docs/api_reference/embeddings)
for the wire shape), and `headers` (sent verbatim on every request to this
entry's endpoints). **A plain-string value is rejected**: the pre-0.9.4
shape (`venice = "https://…"`) was dropped with no compatibility layer, and
the boot error names the table form. `deny_unknown_fields` applies — an
unknown key, an empty table, or an empty URL string also refuses the load.

Declares additional endpoints alongside the built-in OpenRouter client.
Reference one by suffixing a model slug anywhere a `model` / `fallback`
accepts one (any shape — fixed, round-robin, weighted) on a chat-shaped
task, or on `[tasks.embedding]`'s model fields for embeddings:

```toml
[tasks.chat_companion]
model = "venice-uncensored@venice"   # served by [providers].venice.chat
fallback = ["x-ai/grok-4.20"]        # no suffix → built-in OpenRouter

[tasks.embedding]
model = "bge-m3@local"               # served by [providers].local.embeddings
```

Rules, all enforced at boot (the engine refuses to boot on any violation):

- **Names** match `[a-z0-9_]+`. `voyage` stays reserved — its native API is
  not the OpenRouter-compatible embeddings format this mechanism speaks, and
  `$VOYAGE_API_KEY` already belongs to the built-in native Voyage client.
  `openrouter` is a valid, declarable name: it doesn't add a separate
  provider, it **overrides the built-in OpenRouter endpoint URLs** per key
  (see below).
- **URLs** are complete and posted verbatim — no path joining. A provider
  referenced from a chat-shaped task must declare `chat`; one referenced
  from `[tasks.embedding]` must declare `embeddings`. A miss on either
  refuses to boot, naming the slug, the entry, and the missing key.
- **`headers`** (optional) is a table sent verbatim on every request to this
  entry's endpoints (both `chat` and `embeddings`). `Authorization` and
  `Content-Type` are engine-owned and refuse the load if declared
  (case-insensitive) — a silently overridden `Authorization` is the worst
  kind of footgun. Every other name/value must be valid HTTP header material
  or the load refuses.
- **API key** comes from the environment as `<NAME_UPPERCASED>_API_KEY`
  (`venice` → `$VENICE_API_KEY`), one key covering both the `chat` and
  `embeddings` endpoints of that entry, required only for providers actually
  referenced by some model slug. Declared-but-unreferenced entries need no
  key. `openrouter` keeps using the existing `$OPENROUTER_API_KEY` — the
  naming convention degenerates to the var that already exists.
- **Model ids are the provider's own slugs**, sent verbatim — the engine
  never translates model names between providers.
- A literal `@` inside a model id is escaped `\@`. In a TOML double-quoted
  string that is written `"weird\\@vendor/m"`; at most one unescaped `@` per
  slug.
- **Model-keyed tables use bare ids**: `model_name_display_override`,
  `output_regex` `models`, and `output_filter` trigger `models` match on the
  id *without* `@provider`.
- **Wire shape**: custom providers receive a strict OpenAI-compatible
  subset. `[defaults].ignore_providers`, `[defaults].provider_sort`, and
  per-task `reasoning` are **inert** on custom providers; they receive
  exactly this entry's own declared `headers`, never the OpenRouter
  attribution headers.
- **Audit**: rows served by a custom provider record
  `model = "<upstream echo>@<name>"` and the provider's own `generation_id`
  verbatim — a `generation_id` join against OpenRouter's logs misses for
  those rows, and the `model` column says why.
- Under `MODEL_CONFIG_DIR`, `[providers]` merges as one whole top-level key
  (like `[defaults]`, unlike `[tasks]`): all providers live in one file.

#### Built-in endpoint overrides via `[providers].openrouter`

```toml
[providers.openrouter]
embeddings = "http://my-proxy/v1/embeddings"
headers    = { "HTTP-Referer" = "https://eros.example", "X-OpenRouter-Title" = "Eros" }
```

- Each present key (`chat` and/or `embeddings`) overrides that built-in URL;
  each absent key keeps the built-in default
  (`https://openrouter.ai/api/v1/chat/completions` /
  `https://openrouter.ai/api/v1/embeddings`). This partial-override rule is
  unique to `openrouter` — for ordinary entries a missing key is a boot
  error when referenced, because there is no built-in default to fall back
  to.
- The override changes the URL **only**. Traffic through it remains the
  full OpenRouter wire: `provider.ignore`, `provider_sort`, and per-task
  `reasoning` are all still sent — unlike custom providers, which keep
  receiving the strict OpenAI subset.
- **Attribution headers now live here, and nowhere else.** No
  `[providers.openrouter]` entry, or one without `headers`, means no
  attribution headers are sent. The `OPENROUTER_APP_REFERER` /
  `OPENROUTER_APP_TITLE` / `OPENROUTER_APP_CATEGORIES` env vars are
  **soft-deprecated**: a still-set value is silently ignored, never a boot
  error — re-declare the same headers under `[providers].openrouter.headers`
  instead (see [`llm-audit.md`](llm-audit.md) for the header/purpose
  mapping).
- The API key stays `$OPENROUTER_API_KEY`.
- **`OPENROUTER_BASE_URL` no longer exists as an env var.** The only
  override mechanism is `[providers].openrouter.chat` /
  `[providers].openrouter.embeddings`. A still-set `OPENROUTER_BASE_URL` is
  not read and causes no boot error — it's just an unrelated env var now.
- `voyage` remains undeclarable in `[providers]` (see above).

#### `[defaults].ignore_providers` — `@openrouter` required

```toml
[defaults]
ignore_providers = ["some-bad-provider-slug@openrouter"]
```

Every entry must parse (the same `@`-suffix grammar as a model slug) to a
non-empty upstream slug plus provider `openrouter`; a bare entry, any other
`@<provider>` suffix, or malformed `@` grammar refuses to boot naming the
entry and the required form. The wire is unchanged: `provider.ignore`
carries the bare upstream slug, on OpenRouter traffic only — the mandatory
suffix makes that scope part of the syntax; custom providers and Voyage
never receive `provider.ignore`. `[defaults].provider_sort` is untouched (it
has no per-entry syntax to scope). Breaking change for configs written
before this suffix existed — the fix is appending `@openrouter` to each
entry.

### `model_name_display_override` (chat task only)

Controls the `model` value sent to clients in chat SSE `meta` frames. Affects
**only** the client display — never the OpenRouter request, the persisted
assistant row, or usage logging. Task-level on `[tasks.chat_companion]`; every
tier inherits it. Setting it on other tasks parses but has no effect.

| Form | Example | Behavior |
|---|---|---|
| `false` *(default when absent)* | `false` | `model` is **omitted** from the frame |
| `true` | `true` | the real model id is sent (pre-0.x behavior) |
| string | `"Aria"` | always sends `"Aria"` |
| array | `["Aria","Nova"]` | random pick per bubble (re-randomizes on replay) |
| map | `{ "deepseek/x" = "Aria", default = "Companion" }` | maps the real id to a name; `default` when unlisted; omit if no `default` |

Because the display name is never persisted, the **array** form re-randomizes on
history replay; `bool`/`string`/`map` are deterministic.

### `output_filter` — second-pass reply rewrite (chat task only)

Passes the completed chat reply through a second LLM before the client sees it. The
filter is **off by default** and has no effect unless explicitly enabled.

#### Turning the filter on

`output_filter` is a boolean flag on `[tasks.chat_companion]`. It acts as a
task-level default, which any tier sub-table may override:

```toml
[tasks.chat_companion]
output_filter = true              # task-level default; applies when no matching tier block exists

[tasks.chat_companion.tiers.gold]
output_filter = true              # per-tier override; takes precedence over the task default
```

Resolution follows the same precedence as every other `chat_companion` field:

```
matched tier block > task default block
```

The compiled-in default when neither sets `output_filter` is `false`.

#### Gating rules

The filter runs for a given turn only when **all** of the following hold:

1. `output_filter` resolves to `true` for the active tier (per the precedence above).
2. `[tasks.chat_output_filter]` is present in the config.
3. The resolved `filter_prompt` for the active tier is non-blank.
4. Any `trigger` predicates that are present all pass (see below).

If any condition is unmet the filter is **inert** — the original reply is delivered unchanged.

#### `[tasks.chat_output_filter]` fields

```toml
[tasks.chat_output_filter]
model        = "openai/gpt-5.4-nano"
fallback     = ["google/gemini-3.1-flash", "zhipuai/zlm-4.7-flash"]
retry_depth  = 1     # fallbacks to try on filter failure (default 1 = primary + first fallback)
temperature  = 0.3
max_tokens   = 400
filter_prompt = """
Rewrite the assistant reply below per <your policy>. Output only the rewrite.
"""
# trigger: AND of the predicates you specify; omit all ⇒ filter every turn.
trigger      = { random = 0.3, models = ["x/y"], traits = { any = ["nsfw_boost"], when = "present" } }
timing       = "after_extract"   # or "before_extract"

[tasks.chat_output_filter.tiers.gold]
filter_prompt = "..."            # any field is optional; falls back to the default block
```

**Recommended models for `chat_output_filter`:**

- **Primary**: `openai/gpt-5.4-nano` — fast, stable filtered output.
- **DO NOT** use `openai/gpt-4.1-nano` as the filter model — empirically returns `"对不起，无法满足你的要求"`-style refusals with HTTP 200, which the engine cannot distinguish from a successful filtered rewrite, so the fail-open path never triggers and the user sees the refusal text.
- **Recommended fallback**: `google/gemini-3.1-flash` — high success rate; when it does fail it surfaces a proper error response (non-200), letting the engine's fail-open path kick in and emit the original reply.
- **Cost-saving fallback**: `zhipuai/zlm-4.7-flash` — cheaper, similar fail-mode profile to gemini-3.1-flash.
- **DO NOT** use `anthropic/claude-haiku-4.5` for the filter — its input tolerance for NSFW (great for extraction) does NOT extend to output; the safety alignment on the output side is strict enough that the filter LLM often refuses to produce rewritten text at all.

| Field | Type | Default | Notes |
|---|---|---|---|
| `model` | `String` \| `Array` \| `Table` | — | Primary filter model. Accepts the same three shapes as `chat_companion.model`. |
| `fallback` | `String` \| `Array<String>` | — | Fallback chain for the filter call. |
| `retry_depth` | `u32` | `1` | Number of `fallback` entries the filter may try before giving up. `0` = primary only; `1` = primary + first fallback (default). |
| `temperature` | `f64` | `defaults.fallback_temperature` | Sampling temperature for the filter model. **Task-level only — no per-tier override** (same as every other task). |
| `max_tokens` | `u32` | `defaults.fallback_max_tokens` | Token cap for the filter response. **Task-level only — no per-tier override.** |
| `filter_prompt` | `String` | — | **Required for the filter to be active.** System/instruction prompt sent to the filter model. Blank or absent → filter is inert. |
| `trigger` | inline table | absent (every turn) | AND-gate on when to apply the filter. Omit the whole key to filter every qualifying turn. |
| `timing` | `"after_extract"` \| `"before_extract"` | `"after_extract"` | Controls whether extract (memory/insight/affinity) reads the original or the filtered text (see below). |

Per-tier sub-tables (`[tasks.chat_output_filter.tiers.<tier>]`) may override
`model`, `fallback`, `retry_depth`, `filter_prompt`, `trigger`, and `timing`; a
tier that omits one falls back to the default `[tasks.chat_output_filter]` block.
**`temperature` and `max_tokens` are task-level only** (per-tier sub-tables do not
override them — the same rule as every other task).

#### `trigger` predicates

`trigger` is an optional inline table. Every predicate you include must pass; predicates you omit are treated as passing. Omit `trigger` entirely to filter every qualifying turn.

| Predicate | Type | Semantics |
|---|---|---|
| `random` | `f64` in `(0.0, 1.0]` | Probability that this turn passes. `random = 0.3` → ~30 % of turns are filtered. |
| `models` | `Array<String>` | Turn passes only if the producing model id is in the list. |
| `traits` | `{ any = [...], when = "present" \| "absent" }` | Turn passes only if at least one tag in `any` is present (`when = "present"`) or absent (`when = "absent"`) among the tags **actually injected** into the prompt — i.e. after tier `allow_traits` gating, the same set reported in the `final` frame's `prompt_injected`. A trait the tier dropped does not count as present. |

#### `timing` and extract behavior

| `timing` | Extract input | Notes |
|---|---|---|
| `"after_extract"` *(default)* | Original (pre-filter) text | Memory/insight/affinity see the unmodified reply; only the rewritten text is delivered to the client and persisted in `chat_messages`. |
| `"before_extract"` | Filtered text | Extract also reads the rewritten text. Use this when the filter normalizes content that the extract pipeline should reflect. |

**Fail-open:** if the filter LLM call times out or returns an error the engine delivers the **original** reply unchanged (the filter never blocks the chat response).

#### What is stored / shown

Only the **filtered** text is written to `chat_messages` and shown to the client. The original text is used internally for extract when `timing = "after_extract"` (default) and is then discarded. History replay therefore shows the filtered version.

#### SSE `final`-frame fields

The `final` event emitted at the end of a chat SSE stream includes several new
fields. These are independent of whether the filter ran — all are always present
when the frame is emitted.

| Field | Type | Notes |
|---|---|---|
| `filtered` | `bool` | `true` if the client received non-raw output this turn — set by the regex strip (`output_regex`), the LLM `output_filter`, or both; `false` otherwise. |
| `retries_chat` | `u32` | Number of fallback retries consumed by the chat model call (0 = primary succeeded). |
| `retries_filter` | `u32` | Number of fallback retries consumed by the filter model call (0 = primary succeeded or filter did not run). |
| `prompt_injected` | `Array<String>` \| `null` | Trait tags that were injected into the prompt this turn, or `null` if none. Independent of the filter. |
| `tier` | `String` \| `null` | Echo of the `tier` field from the request, or `null` if none was sent. Independent of the filter. |

### `output_regex` — deterministic per-model regex strip (chat task only)

`output_regex` is an array of strip rules on `[tasks.chat_companion]` (task-level
only — no per-tier override). Each rule deletes or replaces regex matches in the
assistant reply produced by any model in `models`. It is **off by default** (absent
or empty array means no stripping).

```toml
[tasks.chat_companion]
output_regex = [
  # Strip L3.3-Euryale's self-narrated photo line on reply_text_image turns.
  { models = ["sao10k/l3.3-euryale-70b"],
    pattern = '\s*\[你给对方发送了一张照片[：:][^\]]*\]\s*$' },
  # Replace matches instead of deleting (replacement defaults to "" = delete):
  # { models = ["x/y"], pattern = '...', replacement = "…" },
]
```

#### Rule shape

| Field | Type | Required | Notes |
|---|---|---|---|
| `models` | `Array<String>` | yes | Model ids whose replies this rule applies to. Exact string match against the chat model id that produced the reply — i.e. the row's `model` column, NOT `filter_model` (which is set to `"<regex>"` when a strip fires). |
| `pattern` | `String` | yes | Rust `regex` crate pattern. **No lookaround or backreferences** — anchor with `$`, `^`, `\s*`, char classes. An invalid pattern causes server boot to fail. |
| `replacement` | `String` | no | Text to substitute for each match. Absent or `""` = delete the match. |

Rules are checked in declaration order; all matching rules are applied sequentially
to the same reply.

#### Execution order — layer 0

The regex strip runs **before** any other processing:

1. Regex strip (layer 0) — applied first, before the client sees anything
2. LLM `output_filter` (if enabled) — second pass
3. Memory / insight / affinity extraction — reads the already-stripped text

The matched text therefore reaches **neither** the client **nor** the stored
`content` **nor** the extract pipeline — regardless of `timing` on
`[tasks.chat_output_filter]`.

#### Audit columns

| Column | Value when strip fires |
|---|---|
| `pre_filter_content` | The raw (pre-strip) reply |
| `filter_model` | `"<regex>"` |

These columns are set only when at least one rule actually changes the reply (same
as the LLM filter — a no-op strip leaves them null).

#### Artifact-only reply ⇒ empty bubble

When the reply is **entirely** an artifact (e.g. a bare
`[你给对方发送了一张照片：…]` with nothing else), the strip empties it. There is
**no fail-safe**: the client receives **no content bubble** (no delta is sent),
the row persists empty `content` (`""`), and the audit columns are still set
(`pre_filter_content` = the raw reply, `filter_model` = `"<regex>"`). The
downstream client decides how to render an empty/NULL reply — the reference web
client simply doesn't show it, which reads as a ghost-like non-reply and tends
to make the user follow up (closer to chatting with a real person).

#### `filtered` flag

The SSE `final`-frame `filtered` field is `true` when the client received non-raw
output from **either** the regex strip **or** the LLM `output_filter` (or both).

### `input_filter` — user-input rewrite (chat task only)

`input_filter` is a trigger on `[tasks.chat_companion]` (default `false`,
task-level only — no per-tier override). It accepts a **bool or a probability**:
`false` = off, `true` = every turn (= `1.0`), `0.8` = a per-turn coin flip that
fires on ~80% of turns. A number outside `[0.0, 1.0]` (or non-finite) is rejected
at config-load time. When it fires for a user **Reply** turn, that turn is passed
to a second LLM (`[tasks.chat_input_filter]`) BEFORE generation. The filter
returns a JSON verdict:

- `{"rewrite": false}` — the input is meaningful; the engine uses it verbatim.
- `{"rewrite": true, "content": "…", "reason": "…"}` — the input was meaningless
  (e.g. `1111`, `？？？`, key-mashing); the engine uses `content` instead.

The user's **original** text is always persisted as `content` and shown to the
client. A rewrite is stored in `pre_filter_content` (model-facing only),
`filter_model`, `f_generation_id`, and `filter_triggers = {"reason": …}`. The
model and memory recall see the effective text (`pre_filter_content ?? content`)
for user rows; extraction (insight/memory/affinity) keeps reading the original.

The filter runs only when `input_filter` fires (`true`, or the per-turn draw
passes its probability) AND `[tasks.chat_input_filter]` exists with a non-blank
`filter_prompt`. It is **fail-open**: any error, timeout, unparseable verdict, or
refusal leaves the original input untouched. Pick a fast, cheap model — at
`input_filter = true` it runs on every user turn before generation.

#### `[tasks.chat_input_filter]` fields

Reuses the standard task shape: `model`, `fallback`, `retry_depth` (default 1),
`temperature`, `max_tokens`, `filter_prompt`, `reasoning` (default off in the
example). `trigger`, `timing`, `tiers`, and `allow_traits` are ignored (the
input filter has no triggers, timing, or tiers).

## Task names

| Name | Consumed by | Status |
|---|---|---|
| `chat_companion` | `pipeline::handlers::ReplyHandler` (chat completions; tip turns ride the same reply path) | live |
| `insight_extraction` | `pipeline::post_process::extract_facts` and `extract_structured_insights` (fact mining + JSONB merge) | live |
| `chat_output_filter` | `pipeline::handlers::ReplyHandler` (optional second-pass rewrite of the chat reply before delivery) | live |
| `pde_decision` | `pipeline::stream` (opt-in LLM judge via `run_pde_decision`, called from `run_stream`; rules engine used when `filter_prompt` is absent or the LLM call fails) | live (opt-in) |
| `chat_image_prompt_compose` | `pipeline::stream` (opt-in image-prompt composer; expands the PDE seed subject before image generation; activated when this task block is present) | live (opt-in) |
| `chat_vision` | `pipeline::stream` via `resolve_vision()` (vision pre-stage: describes an `image_url` attachment into JSON before the reply prompt; off when task block absent or `filter_prompt` blank) | live (opt-in) |
| `chat_product_qa` | `pipeline::stream` via `resolve_product_qa()` (out-of-character product-QA executor for the PDE `product_qa` action; off when task block absent or `filter_prompt` blank; also requires the LLM PDE) | live (opt-in) |
| `affinity_evaluation` | `pipeline::post_process` (per-turn 6-axis affinity delta; runs after each Reply turn, fire-and-forget) | live |
| `memory_extraction` | dreaming sweeper (session-end memory consolidation; off when task block absent) | live (opt-in) |
| `chat_input_filter` | `pipeline::stream` (user-input rewrite filter; activated by `input_filter` on `[tasks.chat_companion]` and this task block; off by default) | live (opt-in) |
| `embedding` | `EmbeddingRouter::from_config()` at boot (`main.rs`), via `ModelConfig::resolve_embedding()` — routes `embed_query`/`embed_document`/`embed_documents` to native Voyage, the built-in OpenRouter embeddings endpoint, or a custom `[providers]` entry; absent block = native Voyage `voyage-4-lite` | live |

A `[tasks.<name>]` entry is only meaningful if the engine actually calls `model_config.resolve("<name>", ...)` somewhere. The current call sites are:

- `crates/eros-engine-server/src/pipeline/handlers.rs` → `chat_companion`, `chat_output_filter`
- `crates/eros-engine-server/src/pipeline/post_process.rs` → `insight_extraction`, `affinity_evaluation`
- `crates/eros-engine-server/src/pipeline/stream.rs` → `pde_decision` via `run_pde_decision` inside `run_stream` (only when `filter_prompt` is set); `chat_image_prompt_compose` via `resolve_image_prompt_compose()` (image-prompt composer, opt-in, resolved lazily only on image turns); `chat_vision` via `resolve_vision()` (vision pre-stage, opt-in); `chat_product_qa` via `resolve_product_qa()` (product-QA executor, opt-in); `chat_input_filter` via `resolve_input_filter()` (input rewrite, opt-in); `memory_extraction` via the dreaming sweeper

`embedding` doesn't go through the generic `resolve()` path above — it has
its own resolver, `ModelConfig::resolve_embedding()`, called once at boot
from `main.rs` to build the `EmbeddingRouter`. See "`[tasks.embedding]` —
active" below.

### `[tasks.pde_decision]` — opt-in LLM PDE judge

By default the engine uses the built-in rule engine (`eros-engine-core/src/pde.rs`) to decide the per-turn action (reply / ghost / proactive). Setting `filter_prompt` in this block switches on an LLM judge:

- The LLM receives the recent conversation, relationship state, and conversation signals, and returns a JSON verdict with:
  - `action`: `"reply_text"` | `"ghost"` | `"reply_image"` | `"reply_text_image"` | `"product_qa"` (image variants are available when the request includes an `image` block — the consumer signalling it handles images this turn; otherwise they degrade to `reply_text`. The chat stream never draws — it emits an `image_request` frame and the consumer calls its own image vendor. `product_qa` is available only when `[tasks.chat_product_qa]` is fully enabled — see below; unavailable proposals degrade to `reply_text`, never upgrade.)
  - `inner_state`: a short mood/tone description folded into the reply prompt
  - `tone` (optional): a short delivery directive for this turn's reply — injected into the reply prompt as a `[reply_tone]` section on text-bearing actions; omitted when absent
  - `image_prompt`, `reason`: optional
- **Fail-open:** any LLM timeout or error falls back to the rule engine — the LLM judge never blocks a chat response.
- **Hard-safety guardrails** (enforced after the LLM verdict, before the rule-engine fallback): never ghost in the first 10 messages, never ghost twice in a row, one-hour ghost cooldown.
- Every judge call is audited to `companion_decision_events`.

**Image-availability context line.** The judge context always carries exactly one line — `[图片能力] 本轮可发图=是` when an image action is available this turn (the request carries an `image` block), or `[图片能力] 本轮可发图=否` otherwise. Prompt authors should treat `本轮可发图=否` as a hard constraint (never choose `reply_image` / `reply_text_image` — they would be degraded by `guard_action` anyway, wasting tokens and skewing audits), and `本轮可发图=是` as the gate that *permits* image actions, then decide by persona/context (the engine does not force an image just because one is possible). Keep the token string `[图片能力] 本轮可发图=是/否` verbatim if a downstream overlay references it.

**`ghosting` field** (bool, default `true`): a safety switch for downstream products. Set `ghosting = false` to disable ghosting across the _entire_ PDE path — LLM verdict, rule fallback, and the pure rule engine — so the companion never goes silent. Useful for products where silent turns are undesirable.

### `[tasks.chat_image_prompt_compose]` — image-prompt composer (opt-in)

The PDE writes a terse seed `image_prompt` while also choosing the action and
`inner_state` on a tight token budget. When this task block is present, the
engine runs a dedicated composer **after** an image action is decided and
**before** generation: it sends the model the persona appearance, the recent
scene, the PDE seed subject, the chosen style, and the target aspect ratio, and
uses the expanded result as the image subject (carried in the `image_request`
frame's `composed_prompt`; the persisted `metadata.image.prompt` marker keeps
the PDE seed subject). The PDE's original seed is preserved separately in the
decision audit.

The feature is **off by default** and activates only when this block exists.
It is **fail-open**: on composer failure / timeout / empty output the engine
falls back to the PDE seed unchanged, so it never blocks or fails the image
turn. The task is resolved **lazily, only on image turns**, so it never advances
a `model` round-robin cursor on text/ghost turns.

```toml
[tasks.chat_image_prompt_compose]
model        = "x-ai/grok-4"                       # any text model; pick one comfortable with your content range
fallback     = ["google/gemini-3.1-flash-lite"]
retry_depth  = 1
temperature  = 0.7
max_tokens   = 700
# reasoning  = { enabled = false }
# filter_prompt is OPTIONAL — omit to use the built-in default (below):
# filter_prompt = """…override…"""
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `model` | `ModelSpec` (string \| array \| table) | absent | Composer model (same three shapes as `chat_companion.model`). |
| `fallback` | `String` \| `Array<String>` | `[]` | Sequential retry chain (FallbackSpec), truncated to `retry_depth`. |
| `retry_depth` | `u32` | `1` | Primary + this many fallbacks. |
| `temperature` | `f64` | task default | Sampling temperature for the composer call. |
| `max_tokens` | `u32` | task default | Token cap for the composer call. |
| `reasoning` | table | absent | Optional reasoning control forwarded to OpenRouter. |
| `filter_prompt` | `String` \| `Array<String>` \| `Table<String, String>` | **built-in default** | **Optional** (unlike other tasks). Blank/absent ⇒ the engine's built-in `DEFAULT_COMPOSE_PROMPT`. See **variants** below for the array/table shapes. |

**Built-in default prompt.** Unlike every other task (whose prompt must come from
config), the composer ships a generic, **product-identity-free** built-in
default. Its stance is **expand-only**: it faithfully expands the seed subject and
appearance into a detailed prompt and does **not** soften, downgrade, censor, or
refuse, and adds no content-moderation layer of its own — moderation is delegated
to the image-model provider and the downstream deployment, not this step. A
non-blank `filter_prompt` overrides it; a blank/absent one falls back to it.

**Variants.** This is the **only** task whose `filter_prompt` accepts more than a
plain string. The consumer picks one per chat turn via `image.prompt_variant` on
the send-message body. Three shapes:

```toml
filter_prompt = "…"                       # one prompt; prompt_variant is ignored
filter_prompt = ["…", "…"]                # pick by index: prompt_variant = "0" / "1"
filter_prompt = { a = "…", b = "…" }      # pick by key:   prompt_variant = "a" / "b"
```

Composer prompts tend to be long, multi-paragraph strings, and TOML 1.0.0
discourages breaking an inline table across lines — so the keyed form also
works as a standard (non-inline) table, one key per section:

```toml
[tasks.chat_image_prompt_compose.filter_prompt]
a = """
…long prompt…
"""
b = """
…another long prompt…
"""
```

Anything not selected falls back to the **built-in** prompt above, never to
"first entry wins" — but the two ways of "not selected" are logged
differently. An **absent** `prompt_variant` falls back silently (the common
case: no variant was requested, so there is nothing to warn about). An
**explicitly-supplied** variant that fails to match — an out-of-range index, an
unknown key — also falls back, but additionally logs at `warn`, since a
variant the caller asked for and didn't get is worth surfacing. There is no
reserved `default` key: writing `default = "…"` defines an ordinary variant,
selected only by a literal `prompt_variant = "default"`.

`prompt_variant = "raw"` (case-insensitive) skips the composer LLM entirely and
draws the seed subject as-is — one fewer LLM call for that turn. `raw` is
reserved: a table key literally named `raw` (any casing) refuses to boot.

Variants are honored on this task only. An array/table `filter_prompt` on any
other task, or inside **any** `[tasks.*.tiers.*]` block (this task's own tiers
included — the composer always resolves with no tier), refuses to boot rather
than sit there unreachable.

Call site: `crates/eros-engine-server/src/pipeline/stream.rs` via
`resolve_image_prompt_compose()` in `model_config.rs`.

**Audit.** A successful composer call writes three keys into the image
turn's `chat_messages.metadata.image`: `compose_variant` (which
`filter_prompt` key/index was selected; absent for a plain/built-in
prompt; recorded as supplied (trimmed), not normalized — e.g. for the
indexed shape, `"01"` selects index 1 and is audited as `"01"`),
`compose_model` (the model that answered), and
`compose_generation_id`. All three are absent when no compose succeeded
(`raw`, fail-open, task not configured) — same NULL semantics as the
affinity audit trio. Usage/cost is not persisted; reconcile via the
generation id against your provider's logs. The composed prompt is not
persisted (consumer-side by design). Spec:
`docs/superpowers/specs/2026-08-02-image-compose-audit-design.md`.

### `[tasks.chat_vision]` — image input (vision pre-stage, opt-in)

When a chat turn carries an `image_url`, the engine runs `resolve_vision()` to
obtain a vision-capable model and `filter_prompt`, calls that model to describe
the image into a fixed JSON schema (`description`, `ocr_text`, `people`, `scene`),
and folds the result into the user-facing prompt before the main chat call. The
main `chat_companion` model stays text-only.

The feature is **off by default** and activates only when this task block exists
with a non-blank `filter_prompt`. `retry_depth` defaults to `1` (primary +
first fallback). Pick a vision-capable model; the example uses
`google/gemini-3.1-flash-lite`.

Call site: `crates/eros-engine-server/src/pipeline/stream.rs` via
`resolve_vision()` in `model_config.rs`.

### `[tasks.chat_product_qa]` — out-of-character product answers (opt-in)

Powers the PDE judge's `product_qa` action: when the end user asks about the
downstream product itself ("这个 app 是什么？", "怎么收费？", "会员怎么取消？"),
the judge routes the turn to this task's own model chain instead of
`chat_companion`. `filter_prompt` (the product documentation + answering
rules) becomes the executor's **entire** system prompt — no persona is
folded in, the companion steps fully out of character for the turn.

**Three enablement gates** — all must hold (`resolve_product_qa()` /
`validate_product_qa_prompt()`):

| Gate | State | Behaviour |
| --- | --- | --- |
| `[tasks.pde_decision].filter_prompt` set | off | The rule engine never emits `product_qa` — the action is unreachable without the LLM judge. If `[tasks.chat_product_qa]` is configured anyway, boot logs one WARN (`"model_config: [tasks.chat_product_qa] is configured but the LLM PDE ([tasks.pde_decision].filter_prompt) is disabled — product_qa is inert"`) and the block stays inert. |
| `[tasks.chat_product_qa]` block present | absent | Feature off. The judge context carries no product-QA lines; a hallucinated `product_qa` verdict degrades to `reply_text`. |
| `chat_product_qa.filter_prompt` non-blank | blank | **Refuses to boot** — the same required-prompt contract as `insight_extraction` / `memory_extraction`. Set a prompt, or remove the `[tasks.chat_product_qa]` section entirely to disable the feature. |

```toml
[tasks.chat_product_qa]
model        = "anthropic/claude-haiku-4.5"
fallback     = ["google/gemini-3.1-flash-lite"]
retry_depth  = 1
temperature  = 0.3
max_tokens   = 800
reasoning    = { enabled = false }
filter_prompt = """
你是 XX 产品的官方说明助手。以下是产品资料：
…（产品定位、功能、价格、会员、退订方式等）…
只根据资料作答；资料没有的信息明确说不知道，不编造。语气友好简洁，不扮演角色。
"""
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `model` | `ModelSpec` (string \| array \| table) | — | Primary executor model (same three shapes as `chat_companion.model`). |
| `fallback` | `String` \| `Array<String>` | `[]` | Sequential retry chain (FallbackSpec), truncated to `retry_depth`. |
| `retry_depth` | `u32` | `1` | Primary + this many fallbacks. |
| `temperature` | `f64` | task default | Sampling temperature for the executor call. |
| `max_tokens` | `u32` | task default | Token cap for the executor call. |
| `reasoning` | table | absent | Optional reasoning control forwarded to OpenRouter. |
| `filter_prompt` | `String` | — | **Required.** Product docs + answering rules; blank or absent refuses to boot (see the gates table above). |

**Judge-side context.** Only when all three gates pass, the judge context
(`build_pde_ctx`) gains two lines the prompt can act on:

- `[产品咨询] 本轮可答产品问题=是` — availability line, rendered only when the
  task is enabled (feature-off deployments see zero prompt drift, zero token
  cost).
- `[最近产品咨询]` — the session's most recent **3** product-QA pairs
  (`channel='product_qa'`), so elliptical follow-ups ("那多少钱一个月？")
  still route to `product_qa`. Omitted when there are none. The same 3 pairs
  are reused as the executor's own conversational context — no second store
  fetch.

**Isolation semantics.** The answer persists as a normal assistant row
(`role='assistant'`, `assistant_action_type='reply'`) marked
`channel='product_qa'`. That marker makes the row invisible to the companion
brain: short-term recall (`recent_turn_pairs` / `recent_turn_pairs_before_message`
/ `recent_assistant_contents`), conversation signals
(`compute_signals_for_session`), the dreaming sweeper's session-log pull, the
judge's own shared companion transcript (`build_input_filter_transcript`),
and the companion's live message window (`assemble_chat_request`) all filter
`channel IS NOT NULL` rows out — while the row stays fully visible on the
live SSE stream, disconnect-replay, and client history (`channel` is exposed
on both history projections). See [architecture.md](architecture.md) for the
full `chat_messages.channel` semantics.

**Failure fallback.** If the executor's whole candidate chain (`model` +
`fallback`) is exhausted without streaming any content, the engine does
**not** fall back to the in-character companion reply — the companion
doesn't know the product facts, and improvising is exactly what this feature
exists to prevent. It instead picks a configured `error_handling` fallback
phrase (the same DB-backed mechanism the rest of the chat stream uses) and
persists it **with the `channel='product_qa'` marker**, so replay and
idempotency still hold; with no fallback phrase configured, the turn ends in
an `Error` frame instead.

Call site: `crates/eros-engine-server/src/pipeline/stream.rs` via
`resolve_product_qa()` in `model_config.rs`.

### `[tasks.embedding]` — active

`VoyageClient` used to hard-code `voyage-3-lite`; `[tasks.embedding]` is now
consumed, and the embedding model, and which backend it routes to, are
config-driven.

```toml
# Single model — read and write use the same backend.
[tasks.embedding]
model = "voyage-4-lite"                       # ≡ "voyage-4-lite@voyage"
# model = "voyage-3-lite"                     # legacy pin (no longer recommended by Voyage)
# model = "openai/text-embedding-3-small@openrouter"
# model = "bge-m3@local"                      # third-party, OpenRouter-compatible wire

# OR: split read/write — voyage-4 series and above ONLY.
#[tasks.embedding]
#model_read  = "voyage-4-lite"   # recall path: embed_query, input_type "query"
#model_write = "voyage-4"        # storage path: embed_document(s), input_type "document"
```

**Dimensions are fixed at 512, with no config knob.** The three pgvector
columns are `VECTOR(512) NOT NULL`, the clients request 512 on the wire, and
every response is length-checked. There used to be a `dimensions` field on
`[tasks.<name>]` — it was never consumed and has been removed; a leftover
`dimensions = 512` line in an existing config is now an inert unknown key
(serde ignores it, exactly like any other stale key).

| field | type | rules |
|---|---|---|
| `model` | single fixed string | bare ⇒ `@voyage`; `@openrouter` / `@<custom>` route to the OpenRouter-compatible wire; mutually exclusive with the pair |
| `model_read` | `Option<String>` | pair-only, voyage-only, N ≥ 4 (see the gate below); serves `embed_query` |
| `model_write` | `Option<String>` | pair-only, voyage-only, N ≥ 4; serves `embed_document` / `embed_documents` |

Routing per suffix, resolved by `ModelConfig::resolve_embedding()`:

| bare (no suffix) | `@openrouter` | `@voyage` | `@<custom>` |
|---|---|---|---|
| native Voyage | built-in OpenRouter embeddings endpoint (overridable, see `[providers].openrouter` above) | same as bare | `[providers].<name>.embeddings` (OpenRouter-compatible wire) |

- `model_read` / `model_write` are plain `Option<String>` — array/table
  shapes are type errors at parse time. `model_read = model_write` is legal
  (redundant but harmless — equivalent to `model`). `@openrouter` and
  `@<custom>` refuse to boot on `model_read`/`model_write` (only Voyage
  guarantees a shared vector space across model sizes).
- `[tasks.embedding]` **absent** ⇒ native Voyage with `voyage-4-lite`
  (`output_dimension: 512` on the wire). Voyage no longer recommends
  `voyage-3-lite`; a deployment that still needs it must pin it explicitly.
  Switching models over existing data changes the vector space — old rows
  are not comparable to new queries — so either pin the old model or
  re-embed.
- `model` must be a single fixed string; round-robin, weighted, `fallback`,
  and `tiers` all refuse to boot (mixed/incompatible vector spaces would be
  the result — the `chat_voice` fixed-only precedent).
- The wire has no `input_type`: the query/document optimisation is a
  Voyage-native nuance. Routing embedding off Voyage forfeits it.

**The voyage-4 gate.** Applied to the bare id after stripping an optional
`@voyage` suffix. The id must begin `voyage-` followed by a numeric segment
(ASCII digits and dots only, ending at the next `-` or end of string) that
parses as a finite number ≥ 4:

- ✓ `voyage-4`, `voyage-4-lite`, `voyage-4.5-large`, `voyage-10`
- ✗ `voyage-3.5-lite` (N = 3.5), `voyage-code-3` (no leading numeric segment
  after `voyage-`), `voyage-inf`/`voyage-nan` (non-digit characters, and
  non-finite even if they parsed), `bge-m3@local` (not voyage), any
  `@openrouter` or custom-provider slug

Only the voyage-4 series and above guarantee a shared vector space across
model sizes — mixing a lower or non-numeric model into the pair would
silently write vectors the read model cannot compare, so the gate is a boot
refusal, not a docs footnote.

**`VOYAGE_API_KEY`** is required iff the resolved read or write backend is
Voyage (block absent ⇒ Voyage default ⇒ still required, so existing
deployments see no change). A deployment that routes both read and write
entirely off Voyage no longer needs the var.

Call site: `crates/eros-engine-server/src/main.rs` builds
`eros_engine_llm::embedding::EmbeddingRouter::from_config(&model_config)`
once at boot; `AppState.embed: Arc<EmbeddingRouter>` serves `embed_query` /
`embed_document` / `embed_documents` from `handlers.rs` / `post_process.rs`
/ `dreaming.rs` / `world.rs` / `story.rs`, unchanged call shapes.

### Enabling / disabling extraction

`insight_extraction` (per-turn fact mining) and `memory_extraction` (session-end
dreaming sweeper) are controlled by the **presence of their `[tasks.*_extraction]`
section**:

- **Section present** → `filter_prompt` is **required**; the server refuses to boot
  if it is blank or absent.
- **Section absent** → that extraction is **off**. The engine boots and runs without
  it (`insight_extraction` is skipped per turn; the dreaming sweeper stays inert).

> **Behavior change (0.6.x):** earlier releases made both sections mandatory (an
> absent section boot-failed). They are now optional-by-omission. The shipped
> `examples/model_config.toml` keeps both sections, so the default — both
> extractions on — is unchanged.

`reasoning` works the same as on every task — omit it to let the model decide,
`reasoning = { enabled = false }` to force reasoning off, `{ enabled = true }` to
force it on.

## Resolution rules

For `model` and `fallback`:

```
matched tier block > task default block > [defaults] > compiled-in fallback
```

For `allow_traits`:

```
matched tier block > task default block
```

For `temperature` and `max_tokens`:

```
task default block > [defaults] > compiled-in fallback
```

Where each step contributes:

- **Matched tier block** — `[tasks.<name>.tiers.<tier>]`, where `<tier>` comes from the `tier` field of the chat request (regex `^[a-z0-9_]{1,32}$`). If the requested tier is absent or unknown (no matching sub-table), the task default block is used and a `tracing::warn!` is emitted.
- **Task default block** — `[tasks.<name>]`.
- **`[defaults]`** — top-level defaults block.
- **Compiled-in fallback** — `x-ai/grok-4-mini`, temperature `0.5`, max_tokens `200`. Hard-coded in `model_config.rs`.

`temperature` and `max_tokens` are task-level only — per-tier sub-tables do not override them.

If `resolve()` is called with an unknown task name, it falls back through `defaults → compiled-in` and emits a `tracing::warn!` ("model_config: unknown task, using defaults").

## Primary model selection

`model` (task-level and per-tier) accepts three shapes:

```toml
model = "x-ai/grok-4.20"                              # fixed
model = ["x-ai/grok-4.20", "z-ai/glm-4.7-flash"]     # round-robin (deterministic)
model = { "x-ai/grok-4.20" = 0.8, "z-ai/glm-4.7-flash" = 0.2 }  # weighted random
```

- **Round-robin** alternates deterministically across calls (per-process counter; resets on restart; each replica counts independently).
- **Weighted** draws randomly; weights are any positive numbers, normalized by their sum (`{a = 8, b = 2}` == `{a = 0.8, b = 0.2}`). Non-positive weights are dropped.
- `["a","b"]` and `{a = 1, b = 1}` produce the same long-run distribution but differ in mechanism (deterministic vs. random).
- A single-entry array/table behaves like a fixed string. An empty array/table falls through to the next precedence level.

**TOML gotcha:** inline-table keys allow only `A-Za-z0-9_-`, but model ids contain `/` and `.`, so weighted keys **must be quoted**: `{ "x-ai/grok-4.20" = 0.8 }`. The array form needs no quoting.

### Fallback dedup

After the primary is selected, any occurrence of that exact id is removed from the resolved `fallback` chain — retrying a model that just failed is wasted. With round-robin/weighted primaries this is dynamic: only the id chosen for that call is dropped.

## Stability commitments (OSS 0.x)

For the duration of `0.x`, the OSS engine commits to:

1. **No removed fields.** Existing field names in `[defaults]` and `[tasks.<name>]` will not disappear.
2. **No renamed fields.** `fallback` will not become `fallback_model`. `model` will not become `primary_model`. Etc.
3. **No newly required fields.** Anything added is optional with a sensible default.
4. **No removed task names from this list:** `chat_companion`, `insight_extraction`, `pde_decision`, `embedding`.
5. **Resolution precedence is fixed.** `matched tier > task default block > [defaults] > compiled-in fallback` for `model`/`fallback`/`allow_traits`. `temperature`/`max_tokens` are task-level only.
6. **`model` accepts a string, array (round-robin), or table (weighted).** A plain string remains valid forever; the array/table forms are an additive widening.

What may still change without notice:

- Compiled-in fallback values (currently `x-ai/grok-4-mini` / `0.5` / `200`). These are fail-safes, not contract.
- Internal struct shapes inside `eros-engine-llm` if `#[non_exhaustive]` is added.
- The `description` field's handling — it's documentation today, may become structured metadata later.
- *Future* new optional fields and new task names beyond those documented here. (The fields documented above — including `allow_traits` and `tiers` — are covered by commitments 1–3.)

### Changelog note

- **`persona_override` (`art_metadata.model`) is no longer read by the engine as of this version.** Use `[tasks.<name>.tiers.<tier>]` for per-tier model selection instead. The `model` field may still exist in a persona's JSONB `art_metadata` but is silently ignored.
- `model_name_display_override` (optional, `[tasks.chat_companion]`): added in
  0.x. When unset the chat `meta.model` field is **omitted** — a change from the
  earlier "always present" behavior. The shipped example sets `= true` to keep
  showing the real id.
- `output_filter` (optional bool, `[tasks.chat_companion]` and per-tier): added in
  0.x. Default `false`. Enables the second-pass reply rewrite via `[tasks.chat_output_filter]`.
- `[tasks.chat_output_filter]` (new task): added in 0.x. Absent by default (filter
  is inert). See "output_filter — second-pass reply rewrite" above.
- SSE `final`-frame fields `filtered`, `retries_chat`, `retries_filter`,
  `prompt_injected`, `tier`: added in 0.x.
- `output_regex` (optional array, `[tasks.chat_companion]`): added in 0.x.
  Task-level only (no per-tier override). Deterministic regex strips applied
  before the client sees the reply, before the LLM `output_filter`, and before
  extract. The `filtered` flag is `true` when either the regex strip or the LLM
  filter (or both) produced non-raw output. See "`output_regex` — deterministic
  per-model regex strip" above.
- **`[tasks.embedding]` is now active** (previously reserved and unconsumed).
  Breaking changes bundled with the activation: `[providers]` values are now
  tables, not plain strings (the string shape is rejected with no compat
  layer); `[defaults].ignore_providers` entries now require the
  `@openrouter` suffix; the `OPENROUTER_BASE_URL` env var is gone (use
  `[providers].openrouter.chat`/`.embeddings`); the `OPENROUTER_APP_*` env
  vars are soft-deprecated (silently ignored, use
  `[providers].openrouter.headers`); the `dimensions` field on
  `[tasks.<name>]` was removed (dims are hard-coded 512; a leftover
  `dimensions = 512` line is now an inert unknown key). See "`[providers]`"
  and "`[tasks.embedding]` — active" above.

## What this config does NOT control

- **Voyage's own base URL** — the native Voyage wire always posts to Voyage's canonical endpoint; only the model id is configurable, via `[tasks.embedding]`. Route around Voyage entirely with an `@openrouter` or custom `[providers]` suffix instead.
- **PDE decisions (default path)** — the rule engine in `eros-engine-core/src/pde.rs` runs unconditionally when no `filter_prompt` is set. Set `[tasks.pde_decision].filter_prompt` to activate the opt-in LLM judge; the rule engine then serves as fallback + hard-safety guardrails.
- **OpenRouter API key** — read directly from `OPENROUTER_API_KEY`, not the config file.
- **Per-call streaming / response format flags** — fixed in `OpenRouterClient`.

## Worked example: tier-based resolution

```toml
[tasks.chat_companion]
model        = "x-ai/grok-4.20"
fallback     = ["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3", "qwen/qwen3.6-flash"]
temperature  = 0.8
max_tokens   = 1200
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.free]
model        = "qwen/qwen3.6-flash"
fallback     = ["deepseek/deepseek-v4-flash"]
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.gold]
model        = "x-ai/grok-4.20"
fallback     = ["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3"]
allow_traits = ["allow_nsfw", "allow_politics"]
```

When a request arrives with `"tier": "gold"`, `resolve("chat_companion", "gold")` returns:

| Field | Value | Source |
|---|---|---|
| `model` | `x-ai/grok-4.20` | `tiers.gold` |
| `fallback` | `["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3"]` | `tiers.gold` |
| `allow_traits` | `["allow_nsfw", "allow_politics"]` | `tiers.gold` |
| `temperature` | `0.8` | task default block (no tier override) |
| `max_tokens` | `1200` | task default block (no tier override) |

When a request arrives with `"tier": "free"`:

| Field | Value | Source |
|---|---|---|
| `model` | `qwen/qwen3.6-flash` | `tiers.free` |
| `fallback` | `["deepseek/deepseek-v4-flash"]` | `tiers.free` |
| `allow_traits` | `["allow_politics"]` | `tiers.free` |
| `temperature` | `0.8` | task default block |
| `max_tokens` | `1200` | task default block |

When no `tier` is sent (or an unknown tier is sent), all fields resolve from the task default block.

## Compatibility test fixture

`model_config.rs` includes a fixture that asserts every field of a representative TOML round-trips correctly. Any breaking schema change will fail CI before it ships. See `compat_fixture_locks_full_schema` in `crates/eros-engine-llm/src/model_config.rs`.
