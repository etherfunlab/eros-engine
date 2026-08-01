# 模型配置

[English](model-config.md) · [中文](model-config.zh.md)

引擎的 LLM 模型选择配置位于服务器启动时加载的 TOML 文件中。它为每个任务配置模型和参数，并可在其上添加可选的 per-tier 覆盖。

## 文件位置

- 默认路径：`examples/model_config.toml`（相对于工作目录）。`examples/` 下的文件是示例模板——请根据自己的需求调整（或通过 `MODEL_CONFIG_PATH` 指向自己的文件）。
- 覆盖方式：`MODEL_CONFIG_PATH` 环境变量（单文件），或 `MODEL_CONFIG_DIR`（目录模式，见下）。两者互斥——同时设置会导致启动报错。
- 服务器启动时由 `eros-engine-server/src/main.rs` 加载一次（`resolve_config_source` → `ModelConfig::from_toml_file` / `from_toml_dir`）。对于嵌入该库的应用，`crates/eros-engine-llm/src/model_config.rs` 中的 `ModelConfig::load()` 执行相同的解析逻辑，并使用相同的默认路径（`examples/model_config.toml`）。
- 以 `Arc<ModelConfig>` 保存在 `AppState` 中；由所有 chat / post-process 轮次共享
- 服务器启动时还会调用 `dotenvy::dotenv()`，因此快速入门中执行 `cp .env.example .env` 后无需显式执行 `source .env`

## 目录模式

`MODEL_CONFIG_DIR` 指向一个目录，启动时将其中的 `.toml` 文件合并成完整配置——用于把大配置按 section 拆分，而不是分层覆盖：

- 只读取目录第一层（不递归）；dotfile 和非 `.toml` 条目会被跳过。目录中没有任何 `.toml` 文件会导致启动报错。
- 每个文件独立解析后合并；加载顺序为文件名字节序（重复即报错，因此顺序不影响结果，只保证报错信息可复现）。
- `[defaults]` 和每个 `[tasks.<name>]` 只能来自一个文件。同一 section 定义两次会启动失败并点名两个文件：`model_config merge failed: [tasks.chat_companion] in chat.toml already defined in base.toml`。文件之间不存在覆盖或优先级。
- 合并成功后服务器会记录文件清单日志：`model_config: loaded from dir`（含目录、文件名列表和数量）。
- 发布的 Docker 镜像内置了 `MODEL_CONFIG_PATH`（`docker/Dockerfile`）。要在该镜像中使用目录模式，需在传入目录的同时清空它：`-e MODEL_CONFIG_PATH= -e MODEL_CONFIG_DIR=/etc/eros/model.d`（空值视为未设置）。

拆分示例：

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

字段详情：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `defaults.fallback_model` | `String` | 否 | 任务配置未提供 model 时使用的最终 fallback。若仍然缺失，代码使用编译时内置默认值 `x-ai/grok-4-mini`。 |
| `defaults.fallback_temperature` | `f64` | 否 | 优先级相同；编译时内置默认值为 `0.5`。 |
| `defaults.fallback_max_tokens` | `u32` | 否 | 优先级相同；编译时内置默认值为 `200`。 |
| `defaults.ignore_providers` | `Array<String>` | 否 | 要从**每个**任务的路由中排除的 OpenRouter provider slug。每个条目必须带 `@openrouter` 后缀（`"some-bad-provider-slug@openrouter"`）——裸 slug、其他 `@<provider>` 后缀、或格式错误的 `@` 语法都会拒绝启动。发到 wire 前会剥掉后缀：只作为 `provider.ignore`（裸 slug）发给 OpenRouter 调用；自定义 provider 和 Voyage 永远不会收到它。`allow_fallbacks` 仍为 `true`，因此模型仍可由任意健康的 provider 提供。某个 provider 为模型返回乱码时（例如未解码的 byte-BPE 文本——issue #84），可使用此字段。通过 OpenRouter generation API 查找有问题的 slug。为空或缺失表示不排除任何 provider。 |
| `tasks.<name>.model` | `String` \| `Array<String>` \| `Table<String,f64>` | 是 | 主模型。字符串 = 固定；数组 = round-robin；表 = weighted 随机。参见“主模型选择”。 |
| `tasks.<name>.fallback` | `String` | 否 | 主调用失败时由 `OpenRouterClient` 使用的次要模型。 |
| `tasks.<name>.temperature` | `f64` | 否 | 每任务的采样 temperature。无 per-tier 覆盖。 |
| `tasks.<name>.max_tokens` | `u32` | 否 | 每任务的 token 上限。无 per-tier 覆盖。 |
| `tasks.<name>.allow_traits` | `Array<String>` | 否 | 此任务的 prompt-trait allow-list（三态：缺失 = 不设门控；`[]` = 丢弃所有 trait；`["a","b"]` = 白名单）。找不到匹配的 tier 块时使用。 |
| `tasks.<name>.tiers.<tier>` | 子表 | 否 | Per-tier 覆盖。可设置 `model`、`fallback` 和/或 `allow_traits`。不覆盖 `temperature` 或 `max_tokens`。 |
| `tasks.chat_companion.input_filter` | `bool` \| `f64` | 否 | 用户输入改写 filter 的全局 trigger。仅可在 `chat_companion` 的任务级配置中设置（无 per-tier 覆盖）。`false`/缺失 = 关闭，`true` = 每轮执行，`0.8` = 约 80% 的轮次执行（超出 `[0.0, 1.0]` 的数字会被拒绝）。参见“`input_filter`”。 |
| `tasks.<name>.description` | `String` | 否 | 文档字段，代码忽略。 |

### `[providers]` — 自定义 chat/embeddings 端点（opt-in）

```toml
[providers]
venice = { chat = "https://api.venice.ai/api/v1/chat/completions" }
mixed  = { chat = "https://x/v1/chat/completions", embeddings = "https://x/v1/embeddings" }
local  = { embeddings = "http://127.0.0.1:8080/v1/embeddings" }

[providers.proxy]           # TOML section 写法也可以
chat    = "https://proxy.internal/v1/chat/completions"
headers = { "X-Team" = "companion", "X-Env" = "prod" }
```

