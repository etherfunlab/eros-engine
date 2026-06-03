# Model config

[English](model-config.md) · [中文](model-config.zh.md)

LLM model selection for the engine lives in a TOML file loaded at server start. Per-task model + parameters, with optional per-tier overrides on top.

## Where it lives

- Default path: `examples/model_config.toml` (relative to the working directory). The file under `examples/` is an illustrative template — adapt it to your own needs (or point `MODEL_CONFIG_PATH` at your own file).
- Override: `MODEL_CONFIG_PATH` environment variable
- Loaded once at server start by `eros-engine-server/src/main.rs` (reads `MODEL_CONFIG_PATH` directly, then `ModelConfig::from_toml_str`). For library embedders, `ModelConfig::load()` in `crates/eros-engine-llm/src/model_config.rs` does the same with the same default path (`examples/model_config.toml`).
- Held as `Arc<ModelConfig>` in `AppState`; shared across all chat / post-process turns
- The server also calls `dotenvy::dotenv()` at startup, so `cp .env.example .env` works for the quickstart without an explicit `source .env`

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
dimensions   = 512                          # optional, embedding-only field

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
| `tasks.<name>.model` | `String` \| `Array<String>` \| `Table<String,f64>` | yes | Primary model. String = fixed; array = round-robin; table = weighted random. See "Primary model selection". |
| `tasks.<name>.fallback` | `String` | no | Secondary model used by `OpenRouterClient` if the primary call fails. |
| `tasks.<name>.temperature` | `f64` | no | Per-task sampling temperature. No per-tier override. |
| `tasks.<name>.max_tokens` | `u32` | no | Per-task token cap. No per-tier override. |
| `tasks.<name>.allow_traits` | `Array<String>` | no | Prompt-trait allow-list for this task (three-state: absent = no gating; `[]` = drop all traits; `["a","b"]` = whitelist). Used when no matching tier block is found. |
| `tasks.<name>.tiers.<tier>` | sub-table | no | Per-tier overrides. May set `model`, `fallback`, and/or `allow_traits`. Does not override `temperature` or `max_tokens`. |
| `tasks.chat_companion.input_filter` | `bool` \| `f64` | no | Global trigger for the user-input rewrite filter. Task-level only on `chat_companion` (no per-tier override). `false`/absent = off, `true` = every turn, `0.8` = ~80% of turns (a number outside `[0.0, 1.0]` is rejected). See "`input_filter`". |
| `tasks.<name>.description` | `String` | no | Documentation field, ignored by code. |
| `tasks.<name>.dimensions` | `u32` | no | Embedding-only. Ignored by chat / insight tasks. |

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
| `filtered` | `bool` | `true` if the output filter ran and rewrote the reply for this turn; `false` otherwise. |
| `retries_chat` | `u32` | Number of fallback retries consumed by the chat model call (0 = primary succeeded). |
| `retries_filter` | `u32` | Number of fallback retries consumed by the filter model call (0 = primary succeeded or filter did not run). |
| `prompt_injected` | `Array<String>` \| `null` | Trait tags that were injected into the prompt this turn, or `null` if none. Independent of the filter. |
| `tier` | `String` \| `null` | Echo of the `tier` field from the request, or `null` if none was sent. Independent of the filter. |

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
| `pde_decision` | reserved — current PDE is rule-based and does not call an LLM | reserved |
| `embedding` | reserved — `VoyageClient` reads its own `VOYAGE_API_KEY` and hard-codes `voyage-3-lite` | reserved |

A `[tasks.<name>]` entry is only meaningful if the engine actually calls `model_config.resolve("<name>", ...)` somewhere. The current call sites are:

- `crates/eros-engine-server/src/pipeline/handlers.rs` → `chat_companion`, `chat_output_filter`
- `crates/eros-engine-server/src/pipeline/post_process.rs` → `insight_extraction`

Anything else is either reserved for a future feature (`pde_decision`) or vestigial (`embedding` — Voyage doesn't go through this path).

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
4. **No removed task names from this list:** `chat_companion`, `insight_extraction`. Reserved task names (`pde_decision`, `embedding`) may shift if a real implementation lands and supersedes their current placeholder semantics; that change will be called out in the changelog.
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

## What this config does NOT control

- **Voyage embedding** — `VoyageClient` hard-codes `voyage-3-lite` and reads `VOYAGE_API_KEY` directly. The `[tasks.embedding]` block is reserved for a future generalisation.
- **PDE decisions** — current PDE is pure rule-based logic in `eros-engine-core/src/pde.rs`. The `[tasks.pde_decision]` block is reserved for an optional LLM decision layer that has not landed yet.
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
