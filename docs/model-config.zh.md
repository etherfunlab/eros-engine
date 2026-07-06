# 模型配置

[English](model-config.md) · [中文](model-config.zh.md)

引擎的 LLM 模型选择配置位于服务器启动时加载的 TOML 文件中。它为每个任务配置模型和参数，并可在其上添加可选的 per-tier 覆盖。

## 文件位置

- 默认路径：`examples/model_config.toml`（相对于工作目录）。`examples/` 下的文件是示例模板——请根据自己的需求调整（或通过 `MODEL_CONFIG_PATH` 指向自己的文件）。
- 覆盖方式：`MODEL_CONFIG_PATH` 环境变量
- 服务器启动时由 `eros-engine-server/src/main.rs` 加载一次（直接读取 `MODEL_CONFIG_PATH`，然后调用 `ModelConfig::from_toml_str`）。对于嵌入该库的应用，`crates/eros-engine-llm/src/model_config.rs` 中的 `ModelConfig::load()` 以相同方式加载，并使用相同的默认路径（`examples/model_config.toml`）。
- 以 `Arc<ModelConfig>` 保存在 `AppState` 中；由所有 chat / post-process 轮次共享
- 服务器启动时还会调用 `dotenvy::dotenv()`，因此快速入门中执行 `cp .env.example .env` 后无需显式执行 `source .env`

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

字段详情：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `defaults.fallback_model` | `String` | 否 | 任务配置未提供 model 时使用的最终 fallback。若仍然缺失，代码使用编译时内置默认值 `x-ai/grok-4-mini`。 |
| `defaults.fallback_temperature` | `f64` | 否 | 优先级相同；编译时内置默认值为 `0.5`。 |
| `defaults.fallback_max_tokens` | `u32` | 否 | 优先级相同；编译时内置默认值为 `200`。 |
| `defaults.ignore_providers` | `Array<String>` | 否 | 要从**每个**任务的路由中排除的 OpenRouter provider slug。每次对外调用时作为 `provider.ignore` 发送；`allow_fallbacks` 仍为 `true`，因此模型仍可由任意健康的 provider 提供。某个 provider 为模型返回乱码时（例如未解码的 byte-BPE 文本——issue #84），可使用此字段。通过 OpenRouter generation API 查找有问题的 slug。为空或缺失表示不排除任何 provider。 |
| `tasks.<name>.model` | `String` \| `Array<String>` \| `Table<String,f64>` | 是 | 主模型。字符串 = 固定；数组 = round-robin；表 = weighted 随机。参见“主模型选择”。 |
| `tasks.<name>.fallback` | `String` | 否 | 主调用失败时由 `OpenRouterClient` 使用的次要模型。 |
| `tasks.<name>.temperature` | `f64` | 否 | 每任务的采样 temperature。无 per-tier 覆盖。 |
| `tasks.<name>.max_tokens` | `u32` | 否 | 每任务的 token 上限。无 per-tier 覆盖。 |
| `tasks.<name>.allow_traits` | `Array<String>` | 否 | 此任务的 prompt-trait allow-list（三态：缺失 = 不设门控；`[]` = 丢弃所有 trait；`["a","b"]` = 白名单）。找不到匹配的 tier 块时使用。 |
| `tasks.<name>.tiers.<tier>` | 子表 | 否 | Per-tier 覆盖。可设置 `model`、`fallback` 和/或 `allow_traits`。不覆盖 `temperature` 或 `max_tokens`。 |
| `tasks.chat_companion.input_filter` | `bool` \| `f64` | 否 | 用户输入改写 filter 的全局 trigger。仅可在 `chat_companion` 的任务级配置中设置（无 per-tier 覆盖）。`false`/缺失 = 关闭，`true` = 每轮执行，`0.8` = 约 80% 的轮次执行（超出 `[0.0, 1.0]` 的数字会被拒绝）。参见“`input_filter`”。 |
| `tasks.<name>.description` | `String` | 否 | 文档字段，代码忽略。 |
| `tasks.<name>.dimensions` | `u32` | 否 | 仅用于 embedding。chat / insight 任务会忽略。 |

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
| `chat_image_generation` | `pipeline::stream`（opt-in 引擎侧绘图执行器——绘图端点；存在此任务块时激活） | live（opt-in） |
| `chat_image_prompt_compose` | `pipeline::stream`（opt-in 图片提示词改写器；在图片生成前扩写 PDE 的种子主题；存在此任务块时激活） | live（opt-in） |
| `chat_vision` | `pipeline::stream`，通过 `resolve_vision()`（视觉预处理阶段：在 reply prompt 前将 `image_url` 附件描述为 JSON；任务块缺失或 `filter_prompt` 为空白时关闭） | live（opt-in） |
| `affinity_evaluation` | `pipeline::post_process`（每轮六轴 affinity delta；每个 Reply 轮次后以 fire-and-forget 方式运行） | live |
| `memory_extraction` | dreaming sweeper（会话结束时进行 memory 整合；任务块缺失时关闭） | live（opt-in） |
| `chat_input_filter` | `pipeline::stream`（用户输入改写 filter；由 `[tasks.chat_companion]` 上的 `input_filter` 和此任务块共同激活；默认关闭） | live（opt-in） |
| `embedding` | 保留——`VoyageClient` 读取自己的 `VOYAGE_API_KEY` 并 hard-code `voyage-3-lite` | reserved |