每个条目都是一张表，最多三个 key——`chat`（OpenAI 兼容的
chat-completions URL）、`embeddings`（OpenRouter 兼容的 embeddings
URL，wire 形态见
[`https://openrouter.ai/docs/api_reference/embeddings`](https://openrouter.ai/docs/api_reference/embeddings)）、
以及 `headers`（原样发到该条目所有端点的每个请求上）。**字符串值会被拒绝**：
0.9.4 之前的写法（`venice = "https://…"`）已彻底移除、无兼容层，启动报错
会指出正确的表写法。适用 `deny_unknown_fields`——未知 key、空表、空
URL 字符串同样拒绝加载。

在内置 OpenRouter client 之外声明额外的端点。在任何接受
`model` / `fallback` 的位置（三种形态：固定、轮询、加权）给模型 slug 加
`@<name>` 后缀即可引用（chat 类任务）；embedding 则在
`[tasks.embedding]` 的模型字段上加后缀：

```toml
[tasks.chat_companion]
model = "venice-uncensored@venice"   # 由 [providers].venice.chat 提供服务
fallback = ["x-ai/grok-4.20"]        # 无后缀 → 内置 OpenRouter

[tasks.embedding]
model = "bge-m3@local"               # 由 [providers].local.embeddings 提供服务
```

以下规则全部在启动时强制校验（任一违反即拒绝启动）：

- **名称**匹配 `[a-z0-9_]+`。`voyage` 仍为保留字——它的原生 API 不是本
  机制所讲的 OpenRouter 兼容 embeddings 格式，且 `$VOYAGE_API_KEY` 已属于
  内置的原生 Voyage 客户端。`openrouter` 是可以声明的合法名字：它不新增
  一个独立 provider，而是**按 key 覆盖内置的 OpenRouter 端点 URL**（见下）。
- **URL** 完整、原样 POST——引擎不做任何路径拼接。被 chat 类任务引用的
  provider 必须声明 `chat`；被 `[tasks.embedding]` 引用的必须声明
  `embeddings`。缺失其一即拒绝启动，并点名 slug、条目和缺失的 key。
- **`headers`**（可选）是原样发到该条目所有端点（`chat` 和
  `embeddings` 都算）每个请求上的表。`Authorization` 和 `Content-Type`
  是引擎自有的，声明它们（不区分大小写）即拒绝加载——静默覆盖
  `Authorization` 是最糟糕的那种坑。其余每个 name/value 都必须是合法的
  HTTP header，否则拒绝加载。
- **API key** 来自环境变量 `<大写名称>_API_KEY`（`venice` →
  `$VENICE_API_KEY`），一个 key 同时覆盖该条目的 `chat` 和 `embeddings`
  端点，仅对被某个模型 slug 实际引用的 provider 强制要求；已声明但未引用
  的条目无需 key。`openrouter` 继续用现有的 `$OPENROUTER_API_KEY`——命名
  约定退化为那个本来就存在的变量。
- **模型 id 用该 provider 自己的 slug**，原样上线——引擎从不在 provider
  之间转译模型名。
- 模型 id 中的字面 `@` 用 `\@` 转义。TOML 双引号字符串写作
  `"weird\\@vendor/m"`；每个 slug 至多一个未转义的 `@`。
- **按模型匹配的表用裸 id**：`model_name_display_override`、`output_regex`
  的 `models`、`output_filter` trigger 的 `models`，匹配时都不带
  `@provider`。
- **wire 形态**：自定义 provider 收到严格的 OpenAI 兼容子集。
  `[defaults].ignore_providers`、`[defaults].provider_sort` 和任务级
  `reasoning` 对自定义 provider **不生效**；它们只收到该条目自己声明的
  `headers`，绝不会收到 OpenRouter 归因标头。
- **审计**：自定义 provider 服务的行记录
  `model = "<上游回显>@<name>"`，`generation_id` 原样存 provider 返回的
  id——用 `generation_id` 去 join OpenRouter 日志时这些行会 miss，
  `model` 列会说明原因。
- 使用 `MODEL_CONFIG_DIR` 时，`[providers]` 作为单个顶层 key 整体合并
  （同 `[defaults]`，不同于 `[tasks]`）：所有 provider 必须写在同一个文件里。

#### 通过 `[providers].openrouter` 覆盖内置端点

```toml
[providers.openrouter]
embeddings = "http://my-proxy/v1/embeddings"
headers    = { "HTTP-Referer" = "https://eros.example", "X-OpenRouter-Title" = "Eros" }
```

- 声明的每个 key（`chat` 和/或 `embeddings`）覆盖对应的内置 URL；缺失的
  key 保留内置默认值（`https://openrouter.ai/api/v1/chat/completions` /
  `https://openrouter.ai/api/v1/embeddings`）。这条“部分覆盖”规则只对
  `openrouter` 生效——普通条目缺 key 且被引用时是启动报错，因为它没有内置
  默认值可以兜底。
- 覆盖**只**改变 URL。流量走的仍然是完整的 OpenRouter wire：
  `provider.ignore`、`provider_sort`、任务级 `reasoning` 全部照常发送——
  和只收到严格 OpenAI 子集的自定义 provider 不同。
- **归因 header 现在只从这里来。** 没有 `[providers.openrouter]` 条目，
  或有条目但没写 `headers`，就不发任何归因 header。
  `OPENROUTER_APP_REFERER` / `OPENROUTER_APP_TITLE` /
  `OPENROUTER_APP_CATEGORIES` 环境变量**软废弃**：仍然设置也会被静默
  忽略，不是启动报错——请把同样的 header 改到
  `[providers.openrouter].headers` 下（对应关系见
  [`llm-audit.zh.md`](llm-audit.zh.md)）。
- API key 仍然是 `$OPENROUTER_API_KEY`。
- **`OPENROUTER_BASE_URL` 环境变量已不存在。** 唯一的覆盖方式是
  `[providers].openrouter.chat` / `[providers].openrouter.embeddings`。仍然
  设置 `OPENROUTER_BASE_URL` 不会被读取，也不会导致启动报错——它现在只是
  一个无关的环境变量。
- `voyage` 仍不能在 `[providers]` 中声明（见上文）。

#### `[defaults].ignore_providers` — 必须带 `@openrouter`

```toml
[defaults]
ignore_providers = ["some-bad-provider-slug@openrouter"]
```

每个条目都必须能解析出（与模型 slug 相同的 `@` 后缀语法）一个非空的上游
slug 加 provider `openrouter`；裸条目、其他 `@<provider>` 后缀、或格式
错误的 `@` 语法都会拒绝启动，并点名该条目和正确写法。wire 行为不变：
`provider.ignore` 只带裸上游 slug，只发给 OpenRouter 流量——强制后缀把这个
作用范围写进了语法本身；自定义 provider 和 Voyage 永远不会收到
`provider.ignore`。`[defaults].provider_sort` 不受影响（它没有 per-entry
语法可限定范围）。对在这个后缀出现之前写好的配置是破坏性变更——修复方式
是给每个条目加上 `@openrouter`。

### `model_name_display_override`（仅限 chat 任务）

控制 chat SSE `meta` frame 中发送给客户端的 `model` 值。它**只**影响客户端显示——绝不影响 OpenRouter 请求、持久化的 assistant 记录或用量日志。该字段位于 `[tasks.chat_companion]` 的任务级配置中；所有 tier 都会继承。为其他任务设置该字段可以通过解析，但不会产生效果。

| 形式 | 示例 | 行为 |
|---|---|---|
| `false` *（缺失时的默认值）* | `false` | frame 中**省略** `model` |
| `true` | `true` | 发送真实 model id（0.x 之前的行为） |
| 字符串 | `"Aria"` | 始终发送 `"Aria"` |
| 数组 | `["Aria","Nova"]` | 每个气泡随机选择（历史重放时重新随机） |
| map | `{ "deepseek/x" = "Aria", default = "Companion" }` | 将真实 id 映射为名称；未列出时使用 `default`；没有 `default` 时省略 |

由于显示名称从不持久化，**数组**形式会在历史重放时重新随机；`bool`/`string`/`map` 形式是确定性的。

### `output_filter` — 二次回复改写（仅限 chat 任务）

在客户端看到完整的 chat 回复之前，先将其交给第二个 LLM 处理。filter **默认关闭**，除非显式启用，否则不会产生任何效果。

#### 启用 filter

`output_filter` 是 `[tasks.chat_companion]` 上的 bool 标志。它充当任务级默认值，任何 tier 子表都可以覆盖：

```toml
[tasks.chat_companion]
output_filter = true              # task-level default; applies when no matching tier block exists

[tasks.chat_companion.tiers.gold]
output_filter = true              # per-tier override; takes precedence over the task default
```

解析遵循与其他所有 `chat_companion` 字段相同的优先级：

```
matched tier block > task default block
```

两处都未设置 `output_filter` 时，编译时内置默认值为 `false`。

#### 门控规则

仅当以下条件**全部**满足时，filter 才会在当前轮次运行：

1. 按上述优先级解析后，当前 tier 的 `output_filter` 为 `true`。
2. 配置中存在 `[tasks.chat_output_filter]`。
3. 当前 tier 解析得到的 `filter_prompt` 非空白。
4. 所有已设置的 `trigger` 谓词均通过（见下文）。

任何条件不满足时，filter 都**不生效**——原始回复不经修改直接交付。

#### `[tasks.chat_output_filter]` 字段

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

**`chat_output_filter` 推荐模型：**

- **Primary**：`openai/gpt-5.4-nano`——速度快，过滤后的输出稳定。
- **不要**使用 `openai/gpt-4.1-nano` 作为 filter 模型——根据实际测试，它会返回类似 `"对不起，无法满足你的要求"` 的拒绝文本和 HTTP 200；引擎无法将其与成功的过滤改写区分，因此 fail-open 路径不会触发，用户会看到拒绝文本。
- **推荐 fallback**：`google/gemini-3.1-flash`——成功率高；失败时会返回正确的错误响应（非 200），使引擎的 fail-open 路径生效并输出原始回复。
- **节省成本的 fallback**：`zhipuai/zlm-4.7-flash`——成本更低，失败模式与 gemini-3.1-flash 类似。
- **不要**使用 `anthropic/claude-haiku-4.5` 作为 filter——它对 NSFW 输入的容忍度（非常适合 extraction）并未延伸至输出；输出侧的安全对齐足够严格，导致 filter LLM 经常完全拒绝生成改写文本。

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `model` | `String` \| `Array` \| `Table` | — | Primary filter 模型。接受与 `chat_companion.model` 相同的三种形式。 |
| `fallback` | `String` \| `Array<String>` | — | filter 调用的 fallback 链。 |
| `retry_depth` | `u32` | `1` | filter 放弃前可尝试的 `fallback` 条目数。`0` = 仅 primary；`1` = primary + 第一个 fallback（默认）。 |
| `temperature` | `f64` | `defaults.fallback_temperature` | filter 模型的采样 temperature。**仅限任务级——无 per-tier 覆盖**（与其他所有任务相同）。 |
| `max_tokens` | `u32` | `defaults.fallback_max_tokens` | filter 响应的 token 上限。**仅限任务级——无 per-tier 覆盖。** |
| `filter_prompt` | `String` | — | **filter 生效的必要条件。** 发送给 filter 模型的 system/instruction prompt。空白或缺失 → filter 不生效。 |
| `trigger` | inline table | 缺失（每轮） | 决定何时应用 filter 的 AND 门控。省略整个 key 即过滤每个符合条件的轮次。 |
| `timing` | `"after_extract"` \| `"before_extract"` | `"after_extract"` | 控制 extract（memory/insight/affinity）读取原始文本还是过滤后文本（见下文）。 |

Per-tier 子表（`[tasks.chat_output_filter.tiers.<tier>]`）可以覆盖 `model`、`fallback`、`retry_depth`、`filter_prompt`、`trigger` 和 `timing`；tier 中省略的字段会回退到默认的 `[tasks.chat_output_filter]` 块。**`temperature` 和 `max_tokens` 仅限任务级**（per-tier 子表不覆盖它们——与其他所有任务相同）。

#### `trigger` 谓词

`trigger` 是可选的 inline table。设置的每个谓词都必须通过；省略的谓词视为通过。完全省略 `trigger`，即可过滤每个符合条件的轮次。

| 谓词 | 类型 | 语义 |
|---|---|---|
| `random` | `(0.0, 1.0]` 范围内的 `f64` | 当前轮次通过的概率。`random = 0.3` → 约 30% 的轮次会被过滤。 |
| `models` | `Array<String>` | 仅当生成回复的 model id 在列表中时，当前轮次才通过。 |
| `traits` | `{ any = [...], when = "present" \| "absent" }` | 仅当 `any` 中至少一个 tag 在**实际注入** prompt 的 tag 中存在（`when = "present"`）或不存在（`when = "absent"`）时，当前轮次才通过；这里指经过 tier `allow_traits` 门控后、与 `final` frame 的 `prompt_injected` 所报告内容相同的集合。被 tier 丢弃的 trait 不算存在。 |

#### `timing` 与 extract 行为

| `timing` | Extract 输入 | 说明 |
|---|---|---|
| `"after_extract"` *（默认）* | 原始（filter 前）文本 | Memory/insight/affinity 读取未修改的回复；仅改写后的文本会交付客户端并持久化到 `chat_messages`。 |
| `"before_extract"` | 过滤后文本 | Extract 也会读取改写后的文本。当 filter 对内容进行规范化且 extract pipeline 应反映该变化时使用。 |

**Fail-open：**如果 filter LLM 调用超时或返回错误，引擎会原样交付**原始**回复（filter 绝不会阻塞 chat 响应）。

#### 存储和显示的内容

只有**过滤后**文本会写入 `chat_messages` 并显示给客户端。当 `timing = "after_extract"`（默认）时，原始文本在内部供 extract 使用，随后被丢弃。因此历史重放显示的是过滤后的版本。

#### SSE `final` frame 字段

chat SSE stream 结束时发出的 `final` event 包含几个新字段。无论 filter 是否运行，这些字段始终会在 frame 发出时存在。

| 字段 | 类型 | 说明 |
|---|---|---|
| `filtered` | `bool` | 当前轮次客户端收到的是非原始输出时为 `true`——由 regex 过滤（`output_regex`）、LLM `output_filter` 或两者同时触发时置为 `true`；否则为 `false`。 |
| `retries_chat` | `u32` | chat 模型调用消耗的 fallback 重试次数（0 = primary 成功）。 |
| `retries_filter` | `u32` | filter 模型调用消耗的 fallback 重试次数（0 = primary 成功或 filter 未运行）。 |
| `prompt_injected` | `Array<String>` \| `null` | 当前轮次注入 prompt 的 trait tag；若无则为 `null`。与 filter 无关。 |
| `tier` | `String` \| `null` | 原样返回请求中的 `tier` 字段；若未发送则为 `null`。与 filter 无关。 |

### `output_regex` — 确定性 per-model 正则过滤（仅限 chat 任务）

`output_regex` 是 `[tasks.chat_companion]` 上的规则数组（仅限任务级——无 per-tier 覆盖）。每条规则对 `models` 中任意模型生成的助手回复进行正则匹配，删除或替换匹配内容。**默认关闭**（缺失或空数组均表示不过滤）。

```toml
[tasks.chat_companion]
output_regex = [
  # 在 reply_text_image 轮次中，去掉 L3.3-Euryale 自述的发图行。
  { models = ["sao10k/l3.3-euryale-70b"],
    pattern = '\s*\[你给对方发送了一张照片[：:][^\]]*\]\s*$' },
  # 替换而非删除（replacement 默认 "" = 删除）：
  # { models = ["x/y"], pattern = '...', replacement = "…" },
]
```

#### 规则结构

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `models` | `Array<String>` | 是 | 此规则适用的模型 id 列表。与生成回复的 chat 模型 id 进行精确字符串匹配——即行上的 `model` 列，而非 `filter_model`（过滤生效时 `filter_model` 被设为 `"<regex>"`）。 |
| `pattern` | `String` | 是 | Rust `regex` crate 正则表达式。**不支持 lookaround 或反向引用**——请使用 `$`、`^`、`\s*`、字符类等锚定。无效 pattern 会导致服务器启动失败。 |
| `replacement` | `String` | 否 | 替换每个匹配项的文本。缺失或 `""` = 删除匹配内容。 |

规则按声明顺序检查；所有匹配规则依次作用于同一条回复。

#### 执行顺序——第 0 层

Regex 过滤在所有其他处理之前运行：

1. Regex 过滤（第 0 层）——最先执行，客户端看到任何内容之前
2. LLM `output_filter`（如已启用）——第二轮处理
3. Memory / insight / affinity 提取——读取已过滤后的文本

因此，匹配到的文本**既不会到达客户端**，**也不会写入 `content`**，**更不会进入提取流水线**——与 `[tasks.chat_output_filter]` 的 `timing` 设置无关。

#### 审计列

| 列 | 过滤生效时的值 |
|---|---|
| `pre_filter_content` | 过滤前的原始回复 |
| `filter_model` | `"<regex>"` |

仅当至少一条规则实际改变了回复时才会设置这些列（与 LLM filter 行为一致——无变更的过滤不会设置这些列）。

#### 空结果 fail-safe

若某次过滤会将非空回复变为空字符串，则该次过滤为**空操作**——原始回复原样交付，审计列不被设置。此机制防止过于宽泛的 pattern 让伴侣陷入沉默。

#### `filtered` 标志

SSE `final` frame 的 `filtered` 字段在客户端收到的是非原始输出时为 `true`——由 **regex 过滤**、LLM `output_filter` 或两者同时触发均会置为 `true`。

### `input_filter` — 用户输入改写（仅限 chat 任务）

`input_filter` 是 `[tasks.chat_companion]` 上的 trigger（默认 `false`，仅限任务级——无 per-tier 覆盖）。它接受 **bool 或概率值**：`false` = 关闭，`true` = 每轮执行（= `1.0`），`0.8` = 每轮抛硬币，约 80% 的轮次触发。超出 `[0.0, 1.0]` 的数字（或非有限数）会在配置加载时被拒绝。当用户的 **Reply** 轮触发时，该轮输入会在生成之前交给第二个 LLM（`[tasks.chat_input_filter]`）。filter 返回 JSON verdict：

- `{"rewrite": false}`——输入有意义；引擎原样使用。
- `{"rewrite": true, "content": "…", "reason": "…"}`——输入无意义（例如 `1111`、`？？？`、乱按键盘）；引擎改用 `content`。

用户的**原始**文本始终作为 `content` 持久化并显示给客户端。改写内容存储在 `pre_filter_content`（仅供模型使用）、`filter_model`、`f_generation_id` 和 `filter_triggers = {"reason": …}` 中。对于用户记录，模型和 memory recall 读取有效文本（`pre_filter_content ?? content`）；extraction（insight/memory/affinity）仍读取原始文本。

仅当 `input_filter` 触发（值为 `true`，或当前轮次的概率抽取通过）并且 `[tasks.chat_input_filter]` 存在且 `filter_prompt` 非空白时，filter 才会运行。它采用 **fail-open**：任何错误、超时、无法解析的 verdict 或拒绝都会保留原始输入不变。请选择快速、低成本的模型——当 `input_filter = true` 时，每个用户轮次都会在生成前运行该模型。

#### `[tasks.chat_input_filter]` 字段

复用标准任务结构：`model`、`fallback`、`retry_depth`（默认 1）、`temperature`、`max_tokens`、`filter_prompt`、`reasoning`（示例中默认关闭）。`trigger`、`timing`、`tiers` 和 `allow_traits` 会被忽略（input filter 没有 trigger、timing 或 tier）。

## 任务名

| 名称 | 使用方 | 状态 |
|---|---|---|
| `chat_companion` | `pipeline::handlers::ReplyHandler`（chat completion；tip 轮次使用相同的 reply 路径） | live |
| `insight_extraction` | `pipeline::post_process::extract_facts` 和 `extract_structured_insights`（事实挖掘 + JSONB 合并） | live |
| `chat_output_filter` | `pipeline::handlers::ReplyHandler`（交付前对 chat 回复进行可选的二次改写） | live |
| `pde_decision` | `pipeline::stream`（通过 `run_pde_decision` 实现的 opt-in LLM 判断器，由 `run_stream` 调用；缺少 `filter_prompt` 或 LLM 调用失败时使用规则引擎） | live（opt-in） |
| `chat_image_prompt_compose` | `pipeline::stream`（opt-in 图片提示词改写器；在图片生成前扩写 PDE 的种子主题；存在此任务块时激活） | live（opt-in） |
| `chat_vision` | `pipeline::stream`，通过 `resolve_vision()`（视觉预处理阶段：在 reply prompt 前将 `image_url` 附件描述为 JSON；任务块缺失或 `filter_prompt` 为空白时关闭） | live（opt-in） |
| `chat_product_qa` | `pipeline::stream`，通过 `resolve_product_qa()`（PDE `product_qa` 动作的出戏产品问答执行器；任务块缺失或 `filter_prompt` 为空白时关闭；还需要 LLM PDE 已启用） | live（opt-in） |
| `affinity_evaluation` | `pipeline::post_process`（每轮六轴 affinity delta；每个 Reply 轮次后以 fire-and-forget 方式运行） | live |
| `memory_extraction` | dreaming sweeper（会话结束时进行 memory 整合；任务块缺失时关闭） | live（opt-in） |
| `chat_input_filter` | `pipeline::stream`（用户输入改写 filter；由 `[tasks.chat_companion]` 上的 `input_filter` 和此任务块共同激活；默认关闭） | live（opt-in） |
| `embedding` | 启动时的 `EmbeddingRouter::from_config()`（`main.rs`），经由 `ModelConfig::resolve_embedding()`——把 `embed_query`/`embed_document`/`embed_documents` 路由到原生 Voyage、内置 OpenRouter embeddings 端点、或某个 `[providers]` 条目；块缺失 = 原生 Voyage `voyage-4-lite` | live |

只有当引擎确实在某处调用 `model_config.resolve("<name>", ...)` 时，`[tasks.<name>]` 条目才有意义。当前调用点如下：

- `crates/eros-engine-server/src/pipeline/handlers.rs` → `chat_companion`、`chat_output_filter`
- `crates/eros-engine-server/src/pipeline/post_process.rs` → `insight_extraction`、`affinity_evaluation`
- `crates/eros-engine-server/src/pipeline/stream.rs` → `pde_decision`，通过 `run_stream` 内的 `run_pde_decision`（仅当设置了 `filter_prompt`）；`chat_image_prompt_compose`，通过 `resolve_image_prompt_compose()`（图片提示词改写器，opt-in，仅在图片轮次按需解析）；`chat_vision`，通过 `resolve_vision()`（视觉预处理阶段，opt-in）；`chat_product_qa`，通过 `resolve_product_qa()`（产品问答执行器，opt-in）；`chat_input_filter`，通过 `resolve_input_filter()`（输入改写，opt-in）；`memory_extraction`，通过 dreaming sweeper

`embedding` 不走上面这条通用 `resolve()` 路径——它有自己的解析器
`ModelConfig::resolve_embedding()`，在 `main.rs` 启动时调用一次来构建
`EmbeddingRouter`。见下文“`[tasks.embedding]` — 已激活”。

### `[tasks.pde_decision]` — opt-in LLM PDE 判断器

默认情况下，引擎使用内置规则引擎（`eros-engine-core/src/pde.rs`）决定每轮动作（reply / ghost / proactive）。在此块中设置 `filter_prompt` 会启用 LLM 判断器：

- LLM 接收最近的对话、关系状态和对话信号，并返回 JSON verdict，其中包含：
  - `action`：`"reply_text"` \| `"ghost"` \| `"reply_image"` \| `"reply_text_image"` \| `"product_qa"`（请求包含 `image` 块时图片变体才可用——调用方以此声明本轮由自己处理图片；否则降级为 `reply_text`。聊天流从不绘图——只发出 `image_request` 帧，由调用方自行调用图像供应商。`product_qa` 仅当 `[tasks.chat_product_qa]` 完全启用时才可用——见下文；不可用时降级为 `reply_text`，绝不升级。）
  - `inner_state`：融入 reply prompt 的简短情绪/语气描述
  - `tone`（选填）：这一轮回复该用的语气/口吻，一句话——在文本类动作上注入 reply prompt 的 `[reply_tone]` 区块；缺省则不注入
  - `image_prompt`、`reason`：可选
- **Fail-open：**任何 LLM 超时或错误都会回退到规则引擎——LLM 判断器绝不会阻塞 chat 响应。
- **硬安全 guardrail**（在 LLM verdict 之后、规则引擎 fallback 之前强制执行）：前 10 条消息绝不 ghost，绝不连续 ghost 两次，ghost cooldown 为一小时。
- 每次判断器调用都会记录到 `companion_decision_events` 以供审计。

**图片能力上下文行。** 判断器上下文每轮必带一行——当本轮图片动作可用（请求带有 `image` 块）时为 `[图片能力] 本轮可发图=是`，否则为 `[图片能力] 本轮可发图=否`。prompt 作者应把 `本轮可发图=否` 当作硬约束（绝不要选 `reply_image` / `reply_text_image`——它们会被 `guard_action` 降级，白费 token 还会污染审计），把 `本轮可发图=是` 当作*允许*发图的开关，再按人格/语境决定要不要发（引擎不会因为"能发"就强制发图）。若下游 overlay 引用了这个 token，请逐字保留 `[图片能力] 本轮可发图=是/否`。

**`ghosting` 字段**（bool，默认 `true`）：面向下游产品的安全开关。设置 `ghosting = false` 可在*整个* PDE 路径上禁用 ghosting——包括 LLM verdict、规则 fallback 和纯规则引擎——从而确保 companion 永不沉默。适用于不希望出现静默轮次的产品。

### `[tasks.chat_image_prompt_compose]` — 图片提示词改写器（opt-in）

PDE 在选动作、定 `inner_state` 的同时，还要在很紧的 token 预算里写一个简短的种子 `image_prompt`。配置此任务块后，引擎会在**决定出图之后、生成之前**单独跑一次改写器：把人格外观、最近场景、PDE 种子主题、所选 style 和目标宽高比交给该模型，用扩写后的结果作为图片主体（随 `image_request` 帧的 `composed_prompt` 下发；持久化的 `metadata.image.prompt` 标记保留 PDE 种子提示词）。PDE 的原始种子单独保留在决策审计里。

该功能**默认关闭**，仅当此块存在时激活。它是 **fail-open** 的：改写器失败 / 超时 / 输出为空时，引擎回退到 PDE 种子原值，绝不阻塞或失败图片轮次。该任务**仅在图片轮次按需解析**，因此不会在文本/ghost 轮次推进 `model` 的 round-robin 游标。

```toml
[tasks.chat_image_prompt_compose]
model        = "x-ai/grok-4"                       # 任意文本模型；选一个能覆盖你内容范围的
fallback     = ["google/gemini-3.1-flash-lite"]
retry_depth  = 1
temperature  = 0.7
max_tokens   = 700
# reasoning  = { enabled = false }
# filter_prompt 可选——省略则用内置默认（见下）：
# filter_prompt = """…覆盖…"""
```

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `model` | `ModelSpec`（字符串 \| 数组 \| 表） | 缺失 | 改写器模型（与 `chat_companion.model` 同样三种形态）。 |
| `fallback` | `String` \| `Array<String>` | `[]` | 顺序重试链（FallbackSpec），按 `retry_depth` 截断。 |
| `retry_depth` | `u32` | `1` | primary + 这么多个 fallback。 |
| `temperature` | `f64` | 任务默认 | 改写器调用的采样温度。 |
| `max_tokens` | `u32` | 任务默认 | 改写器调用的 token 上限。 |
| `reasoning` | 表 | 缺失 | 可选 reasoning 控制，转发给 OpenRouter。 |
| `filter_prompt` | `String` \| `Array<String>` \| `Table<String, String>` | **内置默认** | **可选**（不同于其他任务）。空白/缺失 ⇒ 引擎内置的 `DEFAULT_COMPOSE_PROMPT`。数组/表两种形态见下方"变体"。 |

**内置默认提示词。** 不同于其他每个任务（提示词必须来自配置），改写器自带一个通用、**不含产品身份**的内置默认。它的立场是**只扩写**：忠实地把种子主题和外观扩成详细提示词，**不**软化、不降级、不审查、不拒绝，也不自带内容审核层——审核交给画图模型供应商和下游部署，不在这一步。非空 `filter_prompt` 覆盖它；空白/缺失则回退到它。

**变体（variants）。** 这是**唯一**一个 `filter_prompt` 接受纯字符串以外形态的任务。前端在每个 chat 轮次通过发消息请求体上的 `image.prompt_variant` 挑一个。三种形态：

```toml
filter_prompt = "…"                       # 单一提示词；prompt_variant 被忽略
filter_prompt = ["…", "…"]                # 按下标选：prompt_variant = "0" / "1"
filter_prompt = { a = "…", b = "…" }      # 按 key 选：  prompt_variant = "a" / "b"
```

改写器提示词通常又长又多段，而 TOML 1.0.0 本就不建议把 inline table 拆成多行——所以按
key 选的形态也可以写成标准（非 inline）表，一个 key 一个 section：

```toml
[tasks.chat_image_prompt_compose.filter_prompt]
a = """
…较长的提示词…
"""
b = """
…另一个较长的提示词…
"""
```

没有命中都会回退到上面的**内置**提示词，不存在"取第一项"这种行为——但两种"没命中"的记录方式不同。**没传** `prompt_variant` 属于静默回退（最常见的情况：既然没人要求选变体，也就没什么好警告的）。**明确传了但没命中**的变体——下标越界、key 不存在——同样回退，但会额外记一条 `warn` 日志，因为调用方指定了变体却没用上，值得被看到。也没有保留的 `default` key：写 `default = "…"` 只是定义了一个普通变体，只有字面量 `prompt_variant = "default"` 才会命中它。

`prompt_variant = "raw"`（大小写不敏感）会完全跳过改写器 LLM，直接用种子主题原样出图——这一轮少一次 LLM 调用。`raw` 是保留字：表里若出现字面量为 `raw` 的 key（任意大小写）会拒绝启动。

变体只在这一个任务上生效。任何其他任务的数组/表形态 `filter_prompt`，或**任何** `[tasks.*.tiers.*]` 块（包括本任务自己的 tiers——改写器解析时永远不走 tier）里的数组/表形态，都会拒绝启动，而不是留在那里永远选不到。

调用点：`crates/eros-engine-server/src/pipeline/stream.rs`，通过 `model_config.rs` 中的 `resolve_image_prompt_compose()`。

### `[tasks.chat_vision]` — 图片输入（视觉预处理阶段，opt-in）

当 chat 轮次携带 `image_url` 时，引擎运行 `resolve_vision()` 获取支持视觉的模型和 `filter_prompt`，调用该模型将图片描述为固定 JSON schema（`description`、`ocr_text`、`people`、`scene`），并在主 chat 调用前将结果融入面向用户的 prompt。主 `chat_companion` 模型仍然只处理文本。

此功能**默认关闭**，仅当此任务块存在且 `filter_prompt` 非空白时激活。`retry_depth` 默认为 `1`（primary + 第一个 fallback）。请选择支持视觉的模型；示例使用 `google/gemini-3.1-flash-lite`。

调用点：`crates/eros-engine-server/src/pipeline/stream.rs`，通过 `model_config.rs` 中的 `resolve_vision()`。

### `[tasks.chat_product_qa]` — 出戏产品问答（opt-in）

为 PDE 判断器的 `product_qa` 动作供电：当终端用户问及下游产品本身（"这个 app
是什么？""怎么收费？""会员怎么取消？"）时，判断器把这一轮路由到此任务自己的
模型链，而不是 `chat_companion`。`filter_prompt`（产品资料 + 作答规则）就是
执行器**完整**的 system prompt——不注入任何人格，伴侣在这一轮完全出戏。

**三重启用门槛**——必须同时满足（`resolve_product_qa()` /
`validate_product_qa_prompt()`）：

| 门槛 | 状态 | 行为 |
| --- | --- | --- |
| `[tasks.pde_decision].filter_prompt` 已设置 | 关闭 | 规则引擎永不产生 `product_qa`——没有 LLM 判断器这个动作就不可达。若此时仍配置了 `[tasks.chat_product_qa]`，启动时记录一条 WARN（日志原文为英文，按原样 grep）：`"model_config: [tasks.chat_product_qa] is configured but the LLM PDE ([tasks.pde_decision].filter_prompt) is disabled — product_qa is inert"`，该块保持不生效。 |
| `[tasks.chat_product_qa]` 块存在 | 缺失 | 功能关闭。判断器上下文不带任何产品问答相关行；幻觉出的 `product_qa` verdict 会降级为 `reply_text`。 |
| `chat_product_qa.filter_prompt` 非空白 | 空白 | **拒绝启动**——与 `insight_extraction` / `memory_extraction` 相同的必填 prompt 契约。请设置 prompt，或直接删除 `[tasks.chat_product_qa]` 整个章节以关闭该功能。 |

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

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `model` | `ModelSpec`（字符串 \| 数组 \| 表） | — | 执行器主模型（与 `chat_companion.model` 同样三种形态）。 |
| `fallback` | `String` \| `Array<String>` | `[]` | 顺序重试链（FallbackSpec），按 `retry_depth` 截断。 |
| `retry_depth` | `u32` | `1` | primary + 这么多个 fallback。 |
| `temperature` | `f64` | 任务默认 | 执行器调用的采样温度。 |
| `max_tokens` | `u32` | 任务默认 | 执行器调用的 token 上限。 |
| `reasoning` | 表 | 缺失 | 可选 reasoning 控制，转发给 OpenRouter。 |
| `filter_prompt` | `String` | — | **必填。** 产品资料 + 作答规则；空白或缺失会拒绝启动（见上方门槛表）。 |

**判断器侧上下文。** 仅当三重门槛全部满足时，判断器上下文（`build_pde_ctx`）
才会多出两行供 prompt 使用：

- `[产品咨询] 本轮可答产品问题=是`——可用性行，仅在任务启用时才渲染（功能
  关闭的部署零 prompt 漂移、零 token 成本）。
- `[最近产品咨询]`——本 session 最近 **3** 条产品问答配对
  （`channel='product_qa'`），让省略主语的追问（"那多少钱一个月？"）依然能
  正确路由到 `product_qa`。没有历史配对时省略该块。同样这 3 条配对会复用
  作为执行器自己的对话上下文——不会二次查库。

**隔离语义。** 答案以正常 assistant 行落库（`role='assistant'`、
`assistant_action_type='reply'`），并打上 `channel='product_qa'` 标记。该
标记让这一行对伴侣大脑不可见：短期回忆（`recent_turn_pairs` /
`recent_turn_pairs_before_message` / `recent_assistant_contents`）、对话信号
（`compute_signals_for_session`）、dreaming sweeper 的会话日志拉取、判断器
自己共用的伴侣 transcript（`build_input_filter_transcript`），以及伴侣的
实时消息窗口（`assemble_chat_request`），都会过滤掉 `channel IS NOT NULL`
的行——同时这一行在实时 SSE 流、断线重放和客户端历史记录里完全可见
（两个历史投影都会带出 `channel`）。完整的 `chat_messages.channel` 语义见
[architecture.zh.md](architecture.zh.md)。

**失败兜底。** 若执行器的整条候选链（`model` + `fallback`）耗尽仍未流出任何
内容，引擎**不会**降级到人格内的伴侣回复——伴侣并不知道产品事实，临场发挥
正是这个功能要杜绝的事。取而代之，引擎会挑一条配置好的 `error_handling`
兜底话术（与聊天流其他地方共用的同一套 DB 驱动机制），并**带上
`channel='product_qa'` 标记**落库，让重放/幂等性依然成立；若没有配置兜底
话术，则以 `Error` 帧结束该轮。

调用点：`crates/eros-engine-server/src/pipeline/stream.rs`，通过
`model_config.rs` 中的 `resolve_product_qa()`。

### `[tasks.embedding]` — 已激活

`VoyageClient` 以前 hard-code `voyage-3-lite`；现在 `[tasks.embedding]`
真正被消费，embedding 模型本身、以及它路由到哪个后端，都由配置驱动。

```toml
# 单模型——read 和 write 用同一个后端。
[tasks.embedding]
model = "voyage-4-lite"                       # ≡ "voyage-4-lite@voyage"
# model = "voyage-3-lite"                     # 遗留写法显式钉住（Voyage 官方已不再推荐）
# model = "openai/text-embedding-3-small@openrouter"
# model = "bge-m3@local"                      # 第三方，OpenRouter 兼容 wire

# 或者：拆分 read/write——仅限 voyage-4 系列及以上。
#[tasks.embedding]
#model_read  = "voyage-4-lite"   # 召回路径：embed_query，input_type "query"
#model_write = "voyage-4"        # 存储路径：embed_document(s)，input_type "document"
```

**维度固定为 512，没有配置开关。** pgvector 的三个列都是
`VECTOR(512) NOT NULL`，client 在 wire 上请求的也是 512，且每次响应都会
做长度校验。以前 `[tasks.<name>]` 上有一个 `dimensions` 字段——它从未被
消费，现已移除；已有配置里残留的 `dimensions = 512` 行，现在只是一个被
静默忽略的未知 key（和其他过期 key 一样，serde 直接忽略）。

| 字段 | 类型 | 规则 |
|---|---|---|
| `model` | 单个固定字符串 | 裸 id ⇒ `@voyage`；`@openrouter` / `@<custom>` 路由到 OpenRouter 兼容 wire；与 read/write 对互斥 |
| `model_read` | `Option<String>` | 仅限 read/write 对使用，仅限 Voyage，N ≥ 4（见下方的 gate）；服务 `embed_query` |
| `model_write` | `Option<String>` | 仅限 read/write 对使用，仅限 Voyage，N ≥ 4；服务 `embed_document` / `embed_documents` |

按后缀路由，由 `ModelConfig::resolve_embedding()` 解析：

| 裸 id（无后缀） | `@openrouter` | `@voyage` | `@<custom>` |
|---|---|---|---|
| 原生 Voyage | 内置 OpenRouter embeddings 端点（可覆盖，见上文 `[providers].openrouter`） | 同裸 id | `[providers].<name>.embeddings`（OpenRouter 兼容 wire） |

- `model_read` / `model_write` 是普通的 `Option<String>`——数组/表形态在
  解析时就是类型错误。`model_read = model_write` 合法（虽然多余但无害——
  等价于 `model`）。`@openrouter` 和 `@<custom>` 在 `model_read`/
  `model_write` 上会拒绝启动（只有 Voyage 能保证跨模型体量共享一个向量
  空间）。
- `[tasks.embedding]` **缺失** ⇒ 原生 Voyage + `voyage-4-lite`（wire 上带
  `output_dimension: 512`）。Voyage 官方已不再推荐 `voyage-3-lite`，仍要
  用它的部署必须在配置里显式钉住。在已有数据上换模型会切换向量空间——
  旧行与新查询不可比——所以要么钉住旧模型，要么整体重嵌入。
- `model` 必须是单个固定字符串；round-robin、weighted、`fallback`、
  `tiers` 全部拒绝启动（否则会产生混合/不兼容的向量空间——沿用
  `chat_voice` 仅固定字符串的先例）。
- wire 上没有 `input_type`：query/document 的优化是 Voyage 原生的特性。
  把 embedding 路由离开 Voyage 就放弃了这个优化。

**voyage-4 gate。** 应用于剥掉可选 `@voyage` 后缀之后的裸 id。该 id 必须
以 `voyage-` 开头，后接一个数字段（只能是 ASCII 数字和点号，到下一个 `-`
或字符串末尾为止），且该数字段能解析为一个 ≥ 4 的有限数：

- ✓ `voyage-4`、`voyage-4-lite`、`voyage-4.5-large`、`voyage-10`
- ✗ `voyage-3.5-lite`（N = 3.5）、`voyage-code-3`（`voyage-` 后面没有
  紧跟数字段）、`voyage-inf`/`voyage-nan`（含非数字字符，且即便解析出来
  也不是有限数）、`bge-m3@local`（不是 voyage）、任何 `@openrouter` 或
  自定义 provider 的 slug

只有 voyage-4 系列及以上才能保证跨模型体量共享同一个向量空间——把一个
更低版本或非数字的模型混进 read/write 对，会静默写入 read 模型无法比较
的向量，所以这里是启动拒绝，不只是文档脚注。

**`VOYAGE_API_KEY`** 仅当解析出的 read 或 write 后端是 Voyage 时才要求
（块缺失 ⇒ 默认走 Voyage ⇒ 依然要求，现有部署行为不变）。把 read 和
write 全部路由离开 Voyage 的部署不再需要这个变量。

调用点：`crates/eros-engine-server/src/main.rs` 在启动时构建一次
`eros_engine_llm::embedding::EmbeddingRouter::from_config(&model_config)`；
`AppState.embed: Arc<EmbeddingRouter>` 为 `handlers.rs` / `post_process.rs`
/ `dreaming.rs` / `world.rs` / `story.rs` 里的 `embed_query` /
`embed_document` / `embed_documents` 提供服务，调用形态不变。

### 启用/禁用 extraction

`insight_extraction`（每轮事实挖掘）和 `memory_extraction`（会话结束时的 dreaming sweeper）由其 `[tasks.*_extraction]` **章节是否存在**控制：

- **章节存在** → `filter_prompt` **必填**；若为空白或缺失，服务器会拒绝启动。
- **章节缺失** → 该 extraction **关闭**。引擎可以正常启动和运行（每轮跳过 `insight_extraction`；dreaming sweeper 保持不生效）。

> **行为变更（0.6.x）：**早期版本要求两个章节都必须存在（缺少章节会导致启动失败）。现在可以通过省略章节来关闭。随附的 `examples/model_config.toml` 仍然保留两个章节，因此默认行为——同时启用两种 extraction——没有变化。

`reasoning` 的行为与其他所有任务相同——省略则由模型决定；`reasoning = { enabled = false }` 强制关闭 reasoning；`{ enabled = true }` 强制开启。

## 解析规则

对于 `model` 和 `fallback`：

```
matched tier block > task default block > [defaults] > compiled-in fallback
```

对于 `allow_traits`：

```
matched tier block > task default block
```

对于 `temperature` 和 `max_tokens`：

```
task default block > [defaults] > compiled-in fallback
```

各层级的含义如下：

- **匹配的 tier 块**——`[tasks.<name>.tiers.<tier>]`，其中 `<tier>` 来自 chat 请求的 `tier` 字段（正则 `^[a-z0-9_]{1,32}$`）。如果请求的 tier 缺失或未知（没有匹配的子表），则使用任务默认块，并发出 `tracing::warn!`。
- **任务默认块**——`[tasks.<name>]`。
- **`[defaults]`**——顶层 defaults 块。
- **编译时内置 fallback**——`x-ai/grok-4-mini`、temperature `0.5`、max_tokens `200`。在 `model_config.rs` 中 hard-code。

`temperature` 和 `max_tokens` 仅限任务级——per-tier 子表不会覆盖它们。

如果以未知任务名调用 `resolve()`，它会按 `defaults → 编译时内置` 回退，并发出 `tracing::warn!`（`"model_config: unknown task, using defaults"`）。

## 主模型选择

`model`（任务级和 per-tier）接受三种形式：

```toml
model = "x-ai/grok-4.20"                              # fixed
model = ["x-ai/grok-4.20", "z-ai/glm-4.7-flash"]     # round-robin (deterministic)
model = { "x-ai/grok-4.20" = 0.8, "z-ai/glm-4.7-flash" = 0.2 }  # weighted random
```

- **Round-robin** 在各次调用间进行确定性交替（每进程计数器；重启时重置；每个 replica 独立计数）。
- **Weighted** 随机抽取；权重可以是任意正数，并按总和归一化（`{a = 8, b = 2}` == `{a = 0.8, b = 0.2}`）。非正权重会被丢弃。
- `["a","b"]` 和 `{a = 1, b = 1}` 会产生相同的长期分布，但机制不同（确定性与随机）。
- 单条目数组/表的行为与固定字符串相同。空数组/表会回退到下一个优先级层级。

**TOML 注意事项：**inline table 的 key 只允许 `A-Za-z0-9_-`，但 model id 包含 `/` 和 `.`，因此 weighted key **必须加引号**：`{ "x-ai/grok-4.20" = 0.8 }`。数组形式无需特殊处理。

### Fallback 去重

选择 primary 后，解析出的 `fallback` 链中与其 id 完全相同的条目会被删除——重试刚刚失败的模型毫无意义。对于 round-robin/weighted primary，这是动态行为：只删除当前调用所选的 id。

## 稳定性承诺（OSS 0.x）

在 `0.x` 期间，OSS 引擎承诺：

1. **不删除字段。** `[defaults]` 和 `[tasks.<name>]` 中现有的字段名不会消失。
2. **不重命名字段。** `fallback` 不会变为 `fallback_model`，`model` 不会变为 `primary_model`，以此类推。
3. **不新增必填字段。** 任何新增字段都是可选的，并具有合理默认值。
4. **不从此列表中删除任务名：**`chat_companion`、`insight_extraction`、`pde_decision`、`embedding`。
5. **解析优先级固定。** 对于 `model`/`fallback`/`allow_traits`，优先级为 `matched tier > task default block > [defaults] > compiled-in fallback`。`temperature`/`max_tokens` 仅限任务级。
6. **`model` 接受字符串、数组（round-robin）或表（weighted）。** 普通字符串将始终有效；数组/表形式属于扩展能力。

以下内容仍可能在不另行通知的情况下改变：

- 编译时内置 fallback 值（目前为 `x-ai/grok-4-mini` / `0.5` / `200`）。这些是 fail-safe，而非 contract。
- 如果添加 `#[non_exhaustive]`，`eros-engine-llm` 内部 struct 的形态可能改变。
- `description` 字段的处理方式——目前用于文档，将来可能成为结构化 metadata。
- *未来*新增的可选字段和本文档范围之外的新任务名。（上文记录的字段——包括 `allow_traits` 和 `tiers`——受承诺 1–3 保护。）

### Changelog 说明

- **从此版本开始，引擎不再读取 `persona_override`（`art_metadata.model`）。** 请改用 `[tasks.<name>.tiers.<tier>]` 进行 per-tier 模型选择。persona 的 JSONB `art_metadata` 中可能仍存在 `model` 字段，但会被静默忽略。
- `model_name_display_override`（可选，位于 `[tasks.chat_companion]`）：在 0.x 中新增。未设置时会**省略** chat `meta.model` 字段——这与早期“始终存在”的行为不同。随附的示例设置为 `= true`，以继续显示真实 id。
- `output_filter`（可选 bool，位于 `[tasks.chat_companion]` 和 per-tier 中）：在 0.x 中新增。默认为 `false`。通过 `[tasks.chat_output_filter]` 启用二次回复改写。
- `[tasks.chat_output_filter]`（新任务）：在 0.x 中新增。默认缺失（filter 不生效）。参见上文“`output_filter` — 二次回复改写”。
- SSE `final` frame 字段 `filtered`、`retries_chat`、`retries_filter`、`prompt_injected`、`tier`：在 0.x 中新增。
- `output_regex`（可选数组，位于 `[tasks.chat_companion]`）：在 0.x 中新增。仅限任务级（无 per-tier 覆盖）。在客户端看到回复之前、LLM `output_filter` 之前、提取之前应用的确定性 regex 过滤。regex 过滤或 LLM filter（或两者）产生非原始输出时，`filtered` 标志均为 `true`。参见上文"`output_regex` — 确定性 per-model 正则过滤"。
- **`[tasks.embedding]` 现已激活**（此前是保留、未被消费的占位）。随激活
  一起打包的破坏性变更：`[providers]` 的值现在是表，不再是纯字符串（字符串
  写法会被拒绝、无兼容层）；`[defaults].ignore_providers` 的条目现在必须带
  `@openrouter` 后缀；`OPENROUTER_BASE_URL` 环境变量已移除（改用
  `[providers].openrouter.chat`/`.embeddings`）；`OPENROUTER_APP_*` 环境变量
  软废弃（静默忽略，改用 `[providers].openrouter.headers`）；
  `[tasks.<name>]` 上的 `dimensions` 字段已移除（维度固定为 512；已有配置
  里残留的 `dimensions = 512` 行现在是被忽略的未知 key）。详见上文
  “`[providers]`”和“`[tasks.embedding]` — 已激活”。

## 此配置不控制的内容

- **Voyage 自己的 base URL**——原生 Voyage wire 始终打到 Voyage 的官方端点；只有模型 id 可以配置，通过 `[tasks.embedding]`。要完全绕开 Voyage，改用 `@openrouter` 或自定义 `[providers]` 后缀。
- **PDE 决策（默认路径）**——未设置 `filter_prompt` 时，`eros-engine-core/src/pde.rs` 中的规则引擎无条件运行。设置 `[tasks.pde_decision].filter_prompt` 可激活 opt-in LLM 判断器；此时规则引擎充当 fallback + 硬安全 guardrail。
- **OpenRouter API key**——直接从 `OPENROUTER_API_KEY` 读取，而非从配置文件读取。
- **每次调用的 streaming / response format 标志**——在 `OpenRouterClient` 中固定。

## 完整示例：基于 tier 的解析

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

请求携带 `"tier": "gold"` 时，`resolve("chat_companion", "gold")` 返回：

| 字段 | 值 | 来源 |
|---|---|---|
| `model` | `x-ai/grok-4.20` | `tiers.gold` |
| `fallback` | `["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3"]` | `tiers.gold` |
| `allow_traits` | `["allow_nsfw", "allow_politics"]` | `tiers.gold` |
| `temperature` | `0.8` | 任务默认块（无 tier 覆盖） |
| `max_tokens` | `1200` | 任务默认块（无 tier 覆盖） |

请求携带 `"tier": "free"` 时：

| 字段 | 值 | 来源 |
|---|---|---|
| `model` | `qwen/qwen3.6-flash` | `tiers.free` |
| `fallback` | `["deepseek/deepseek-v4-flash"]` | `tiers.free` |
| `allow_traits` | `["allow_politics"]` | `tiers.free` |
| `temperature` | `0.8` | 任务默认块 |
| `max_tokens` | `1200` | 任务默认块 |

未发送 `tier`（或发送未知 tier）时，所有字段都从任务默认块解析。

## 兼容性测试 fixture

`model_config.rs` 包含一个 fixture，用于断言代表性 TOML 的每个字段都能正确 round-trip。任何破坏 schema 的变更都会在发布前导致 CI 失败。参见 `crates/eros-engine-llm/src/model_config.rs` 中的 `compat_fixture_locks_full_schema`。
