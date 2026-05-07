# 模型配置 (model_config.toml)

[English](model-config.md) · [中文](model-config.zh.md)

引擎用一份 TOML 在 server 启动时载入,决定每个任务用哪个 LLM 模型,以及各种参数。Persona 层级的 model override 可以再覆盖一层。

## 文件位置

- 默认路径: `examples/model_config.toml`(相对于工作目录)
- 覆盖: `MODEL_CONFIG_PATH` 环境变量
- 启动时一次性载入,走 `ModelConfig::load()`(`crates/eros-engine-llm/src/model_config.rs`)
- 以 `Arc<ModelConfig>` 形式挂在 `AppState` 上,所有 chat / post-process 轮共享

## Schema

```toml
[defaults]
fallback_model       = "x-ai/grok-4-mini"   # task 没指定 model + 没指定 fallback 时兜底
fallback_temperature = 0.5
fallback_max_tokens  = 200

[tasks.<name>]
model        = "<provider>/<model-id>"      # 必填
fallback     = "<provider>/<model-id>"      # 可选,主模型挂掉时的备选
temperature  = 0.85                         # 可选,缺省走 defaults.fallback_temperature
max_tokens   = 600                          # 可选,缺省走 defaults.fallback_max_tokens
description  = "free-form note"             # 可选,纯文档,代码不消费
dimensions   = 512                          # 可选,只对 embedding 类任务有意义
```

字段细节:

| 字段 | 类型 | 必填 | 备注 |
|---|---|---|---|
| `defaults.fallback_model` | `String` | 否 | 任务和 persona override 都没给 model 时的兜底。还是缺,走代码内置的 `x-ai/grok-4-mini`。 |
| `defaults.fallback_temperature` | `f64` | 否 | 同样优先级链;代码内置默认 `0.5`。 |
| `defaults.fallback_max_tokens` | `u32` | 否 | 同样;代码内置默认 `200`。 |
| `tasks.<name>.model` | `String` | 是 | 任务的主模型。 |
| `tasks.<name>.fallback` | `String` | 否 | 主模型调用挂掉时,`OpenRouterClient` 用的备选。 |
| `tasks.<name>.temperature` | `f64` | 否 | 任务级 sampling temperature。 |
| `tasks.<name>.max_tokens` | `u32` | 否 | 任务级 token 上限。 |
| `tasks.<name>.description` | `String` | 否 | 文档字段,代码忽略。 |
| `tasks.<name>.dimensions` | `u32` | 否 | 只对 embedding 类任务有意义,chat / insight 任务忽略。 |

## 任务名

| 名字 | 谁消费 | 状态 |
|---|---|---|
| `chat_companion` | `pipeline::handlers::ReplyHandler` 和 `GiftHandler` (chat completions) | live |
| `insight_extraction` | `pipeline::post_process::extract_facts` 和 `extract_structured_insights` (抽事实 + JSONB merge) | live |
| `pde_decision` | reserved — 当前 PDE 是纯规则,不调 LLM | reserved |
| `embedding` | reserved — `VoyageClient` 自己读 `VOYAGE_API_KEY` 并 hard-code `voyage-3-lite`,不走这条路径 | reserved |

`[tasks.<name>]` 只有当代码里真有 `model_config.resolve("<name>", ...)` 调用时才有意义。当前调用点:

- `crates/eros-engine-server/src/pipeline/handlers.rs` → `chat_companion`
- `crates/eros-engine-server/src/pipeline/post_process.rs` → `insight_extraction`

其它要么是给未来功能留的位置 (`pde_decision`),要么是 vestigial (`embedding` —— Voyage 完全不走这条路径)。

## 解析优先级

```
persona_override > task_config > defaults > 代码内置兜底
```

各级贡献什么:

- **`persona_override`** —— per-persona 在 `genome.art_metadata.model` 里设。**只**覆盖 `model` 字段,temperature / max_tokens 还是走 task config。
- **`task_config`** —— `[tasks.<name>]` 块。
- **`defaults`** —— `[defaults]` 块。
- **代码内置兜底** —— `x-ai/grok-4-mini`,temperature `0.5`,max_tokens `200`。`model_config.rs` 里 hard-code。

如果 `resolve()` 被传了一个未知 task 名,会落到 `defaults → 内置兜底`,同时 `tracing::warn!` 一条 "model_config: unknown task, using defaults"。

## 稳定性承诺 (OSS 0.x)

整个 `0.x` 期间,OSS 引擎承诺:

1. **不删字段。** `[defaults]` 和 `[tasks.<name>]` 现有的字段名不会消失。
2. **不改字段名。** `fallback` 不会变成 `fallback_model`。`model` 不会变成 `primary_model`。
3. **不加新的必填字段。** 后续加的字段一律可选,带合理默认。
4. **以下任务名不会被删除:** `chat_companion`,`insight_extraction`。Reserved 名 (`pde_decision`,`embedding`) 在真实现落地后可能有 semantic 变化,但会在 changelog 里明确写。
5. **解析优先级顺序固定。** `persona_override > task_config > defaults > 内置兜底`。

可能不通知就改的:

- 代码内置兜底值(目前 `x-ai/grok-4-mini` / `0.5` / `200`)—— 这是 fail-safe,不是 contract。
- `eros-engine-llm` 内部 struct 形状(可能加 `#[non_exhaustive]`)。
- `description` 字段的处理 —— 现在是纯文档,以后可能变成结构化 metadata。
- 新加的可选字段和新的 task 名。

## 这份 config 不管的事

- **Voyage embedding** —— `VoyageClient` hard-code `voyage-3-lite`,直接读 `VOYAGE_API_KEY`。`[tasks.embedding]` 是给未来通用化留的位置。
- **PDE 决策** —— 当前 PDE 是 `eros-engine-core/src/pde.rs` 里的纯规则逻辑。`[tasks.pde_decision]` 是给未来可选 LLM 决策层留的位置,目前没接。
- **OpenRouter API key** —— 直接读 `OPENROUTER_API_KEY`,不走配置文件。
- **Per-call streaming / response format flag** —— 在 `OpenRouterClient` 里写死。

## 实例: persona 级别的 model override

```toml
[tasks.chat_companion]
model = "x-ai/grok-4-fast"
fallback = "deepseek/deepseek-chat-v3.2"
temperature = 0.85
max_tokens = 600
```

某个 persona 的 genome `art_metadata.model = "anthropic/claude-sonnet-4"`。当这个 persona 在 chat session 里时,`resolve("chat_companion", Some("anthropic/claude-sonnet-4"))` 返回:

| 字段 | 值 | 来源 |
|---|---|---|
| `model` | `anthropic/claude-sonnet-4` | persona override |
| `fallback_model` | `deepseek/deepseek-chat-v3.2` | task config (override 只动 `model`) |
| `temperature` | `0.85` | task config |
| `max_tokens` | `600` | task config |

## 兼容性测试 fixture

`model_config.rs` 里有一个 fixture,把代表性 TOML 的每个字段都做 round-trip 验证。任何破坏性 schema 改动会让 CI 在合并前直接挂。见 `crates/eros-engine-llm/src/model_config.rs` 的 `compat_fixture_locks_full_schema` 测试。