只有当引擎确实在某处调用 `model_config.resolve("<name>", ...)` 时，`[tasks.<name>]` 条目才有意义。当前调用点如下：

- `crates/eros-engine-server/src/pipeline/handlers.rs` → `chat_companion`、`chat_output_filter`
- `crates/eros-engine-server/src/pipeline/post_process.rs` → `insight_extraction`、`affinity_evaluation`
- `crates/eros-engine-server/src/pipeline/stream.rs` → `pde_decision`，通过 `run_stream` 内的 `run_pde_decision`（仅当设置了 `filter_prompt`）；`chat_image_generation`，通过 `resolve_image_gen()`（图片执行器，opt-in）；`chat_image_prompt_compose`，通过 `resolve_image_prompt_compose()`（图片提示词改写器，opt-in，仅在图片轮次按需解析）；`chat_vision`，通过 `resolve_vision()`（视觉预处理阶段，opt-in）；`chat_input_filter`，通过 `resolve_input_filter()`（输入改写，opt-in）；`memory_extraction`，通过 dreaming sweeper

`embedding` 已无实际作用——Voyage 不经过此路径。

### `[tasks.pde_decision]` — opt-in LLM PDE 判断器

默认情况下，引擎使用内置规则引擎（`eros-engine-core/src/pde.rs`）决定每轮动作（reply / ghost / proactive）。在此块中设置 `filter_prompt` 会启用 LLM 判断器：

- LLM 接收最近的对话、关系状态和对话信号，并返回 JSON verdict，其中包含：
  - `action`：`"reply_text"` \| `"ghost"` \| `"reply_image"` \| `"reply_text_image"`（请求包含 `image` 块时图片变体才可用——调用方以此声明本轮由自己处理图片；否则降级为 `reply_text`。可用性不再依赖 `[tasks.chat_image_generation]`：聊天流从不绘图，只发出 `image_request` 帧；`[tasks.chat_image_generation]` 仅用于门控独立的绘图端点 `POST /comp/chat/{session_id}/image/stream`。）
  - `inner_state`：融入 reply prompt 的简短情绪/语气描述
  - `image_prompt`、`reason`：可选
- **Fail-open：**任何 LLM 超时或错误都会回退到规则引擎——LLM 判断器绝不会阻塞 chat 响应。
- **硬安全 guardrail**（在 LLM verdict 之后、规则引擎 fallback 之前强制执行）：前 10 条消息绝不 ghost，绝不连续 ghost 两次，ghost cooldown 为一小时。
- 每次判断器调用都会记录到 `companion_decision_events` 以供审计。

