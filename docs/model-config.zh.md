# 模型配置 (model_config.toml)

[English](model-config.md) · [中文](model-config.zh.md)

引擎用一份 TOML 在 server 启动时载入,决定每个任务用哪个 LLM 模型,以及各种参数。可选的 per-tier 覆盖层可以再覆盖一层。

## 文件位置

- 默认路径: `examples/model_config.toml`(相对于工作目录)
- 覆盖: `MODEL_CONFIG_PATH` 环境变量
- 服务启动时由 `eros-engine-server/src/main.rs` 一次性载入(直接读 `MODEL_CONFIG_PATH` + `ModelConfig::from_toml_str`)。`crates/eros-engine-llm/src/model_config.rs` 里的 `ModelConfig::load()` 是给 library embedder 用的便利方法,默认路径一致
- 以 `Arc<ModelConfig>` 形式挂在 `AppState` 上,所有 chat / post-process 轮共享
- Server 启动时还会调一次 `dotenvy::dotenv()`,所以快速开始里 `cp .env.example .env` 之后可以直接 `cargo run`,不需要手动 `source .env`

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
allow_traits = ["tag_a", "tag_b"]           # 可选,prompt-trait 白名单(三态)
description  = "free-form note"             # 可选,纯文档,代码不消费
dimensions   = 512                          # 可选,只对 embedding 类任务有意义

[tasks.<name>.tiers.<tier>]
model        = "<provider>/<model-id>"      # 可选,该 tier 覆盖任务级 model
fallback     = "<provider>/<model-id>"      # 可选,该 tier 覆盖任务级 fallback
allow_traits = ["tag_a"]                    # 可选,该 tier 覆盖任务级 allow_traits
```

字段细节:

| 字段 | 类型 | 必填 | 备注 |
|---|---|---|---|
| `defaults.fallback_model` | `String` | 否 | 任务没给 model 时的兜底。还是缺,走代码内置的 `x-ai/grok-4-mini`。 |
| `defaults.fallback_temperature` | `f64` | 否 | 同样优先级链;代码内置默认 `0.5`。 |
| `defaults.fallback_max_tokens` | `u32` | 否 | 同样;代码内置默认 `200`。 |
| `tasks.<name>.model` | `String` | 是 | 任务的主模型（任务默认块）。 |
| `tasks.<name>.fallback` | `String` | 否 | 主模型调用挂掉时,`OpenRouterClient` 用的备选。 |
| `tasks.<name>.temperature` | `f64` | 否 | 任务级 sampling temperature。无 per-tier 覆盖。 |
| `tasks.<name>.max_tokens` | `u32` | 否 | 任务级 token 上限。无 per-tier 覆盖。 |
| `tasks.<name>.allow_traits` | `Array<String>` | 否 | 该任务的 prompt-trait 白名单（三态:缺省=不过滤;`[]`=丢弃所有;`["a","b"]`=白名单）。没有匹配 tier 块时使用。 |
| `tasks.<name>.tiers.<tier>` | 子表 | 否 | per-tier 覆盖。可设 `model`、`fallback`、`allow_traits`。不覆盖 `temperature` 或 `max_tokens`。 |
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

`model` 和 `fallback`:

```
匹配的 tier 块 > 任务默认块 > [defaults] > 代码内置兜底
```

`allow_traits`:

```
匹配的 tier 块 > 任务默认块
```

`temperature` 和 `max_tokens`:

```
任务默认块 > [defaults] > 代码内置兜底
```

各级贡献什么:

- **匹配的 tier 块** —— `[tasks.<name>.tiers.<tier>]`,`<tier>` 来自 chat 请求的 `tier` 字段(正则 `^[a-z0-9_]{1,32}$`)。如果请求没有 `tier` 或者 tier 未知(没有对应子表),使用任务默认块,同时 `tracing::warn!`。
- **任务默认块** —— `[tasks.<name>]` 块。
- **`[defaults]`** —— 顶层 defaults 块。
- **代码内置兜底** —— `x-ai/grok-4-mini`,temperature `0.5`,max_tokens `200`。`model_config.rs` 里 hard-code。

`temperature` 和 `max_tokens` 只在任务级设置 —— per-tier 子表不覆盖它们。

如果 `resolve()` 被传了一个未知 task 名,会落到 `defaults → 内置兜底`,同时 `tracing::warn!` 一条 "model_config: unknown task, using defaults"。

## 稳定性承诺 (OSS 0.x)

整个 `0.x` 期间,OSS 引擎承诺:

1. **不删字段。** `[defaults]` 和 `[tasks.<name>]` 现有的字段名不会消失。
2. **不改字段名。** `fallback` 不会变成 `fallback_model`。`model` 不会变成 `primary_model`。
3. **不加新的必填字段。** 后续加的字段一律可选,带合理默认。
4. **以下任务名不会被删除:** `chat_companion`,`insight_extraction`。Reserved 名 (`pde_decision`,`embedding`) 在真实现落地后可能有 semantic 变化,但会在 changelog 里明确写。
5. **解析优先级顺序固定。** `model`/`fallback`/`allow_traits` 走 `匹配 tier > 任务默认块 > [defaults] > 内置兜底`;`temperature`/`max_tokens` 只在任务级设置。

可能不通知就改的:

- 代码内置兜底值(目前 `x-ai/grok-4-mini` / `0.5` / `200`)—— 这是 fail-safe,不是 contract。
- `eros-engine-llm` 内部 struct 形状(可能加 `#[non_exhaustive]`)。
- `description` 字段的处理 —— 现在是纯文档,以后可能变成结构化 metadata。
- 新加的可选字段(`allow_traits`、`tiers`)和新的 task 名。

