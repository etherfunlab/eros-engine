# 模型配置 (model_config.toml)

[English](model-config.md) · [中文](model-config.zh.md)

引擎用一份 TOML 在 server 启动时载入,决定每个任务用哪个 LLM 模型,以及各种参数。可选的 per-tier 覆盖层可以再覆盖一层。

## 文件位置

- 默认路径: `examples/model_config.toml`(相对于工作目录)。examples 内的是示意模版，请依自身业务需求修改（或用 `MODEL_CONFIG_PATH` 指向自己的文件）。
- 覆盖: `MODEL_CONFIG_PATH` 环境变量
- 服务启动时由 `eros-engine-server/src/main.rs` 一次性载入(直接读 `MODEL_CONFIG_PATH` + `ModelConfig::from_toml_str`)。`crates/eros-engine-llm/src/model_config.rs` 里的 `ModelConfig::load()` 是给 library embedder 用的便利方法,默认路径同为 `examples/model_config.toml`
- 以 `Arc<ModelConfig>` 形式挂在 `AppState` 上,所有 chat / post-process 轮共享
- Server 启动时还会调一次 `dotenvy::dotenv()`,所以快速开始里 `cp .env.example .env` 之后可以直接 `cargo run`,不需要手动 `source .env`

## Schema

```toml
[defaults]
fallback_model       = "x-ai/grok-4-mini"   # task 没指定 model + 没指定 fallback 时兜底
fallback_temperature = 0.5
fallback_max_tokens  = 200

[tasks.<name>]
model        = "<provider>/<model-id>"      # 必填；也接受数组（轮转）或表（加权）—— 见「主模型选择」
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
| `tasks.<name>.model` | `String` \| `Array<String>` \| `Table<String,f64>` | 是 | 主模型。String = 固定；数组 = 轮转（round-robin）；表 = 加权随机。见「主模型选择」。 |
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

### 开启 / 关闭抽取任务

`insight_extraction`（逐轮事实抽取）和 `memory_extraction`（会话结束时的 dreaming
sweeper）由对应 `[tasks.*_extraction]` **段落是否存在**控制：

- **段落存在** → `filter_prompt` 为**必填**；为空或缺失时服务器拒绝启动。
- **段落缺失** → 对应抽取**关闭**。引擎正常启动并运行（逐轮的 `insight_extraction`
  被跳过；dreaming sweeper 保持空转不工作）。

> **行为变更（0.6.x）：** 旧版本把这两个段落设为必填（缺段落会启动失败）。现在改为
> 缺省即关闭。随仓库发布的 `examples/model_config.toml` 仍保留两个段落，所以默认行为
> （两个抽取都开启）不变。

`reasoning` 与其他任务一致：不写 → 由模型决定；`reasoning = { enabled = false }` →
强制关闭推理；`{ enabled = true }` → 强制开启。

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

## 主模型选择

`model`（任务级和 per-tier）接受三种写法：

```toml
model = "x-ai/grok-4.20"                              # 固定
model = ["x-ai/grok-4.20", "z-ai/glm-4.7-flash"]     # 轮转（round-robin，确定性交替）
model = { "x-ai/grok-4.20" = 0.8, "z-ai/glm-4.7-flash" = 0.2 }  # 加权随机
```

- **轮转（Round-robin）**：每次调用确定性地轮流选取（进程级计数器；重启清零；每个副本独立计数）。
- **加权（Weighted）**：随机抽取；权重为任意正数，按总和归一化（`{a = 8, b = 2}` 等价于 `{a = 0.8, b = 0.2}`）。非正权重直接丢弃。
- `["a","b"]` 与 `{a = 1, b = 1}` 的长期分布相同，但机制不同（确定性 vs. 随机）。
- 单元素数组/表的行为等同于固定字符串。空数组/表会透传到下一级优先级。

**TOML 坑：** 内联表的裸键只允许 `A-Za-z0-9_-`，但 model id 含有 `/` 和 `.`，因此加权写法的键**必须加引号**：`{ "x-ai/grok-4.20" = 0.8 }`。数组写法无此限制。

### fallback 去重

主模型选定后，已选出的那个 id 会从解析后的 `fallback` 链中自动移除 —— 刚失败的模型重试毫无意义。轮转/加权主模型下，这是动态的：每次调用只去掉本次选中的那个 id。

## 稳定性承诺 (OSS 0.x)

整个 `0.x` 期间,OSS 引擎承诺:

1. **不删字段。** `[defaults]` 和 `[tasks.<name>]` 现有的字段名不会消失。
2. **不改字段名。** `fallback` 不会变成 `fallback_model`。`model` 不会变成 `primary_model`。
3. **不加新的必填字段。** 后续加的字段一律可选,带合理默认。
4. **以下任务名不会被删除:** `chat_companion`,`insight_extraction`。Reserved 名 (`pde_decision`,`embedding`) 在真实现落地后可能有 semantic 变化,但会在 changelog 里明确写。
5. **解析优先级顺序固定。** `model`/`fallback`/`allow_traits` 走 `匹配 tier > 任务默认块 > [defaults] > 内置兜底`;`temperature`/`max_tokens` 只在任务级设置。
6. **`model` 接受字符串、数组（轮转）或表（加权）。** 纯字符串写法永久有效；数组/表形式是向后兼容的扩展。

可能不通知就改的:

- 代码内置兜底值(目前 `x-ai/grok-4-mini` / `0.5` / `200`)—— 这是 fail-safe,不是 contract。
- `eros-engine-llm` 内部 struct 形状(可能加 `#[non_exhaustive]`)。
- `description` 字段的处理 —— 现在是纯文档,以后可能变成结构化 metadata。
- *未来*新增的可选字段和新的 task 名（本文已记录的字段——含 `allow_traits`、`tiers`——受承诺 1–3 保护）。

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