**图片能力上下文行。** 判断器上下文每轮必带一行——当本轮图片动作可用（请求带有 `image` 块）时为 `[图片能力] 本轮可发图=是`，否则为 `[图片能力] 本轮可发图=否`。prompt 作者应把 `本轮可发图=否` 当作硬约束（绝不要选 `reply_image` / `reply_text_image`——它们会被 `guard_action` 降级，白费 token 还会污染审计），把 `本轮可发图=是` 当作*允许*发图的开关，再按人格/语境决定要不要发（引擎不会因为"能发"就强制发图）。若下游 overlay 引用了这个 token，请逐字保留 `[图片能力] 本轮可发图=是/否`。

**`ghosting` 字段**（bool，默认 `true`）：面向下游产品的安全开关。设置 `ghosting = false` 可在*整个* PDE 路径上禁用 ghosting——包括 LLM verdict、规则 fallback 和纯规则引擎——从而确保 companion 永不沉默。适用于不希望出现静默轮次的产品。

### `[tasks.chat_image_generation]` — 引擎侧绘图（opt-in）

此块配置引擎的图片执行器——绘图端点（`POST /comp/chat/{session_id}/image/stream`）用来绘制已组合提示词的模型链、风格和尺寸默认值。它**默认关闭**且**可选**。

它**不**门控聊天流。只要当前轮次的请求带有 `image` 块，本轮的 `reply_image` / `reply_text_image` 动作就可用（省略 `image` 会把这些动作降级为 `reply_text`，类似 `chat_vision` 仅在带 `image_url` 时运行）；此时聊天流始终发出 `image_request` 帧、从不绘图。此块只决定当调用方调用绘图端点时引擎是否*绘制*：存在 ⇒ 端点按下方模型链绘图；缺失（或无法解析出模型）⇒ 端点返回 `501`，由调用方自行绘制已组合提示词。

这里可以用任意 OpenRouter 图片模型，包括**只输出图片**的模型（如 `bytedance-seed/seedream-4.5`）：引擎只请求 `modalities: ["image"]`，从不向图片模型要文本。`reply_text_image` 轮次的文字始终来自 `chat_companion`，而非图片模型。

```toml
[tasks.chat_image_generation]
# `model` is OPTIONAL. Omit to defer model selection to the per-turn frontend
# param (req.image.model). When set, reuses ModelSpec: "" fixed / [] round-robin
# / {} weighted (the same three shapes as chat_companion.model).
model = "google/gemini-2.5-flash-image"   # OPTIONAL
# `fallback` is a FallbackSpec: a single id string OR an ordered array tried
# SEQUENTIALLY (first success wins — NOT round-robin). Note: under `model`,
# [...] = round-robin; under `fallback`, [...] = ordered retry chain.
fallback = ["google/gemini-2.5-flash-image"]
default_style = "realistic"          # realistic | semi_realistic | anime
default_aspect_ratio = "3:4"
default_resolution = "1024x1365"
max_tokens = 4096
```

**每轮模型解析**——使用一个统一的候选列表，头部 = primary，尾部 = 重试链：

1. `req.image.model`——前端提供的每轮单 id 覆盖
2. 配置中的 `model`——为本次调用将 `ModelSpec` 解析为一个 id
3. 配置中的 `fallback`——有序重试链条目

后续重复项会被删除（保留最先出现的项）。列表为空表示无法解析出模型，当前轮次会降级为 `reply_text`。

**只配置 `fallback` 也足够：**未设置 `model` 且没有每轮覆盖时，`fallback` 的头部会成为 primary。仅配置 `model`（无 `fallback`）时，失败后没有安全保障。

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `model` | `ModelSpec`（字符串 \| 数组 \| 表） | 缺失 | **可选。** 缺失 ⇒ 执行器已启用，但前端必须为每轮提供模型。 |
| `fallback` | `String` \| `Array<String>` | `[]` | 顺序重试链（FallbackSpec）。 |
| `default_style` | `"realistic"` \| `"semi_realistic"` \| `"anime"` | `"realistic"` | 每轮 style key（可通过 `req.image.style` 覆盖）。 |
| `default_aspect_ratio` | `String` | `"3:4"` | 每轮宽高比（可通过 `req.image.aspect_ratio` 覆盖）。允许值：`1:1`、`3:4`、`4:3`、`9:16`、`16:9`。 |
| `default_resolution` | `String` | 缺失 | 取决于模型的分辨率提示（例如 `"1024x1365"`）。**绘图端点不会应用它**——端点根据 `image_request` 帧中携带的每轮宽高比推导图片尺寸，仅在绘图请求显式传入 `resolution`（`DrawImageRequest.resolution`）时才使用它。 |
| `max_tokens` | `u32` | 编译时内置默认值 | 图片生成调用的 token 上限。 |