### Changelog 说明

- **`persona_override`（`art_metadata.model`）从本版本起引擎不再读取。** 请改用 `[tasks.<name>.tiers.<tier>]` 做 per-tier 模型选择。persona JSONB `art_metadata` 里的 `model` 字段可能仍然存在,但会被静默忽略。

## 这份 config 不管的事

- **Voyage embedding** —— `VoyageClient` hard-code `voyage-3-lite`,直接读 `VOYAGE_API_KEY`。`[tasks.embedding]` 是给未来通用化留的位置。
- **PDE 决策** —— 当前 PDE 是 `eros-engine-core/src/pde.rs` 里的纯规则逻辑。`[tasks.pde_decision]` 是给未来可选 LLM 决策层留的位置,目前没接。
- **OpenRouter API key** —— 直接读 `OPENROUTER_API_KEY`,不走配置文件。
- **Per-call streaming / response format flag** —— 在 `OpenRouterClient` 里写死。

## 实例: tier 级别的解析

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

请求带 `"tier": "gold"` 时,`resolve("chat_companion", "gold")` 返回:

| 字段 | 值 | 来源 |
|---|---|---|
| `model` | `x-ai/grok-4.20` | `tiers.gold` |
| `fallback` | `["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3"]` | `tiers.gold` |
| `allow_traits` | `["allow_nsfw", "allow_politics"]` | `tiers.gold` |
| `temperature` | `0.8` | 任务默认块（无 tier 覆盖） |
| `max_tokens` | `1200` | 任务默认块（无 tier 覆盖） |

请求带 `"tier": "free"` 时:

| 字段 | 值 | 来源 |
|---|---|---|
| `model` | `qwen/qwen3.6-flash` | `tiers.free` |
| `fallback` | `["deepseek/deepseek-v4-flash"]` | `tiers.free` |
| `allow_traits` | `["allow_politics"]` | `tiers.free` |
| `temperature` | `0.8` | 任务默认块 |
| `max_tokens` | `1200` | 任务默认块 |

不带 `tier` 或 tier 未知时,所有字段从任务默认块解析。

## 兼容性测试 fixture

`model_config.rs` 里有一个 fixture,把代表性 TOML 的每个字段都做 round-trip 验证。任何破坏性 schema 改动会让 CI 在合并前直接挂。见 `crates/eros-engine-llm/src/model_config.rs` 的 `compat_fixture_locks_full_schema` 测试。
