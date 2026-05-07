# Model config

[English](model-config.md) · [中文](model-config.zh.md)

LLM model selection for the engine lives in a TOML file loaded at server start. Per-task model + parameters, with persona-level overrides on top.

## Where it lives

- Default path: `examples/model_config.toml` (relative to the working directory)
- Override: `MODEL_CONFIG_PATH` environment variable
- Loaded once at server start by `eros-engine-server/src/main.rs` (reads `MODEL_CONFIG_PATH` directly, then `ModelConfig::from_toml_str`). For library embedders, `ModelConfig::load()` in `crates/eros-engine-llm/src/model_config.rs` does the same with the same default path.
- Held as `Arc<ModelConfig>` in `AppState`; shared across all chat / post-process turns
- The server also calls `dotenvy::dotenv()` at startup, so `cp .env.example .env` works for the quickstart without an explicit `source .env`

## Schema

```toml
[defaults]
fallback_model       = "x-ai/grok-4-mini"   # used when a task has no model and no per-task fallback
fallback_temperature = 0.5
fallback_max_tokens  = 200

[tasks.<name>]
model        = "<provider>/<model-id>"      # required
fallback     = "<provider>/<model-id>"      # optional secondary model
temperature  = 0.85                         # optional, falls back to defaults.fallback_temperature
max_tokens   = 600                          # optional, falls back to defaults.fallback_max_tokens
description  = "free-form note"             # optional, documentation only — not consumed by code
dimensions   = 512                          # optional, embedding-only field
```

Field details:

| Field | Type | Required | Notes |
|---|---|---|---|
| `defaults.fallback_model` | `String` | no | Hard fallback if neither task config nor persona override provides a model. If still missing, code uses the compiled-in default `x-ai/grok-4-mini`. |
| `defaults.fallback_temperature` | `f64` | no | Same precedence; compiled-in default `0.5`. |
| `defaults.fallback_max_tokens` | `u32` | no | Same precedence; compiled-in default `200`. |
| `tasks.<name>.model` | `String` | yes | Primary model for the task. |
| `tasks.<name>.fallback` | `String` | no | Secondary model used by `OpenRouterClient` if the primary call fails. |
| `tasks.<name>.temperature` | `f64` | no | Per-task sampling temperature. |
| `tasks.<name>.max_tokens` | `u32` | no | Per-task token cap. |
| `tasks.<name>.description` | `String` | no | Documentation field, ignored by code. |
| `tasks.<name>.dimensions` | `u32` | no | Embedding-only. Ignored by chat / insight tasks. |

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

```
persona_override > task_config > defaults > compiled-in fallback
```

Where each step contributes:

- **`persona_override`** — set per persona in `genome.art_metadata.model`. Wins for the `model` field only; temperature / max_tokens still come from the task config.
- **`task_config`** — `[tasks.<name>]` block.
- **`defaults`** — `[defaults]` block.
- **Compiled-in fallback** — `x-ai/grok-4-mini`, temperature `0.5`, max_tokens `200`. Hard-coded in `model_config.rs`.

If `resolve()` is called with an unknown task name, it falls back through `defaults → compiled-in` and emits a `tracing::warn!` ("model_config: unknown task, using defaults").

## Stability commitments (OSS 0.x)

For the duration of `0.x`, the OSS engine commits to:

1. **No removed fields.** Existing field names in `[defaults]` and `[tasks.<name>]` will not disappear.
2. **No renamed fields.** `fallback` will not become `fallback_model`. `model` will not become `primary_model`. Etc.
3. **No newly required fields.** Anything added is optional with a sensible default.
4. **No removed task names from this list:** `chat_companion`, `insight_extraction`. Reserved task names (`pde_decision`, `embedding`) may shift if a real implementation lands and supersedes their current placeholder semantics; that change will be called out in the changelog.
5. **Resolution precedence is fixed.** `persona_override > task_config > defaults > compiled-in fallback`.

What may still change without notice:

- Compiled-in fallback values (currently `x-ai/grok-4-mini` / `0.5` / `200`). These are fail-safes, not contract.
- Internal struct shapes inside `eros-engine-llm` if `#[non_exhaustive]` is added.
- The `description` field's handling — it's documentation today, may become structured metadata later.
- Newly added optional fields and new task names.

## What this config does NOT control

- **Voyage embedding** — `VoyageClient` hard-codes `voyage-3-lite` and reads `VOYAGE_API_KEY` directly. The `[tasks.embedding]` block is reserved for a future generalisation.
- **PDE decisions** — current PDE is pure rule-based logic in `eros-engine-core/src/pde.rs`. The `[tasks.pde_decision]` block is reserved for an optional LLM decision layer that has not landed yet.
- **OpenRouter API key** — read directly from `OPENROUTER_API_KEY`, not the config file.
- **Per-call streaming / response format flags** — fixed in `OpenRouterClient`.

## Worked example: persona override

```toml
[tasks.chat_companion]
model = "x-ai/grok-4-fast"
fallback = "deepseek/deepseek-chat-v3.2"
temperature = 0.85
max_tokens = 600
```

Persona genome `art_metadata.model = "anthropic/claude-sonnet-4"` for a particular character. When that persona is in the chat session, `resolve("chat_companion", Some("anthropic/claude-sonnet-4"))` returns:

| Field | Value | Source |
|---|---|---|
| `model` | `anthropic/claude-sonnet-4` | persona override |
| `fallback_model` | `deepseek/deepseek-chat-v3.2` | task config (override only touches `model`) |
| `temperature` | `0.85` | task config |
| `max_tokens` | `600` | task config |

## Compatibility test fixture

`model_config.rs` includes a fixture that asserts every field of a representative TOML round-trips correctly. Any breaking schema change will fail CI before it ships. See `compat_fixture_locks_full_schema` in `crates/eros-engine-llm/src/model_config.rs`.