**Style preset** 是引擎所有的常量，会注入生成 prompt：

| Key | 描述 |
|---|---|
| `realistic` | 照片级写实的生活抓拍，自然的皮肤纹理、可信的解剖结构、柔和自然光线和真实的智能手机照片美学。 |
| `semi_realistic` | 半写实数字角色插画，可信的解剖结构、柔和绘制的皮肤、略微风格化的面部特征和细致的电影感光线。 |
| `anime` | 高质量日式 anime 插画，干净且富有表现力的线稿、细致的眼睛、精致的赛璐璐上色、协调的解剖结构和细致的背景。 |

**Persona 外观**——如果 persona 的 `art_metadata` 含有 `appearance` key，它会注入生成 prompt，位于 style preset 和主体之间。`appearance` 字段是可选且附加的——没有该字段的现有 persona 不受影响。

调用点：`crates/eros-engine-server/src/pipeline/stream.rs`，通过 `model_config.rs` 中的 `resolve_image_gen()`。

### `[tasks.chat_image_prompt_compose]` — 图片提示词改写器（opt-in）

PDE 在选动作、定 `inner_state` 的同时，还要在很紧的 token 预算里写一个简短的种子 `image_prompt`。配置此任务块后，引擎会在**决定出图之后、生成之前**单独跑一次改写器：把人格外观、最近场景、PDE 种子主题、所选 style 和目标宽高比交给该模型，用扩写后的结果作为图片主体（送给图片模型、写入 `metadata.image.prompt`、并在 `image` SSE frame 中发出）。PDE 的原始种子单独保留在决策审计里。

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
| `filter_prompt` | `String` | **内置默认** | **可选**（不同于其他任务）。空白/缺失 ⇒ 引擎内置的 `DEFAULT_COMPOSE_PROMPT`；非空 ⇒ 覆盖。 |

**内置默认提示词。** 不同于其他每个任务（提示词必须来自配置），改写器自带一个通用、**不含产品身份**的内置默认。它的立场是**只扩写**：忠实地把种子主题和外观扩成详细提示词，**不**软化、不降级、不审查、不拒绝，也不自带内容审核层——审核交给画图模型供应商和下游部署，不在这一步。非空 `filter_prompt` 覆盖它；空白/缺失则回退到它。

调用点：`crates/eros-engine-server/src/pipeline/stream.rs`，通过 `model_config.rs` 中的 `resolve_image_prompt_compose()`。

### `[tasks.chat_vision]` — 图片输入（视觉预处理阶段，opt-in）

当 chat 轮次携带 `image_url` 时，引擎运行 `resolve_vision()` 获取支持视觉的模型和 `filter_prompt`，调用该模型将图片描述为固定 JSON schema（`description`、`ocr_text`、`people`、`scene`），并在主 chat 调用前将结果融入面向用户的 prompt。主 `chat_companion` 模型仍然只处理文本。

此功能**默认关闭**，仅当此任务块存在且 `filter_prompt` 非空白时激活。`retry_depth` 默认为 `1`（primary + 第一个 fallback）。请选择支持视觉的模型；示例使用 `google/gemini-3.1-flash-lite`。

调用点：`crates/eros-engine-server/src/pipeline/stream.rs`，通过 `model_config.rs` 中的 `resolve_vision()`。

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
4. **不从此列表中删除任务名：**`chat_companion`、`insight_extraction`、`pde_decision`。若真实实现落地并取代 reserved 任务名（`embedding`）当前的占位语义，reserved 任务名可能发生变化；该变更会在 changelog 中说明。
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

## 此配置不控制的内容

- **Voyage embedding**——`VoyageClient` hard-code `voyage-3-lite` 并直接读取 `VOYAGE_API_KEY`。`[tasks.embedding]` 块为将来的通用化保留。
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
