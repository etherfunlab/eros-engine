# Model config

[English](model-config.md) · [中文](model-config.zh.md)

LLM model selection for the engine lives in a TOML file loaded at server start. Per-task model + parameters, with optional per-tier overrides on top.

## Where it lives

- Default path: `examples/model_config.toml` (relative to the working directory). The committed file is a sanitized template; copy it (or point `MODEL_CONFIG_PATH` at your own) for real deployments.
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

## Task names

| Name | Consumed by | Status |
|---|---|---|
| `chat_companion` | `pipeline::handlers::ReplyHandler` and `GiftHandler` (chat completions) | live |
| `insight_extraction` | `pipeline::post_process::extract_facts` and `extract_structured_insights` (fact mining + JSONB merge) | live |
| `pde_decision` | reserved — current PDE is rule-based and does not call an LLM | reserved |
| `embedding` | reserved — `VoyageClient` reads its own `VOYAGE_API_KEY` and hard-codes `voyage-3-lite` | reserved |

A `[tasks.<name>]` entry is only meaningful if the engine actually calls `model_config.resolve("<name>", ...)` somewhere. The current call sites are:

- `crates/eros-engine-server/src/pipeline/handlers.rs` → `chat_companion`
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
