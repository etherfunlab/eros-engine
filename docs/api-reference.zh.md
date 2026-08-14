# API 參考

[English](api-reference.md) · [中文](api-reference.zh.md)

任何運行中的實例 **`/docs`** 路徑下都有實時、可瀏覽的參考文檔（utoipa 註解生成的 Scalar UI）。

這個頁面是手寫的端點摘要。Scalar UI 是權威 spec。

## 鑒權

每個 `/comp/*` 跟 `/bff/v1/*` 端點都需要 `Authorization: Bearer <Supabase JWT>`。JWT 必須是 HS256 簽名、密鑰為 `SUPABASE_JWT_SECRET`。`sub` claim 必須是個 UUID；該 UUID 即該請求的 user_id。

`/healthz` 跟 `/docs` 是公開的。

## 公開端點

### `GET /healthz`

存活探針。無需鑒權。

```bash
curl http://localhost:8080/healthz
```

```json
{
  "status": "ok",
  "service": "eros-engine",
  "version": "1.0.x",
  "timestamp": "2026-05-05T19:06:05.309302232+00:00"
}
```

## 對話生命周期

### `POST /comp/chat/start`

對指定人格基因開新 chat session。如果 `(genome_id, jwt_user_id)` 對應的 `persona_instance` 還不存在，服務器先建一個，然後建一個引用該 instance 的 `chat_session`。

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"genome_id":"11d6a45a-1fd9-4fe6-a943-3f049035eb68"}' \
  http://localhost:8080/comp/chat/start
```

```json
{
  "session_id": "5f7e…",
  "instance_id": "…",
  "persona_name": "Aria",
  "is_new": true
}
```

`is_new=false` 表示同一用戶用同一個 `genome_id` 再調 `/start`——引擎恢復已有 session 而不是建重複的。

可选的 `channel` 字段：`"text"`（默认）或 `"voice"`。开新/恢复是按频道隔离的
——语音频道的 start 永远不会恢复一个文本 session，反之亦然。语音客户端必须先
在这里用 `"channel": "voice"` 拿到 session，才能去调语音轮次端点。

可选的 `force_new` 字段：为 `true` 时跳过恢复，总是创建一个全新 session
（`is_new: true`），即使这个用户 × instance × channel 组合本有可以恢复的
session。默认 `false`/缺省保持原本的恢复优先行为。语音通话推荐使用：每通
电话都用 `{"channel": "voice", "force_new": true}` 开始，让每通电话都拿到
自己的 session，而不是接着上一通的。`POST /comp/chat/start` 本身没有内置
速率限制，暴露 `force_new` 的部署可能需要在下游做请求级别的限流。

可选的 `instance_id` 字段：显式指定 `persona_instance` id。缺省时服务器为
所给 `genome_id` 挑选（或自动创建）该用户的 instance；仅当 `instance_id`
缺省时 `genome_id` 才是必填。

可选的 `is_demo` 字段：把新建的 session 标记为 demo。持久化到 session 的
`metadata.is_demo`，好感度管线读它来把正向评分乘以 `AFFINITY_DEMO_BOOST`
（默认 `1.4`），让好感度表在 demo 的轮次预算内有可见的移动。恢复已有
session 时忽略。

### `POST /comp/chat/{session_id}/message/stream`

流式聊天，返回 `text/event-stream`，使用 `meta → delta* → done → final`
状态机（详见 [SSE streaming chat 0.2 设计文档](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md)）。

请求体必须包含 `client_msg_id`（26..36 个 ASCII 可打印字符，任意 UUID 或
ULID）。24 小时内同一对 `(session_id, client_msg_id)` 的重复请求将从
数据库重放历史帧，不会再次调用 OpenRouter。

```bash
curl -N -X POST \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"content":"hi","client_msg_id":"01J3333333333333333333333A"}' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

示例帧（每行 `data:` 后跟一个 JSON 对象）：

```text
data: {"type":"meta","message_id":"01J...","action_type":"reply","model":"x-ai/grok-4-fast"}

data: {"type":"delta","message_id":"01J...","content":"你好"}

data: {"type":"done","message_id":"01J...","truncated":false,"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16},"generation_id":"gen-abc"}

data: {"type":"final","filtered":false,"prompt_injected":null,"tier":null,"retries_chat":0,"retries_filter":0}
```

帧字段说明：

- **`meta`** —— `message_id`、`action_type`、`model`（实际服务的模型 id，可能省略），以及 `continues_from`（可选，本轮续接重试链时为上一条消息 id）。`action_type` 是以下之一：`reply` | `ghost` | `reply_image` | `reply_text_image` | `product_qa`（纯文本回复报告为 `reply`，不是 `reply_text`——线上协议里没有 `reply_text`）。`product_qa` 标记由 PDE 判断器路由的出戏产品问答（见 [model-config.zh.md](model-config.zh.md)）；它被排除在伴侣上下文/记忆之外，但实时流与重放上报告的方式相同。客户端必须容忍未知的 `action_type` 值（新值可能在不打大版本号的情况下新增）。
- **`done`** —— `truncated`、`usage`（经 `OPENROUTER_USAGE_HIDDEN_KEYS` 过滤后；总是存在——不适用时为 `null`）、`generation_id`（OpenRouter id；总是存在——不适用时为 `null`），以及 `ghost_fallback`（bool；为 `false` 时整个字段省略）。`ghost_fallback: true` 标记一条最终解析为空、以静默回退形式交付的回复——它**不是** `action_type=ghost` 轮次，也不会动 ghost 计数。原因记录在落库行的 `metadata.fallback_reason` 上。承诺出图的轮次（`action_type=reply_text_image`）例外：文本半边为空是一条纯图片回复而不是沉默，因此它上报 `ghost_fallback: false`、不带 `fallback_reason`，尾随的 `image_request` 照常发出。
- **`final`** —— 本轮汇总：`filtered`（bool，回复是否被输出过滤）、`prompt_injected`（本轮注入的 trait tag 数组，无则为 `null`）、`tier`（回显请求的 `tier`，未传为 `null`）、`retries_chat`（命中的对话尝试下标，从 0 起）、`retries_filter`（实际服务的过滤模型尝试下标）。这一帧不再带任何画像/lead 信号——`lead_score`、`should_show_cta`、`agent_training_level` 已随 companion_insights 拆除（spec 2026-08-11）移除；画像状态改从 `GET /comp/user/{user_id}/profile` 读取。

每个用户最多 3 条并发活跃流。保活心跳（`: ping`）每 15 秒发一次，
防止反向代理因空闲超时断开连接。

流前错误（第一个 SSE 字节写出之前的 HTTP 4xx/5xx）携带含 `code`、
`message`、`user_message` 字段的 JSON 响应体；`409 duplicate_in_progress`
时还会带 `original_user_message_id`。完整错误码表见
[设计文档](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md#13-pre-stream-errors-http-status-json-body)。

**该端点仅限文本频道。** 传入语音频道 session 的 `session_id` 会在落库任何
一行之前被拒绝，返回 `409 wrong_channel` —— 它写的是文本频道消息，让这些行
落进语音会话会使两个频道在同一份 transcript 里交错。语音轮次请改用
[`POST /comp/voice/{session_id}/turn/stream`](#post-compvoicesession_idturnstream)。
这道闸与语音端点自己的闸对称：两个端点都不会写进对方的 session。

一旦第一个 SSE 字节写出，终端错误以带内 `error` 帧的形式到达并关闭流；
此时 HTTP 响应已提交 `200 OK`。

**可选：tier 选择。** 请求体可附加 `tier` 字符串 ——
类型 `String`，正则 `^[a-z0-9_]{1,32}$`（格式错返回 `400`）。
从 `model_config.toml` 中选择对应 tier 的模型和 `allow_traits`
（`[tasks.chat_companion.tiers.<tier>]`）。tier 未知或缺省时
回退到任务默认块（会记录一条 warn 日志）。示例：

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "tier": "gold"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**可选：单轮 prompt traits。** 请求体可附加 `prompt_traits` 数组 ——
详见 [prompt-traits.zh.md](prompt-traits.zh.md)。示例：

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "prompt_traits": [
          {"tag": "nsfw_boost", "text": "<your injection text>"}
        ]
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

限制：最多 8 条，`tag` 满足 `[a-z0-9_]{1,32}`，`text` ≤ 2000 字符
（trim 后非空）。违反作为 pre-stream 错误返回 `400 BadRequest`。

**可选：记忆注入范围。** 请求体可附加 `memory_scope` 字符串，控制哪些
记忆层会被注入到 prompt 中。接受值：

| 值 | 注入内容 |
|-------|----------|
| `full` | 完整用户画像（含亲密字段）+ 关系记忆 |
| `neutral_and_relationship` | 中性画像（仅城市/职业/MBTI）+ 关系记忆 **（默认）** |
| `relationship_only` | 仅关系记忆；不含画像 |
| `neutral_only` | 仅中性画像；不含关系记忆 |
| `insights_only` | 仅完整用户画像（含亲密字段）；不含关系记忆 |
| `none` | 不注入任何记忆 |

> **重要（#40 缓解措施）：** 默认的 `neutral_and_relationship` 有意比
> #40 之前的行为更窄（旧行为注入全部内容）。**省略 `memory_scope` 并不
> 等同于旧行为**——会应用收窄后的默认值。如需完整注入，请显式指定 `full`。

**可选：好感度注入范围。** 请求体可附加 `affinity_scope` 值，控制六个
好感度轴中哪些会被注入到 prompt 中。接受值：

- 具名预设：`"bond"` **（默认）** — warmth + intimacy + tension；
  `"chemistry"` — trust + intrigue + patience；`"bond_and_chemistry"` / `"full"` — 全部六轴；`"none"` — 不注入好感度。
- 轴数组：`["warmth", "trust", "intrigue", "intimacy", "patience", "tension"]` 的任意子集。

> **重要（#40 缓解措施）：** 默认的 `bond`（3 轴）有意比 #40 之前的行为
> 更窄（旧行为注入全部六轴）。**省略 `affinity_scope` 并不等同于旧行为**。
> 如需全轴注入，请显式指定 `"bond_and_chemistry"` 或 `"full"`。

> **1.2.1 起：该字段同时影响计分。** 除了决定注入什么，`affinity_scope` 还决定
> 本轮判官档位如何折算成分数增量：只点名 bond 半边的 scope 会增强 bond 独占轴；
> 任何触及 chemistry 半边的 scope（含 `full`）会让 chemistry 独占轴过一道档位门槛。
> `none` 不做任何转向。共享的 `warmth` 两个方向都豁免。
> 只要注入分级、不要计分影响的部署，把 `AFFINITY_SCOPE_BOND_BOOST=1.0` 与
> `AFFINITY_SCOPE_CHEM_LADDER=0,1,2,3,4` 设成恒等即可。详见
> [好感度模型 → Scope 转向](affinity-model.zh.md#scope-转向好感度-31)。

同时使用两个字段的示例：

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "memory_scope": "full",
        "affinity_scope": "bond_and_chemistry"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**可选：回复锚点（回卷）。** 请求体可附加 `reply_to_message_id` ——
本 session 内某条 `chat_messages` 行的 UUID，把本轮上下文锚定到那条消息。
解析成功时，历史回卷到（且包含）那条消息：晚于它发送的行不进入 prompt，
锚点记录在落库的用户行的 `metadata.reply_to_message_id` 上。传了但解析
不到（id 不存在，或属于别的 session）不会让请求失败——本轮丢弃历史
（上下文里只剩当前这条消息），并在该行的 `metadata.reply_to_error` 写入
`"not_found"`。省略该字段即为正常的最新历史行为。

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "等等，说回之前那个计划",
        "client_msg_id": "01J3333333333333333333333A",
        "reply_to_message_id": "3cc06c53-9d2e-4f8a-b3c1-0a1b2c3d4e5f"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**可选：OpenRouter audit 透传。** 请求体可附加 `audit` 对象，
原样作为 wire 级别的 `user` / `session_id` / `metadata` 发送给
OpenRouter —— 详见 [llm-audit.zh.md](llm-audit.zh.md)。示例：

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "hi",
        "client_msg_id": "01J3333333333333333333333A",
        "audit": {
          "user": "u_<hash>",
          "session_id": "conv_xyz",
          "metadata": { "feature": "chat", "plan": "pro" }
        }
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

限制：`audit.user` 与 `audit.session_id` ≤ 256 字符；`audit.metadata`
≤ 16 个 key，key 满足 `[A-Za-z0-9_.-]{1,64}`，value 必须是 string
且 ≤ 512 字符。违反作为 pre-stream 错误返回 `400 BadRequest`。

**可选：打赏。** 请求体可附加 `tips_amount_usd`（有限数值，`> 0` 且
`≤ 1_000_000`）把本轮标记为打赏。该轮以 `role = gift_user` 落库：`content`
为空时存为 `(打赏 $<金额>)`，否则保留你的 `content`。打赏金额会带给模型，
让人格在回复里作出反应，并在 BFF 历史行回显（`tips_amount_usd`）。同一轮
不能既打赏又带图。替代了已移除的 `POST /comp/chat/{session_id}/event/gift`
路由。

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "",
        "client_msg_id": "01J3333333333333333333333A",
        "tips_amount_usd": 9.99
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**可选：图片输入（vision）。** 请求体可附加 `image_url` —— 绝对 `http(s)`
URL，需带 host、不含空白、≤ 2048 字符。带图时引擎先跑一段 vision *describe*
预处理（`chat_vision` 任务），把图片描述喂给回复。`image_url` 与
`tips_amount_usd` 同一轮互斥。URL 非法时作为 pre-stream 错误返回
`422 Unprocessable Entity`（`code: "unprocessable"`）。仅当
`[tasks.chat_vision]` 配了非空 `filter_prompt` 时 vision 才生效（见
[model-config.zh.md](model-config.zh.md)）。

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "这张图里有什么？",
        "client_msg_id": "01J3333333333333333333333A",
        "image_url": "https://example.com/cat.jpg"
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

**可选：伴侣图片回复。** 请求体可附加 `image` 对象（`ImageReplyParams`），请求或强制本轮生成一张伴侣发送的图片。`image` 块同时是本轮的 opt-in 开关：**省略它即可关闭本轮的图片生成**（此时 PDE 只能 `reply_text` / `ghost`），或发送 `image: {}` 用引擎内置默认值启用。这样调用方可以用自己的 per-turn 策略独立于 PDE 的内容决策来控制是否出图。

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "给我看个笑脸",
        "client_msg_id": "01J3333333333333333333333A",
        "image": {
          "force": true,
          "style": "realistic",
          "aspect_ratio": "3:4"
        }
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

出现 `image` 块表示消费方负责本轮的画图动作；引擎只组装提示词并发送单个
`image_request` 帧（绝不在聊天流上绘图）。

`ImageReplyParams` 字段（全部可选）：

| 字段 | 类型 | 默认值 | 备注 |
|---|---|---|---|
| `force` | `bool` | `false` | 强制本轮发图，覆盖 PDE 决策——该轮固定为 `reply_image`（仅图片，无文字回复）。要求部署配置了 `[tasks.chat_image_prompt_compose]`（否则 `422`），且 `content` 遵循普通的非空规则。`false` 时由 PDE 决定。1.0.1 之前旧契约遗留的 `mode` 键可以正常反序列化，但会被静默忽略。 |
| `style` | `"realistic"` \| `"semi_realistic"` \| `"anime"` | `"realistic"` | 引擎内置三种风格预设之一；`"realistic"` 是引擎内置默认值。 |
| `aspect_ratio` | `String` | 无 | 允许值：`1:1`、`3:4`、`4:3`、`9:16`、`16:9`；省略时不存在（PDE 计划 → 请求 → 不存在）。非法时返回 `422`。 |
| `prompt_variant` | `String` | 无 | 选择 `[tasks.chat_image_prompt_compose].filter_prompt` 的一个变体：按下标（`"0"`、`"1"`）或按 key（`"a"`、`"b"`），取决于该任务的配置形态（见 [model-config.zh.md](model-config.zh.md)）。`"raw"` 不带任何特殊含义：只有当该部署把某个变体配置在这个字面量 key 下时才会命中，和其他任意变体名一样。下标/key 没命中——包括未配置的 `"raw"`——都会回退到引擎内置的合成器提示词，绝不报 `422` 或其他错误。该任务未配置，或配置为单一纯字符串提示词时，此字段被忽略。 |

**参考图选择（`image_ref`）。** PDE verdict 带有 `image_ref`（`"face"` \| `"previous"`，默认 `"face"`），并附带在下方的 `image_request` 帧中——聊天流本身不会把它解析成实际 URL。`previous` 且无可用图时回退到 `face` 的规则，以及 `face_ref_url` / `prev_image_url` 参考图 URL，都属于消费方自己调用的图像供应商（引擎没有绘图端点）。持久化的 `metadata.image` 标记记录合成器决定的图片主题、画幅，以及它的 `caption`（合成器随 prompt 一起返回的一句话描述；没有则为 `None`——聊天历史和 judge transcript 只读回 caption，从不读回那段长 prompt），加上——仅当合成器 LLM 调用成功时——审计三元组 `compose_variant`（命中的 `filter_prompt` key/下标，纯字符串或内置提示词时缺省）、`compose_model` 和 `compose_generation_id`。三元组缺失意味着本轮没有成功合成（fail-open 降级，或合成器未配置）。只要审计写入本身成功，就会带一个 `compose_event_id` 指针——不管合成本身有没有成功——它是通往 `engine.chat_images_events` 的可达链接，拼装出的**线上 wire** prompt 实际存在那张表里（`metadata.image` 标记从不重复存它）；详见 [LLM audit → 图片链路事件表](llm-audit.zh.md#图片链路事件表)。不记录参考类型。

校验：同一轮同时有 `force` 和 `tips_amount_usd` → `422`。`force` 而部署未配置
`[tasks.chat_image_prompt_compose]` → `422`（合成器是唯一的提示词来源；没有它，
强制出图只能产出一张无视用户消息的通用肖像）。`aspect_ratio` 不在允许集时，作为
pre-stream 错误返回 `422 Unprocessable Entity`（`code: "unprocessable"`）。以上
全部是 pre-stream 错误：不会落任何用户行。

**`image_request` SSE 帧** — 每个图片轮次发出一次，取代任何引擎内绘图。引擎负责组装提示词；由消费方通过自己的图像供应商绘制（引擎没有绘图端点）。聊天流本身不绘图、不回传图像字节、不持久化绘图结果。

```
data: {"type":"image_request","message_id":"01J...","composed_prompt":"5YaZ5a6e...","image_ref":"face","aspect_ratio":"3:4"}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `type` | `"image_request"` | 帧类型标识。|
| `message_id` | `String` | 真实的 assistant `message_id`；绘图与存储都以它为键。|
| `composed_prompt` | `String` | 最终发给图像服务的提示词的 UTF-8 字节的 base64（`STANDARD`，无换行）。在最后一跳解码后原样用作图像服务的文本提示词，不要再重建任何提示词逻辑。|
| `image_ref` | `"face"` \| `"previous"` | 计划选择的参考图；实际 URL 由消费方解析。|
| `aspect_ratio` | `String` \| 不存在 | 语义画幅（`1:1`,`3:4`,`4:3`,`9:16`,`16:9`）或不存在。画幅→分辨率映射由消费方负责，引擎不发送宽高。|

**完整 SSE 帧序列：**

- 纯图片：`meta(reply_image) → done → image_request → final`
- 图文：`meta(reply_text_image) → delta* → done → image_request → final`
- `ghost`：`meta(action_type=ghost) → done → final` — 无 `delta`，`meta` 中无 `model`，`done` 的 `usage` 和 `generation_id` 均为 `null`。该轮伴侣保持沉默，未调用任何 LLM。
- `product_qa`：`meta(action_type=product_qa) → delta* → done → final` — 形状与普通文本回复相同，由独立的模型链（`[tasks.chat_product_qa]`）流式生成，而非 `chat_companion`；落库时带 `channel='product_qa'`，重放时同样报告为 `product_qa`。

引擎从不绘图，也不存在任何绘图生命周期帧：消费方收到 `image_request` 后自行调用图像供应商。

### `GET /comp/chat/{session_id}/history?limit=20&offset=0`

分頁讀消息歷史，最新在前。`limit` 默认 20（上限 50）。

```json
{
  "messages": [
    { "id": "…", "role": "assistant", "content": "Bishop。", "sent_at": "…" },
    { "id": "…", "role": "user",      "content": "嗨…",     "sent_at": "…" },
    { "id": "…", "role": "assistant", "content": "…", "sent_at": "…", "channel": "product_qa" }
  ]
}
```

`role` ∈ `user | assistant | gift_user | system_error`。`gift_user` 是打赏轮
（通过上面 stream 路由的 `tips_amount_usd` 发起）。每条记录还带一个可选的
`channel` 字段——`"product_qa"` 标记出戏产品问答（排除在伴侣上下文/记忆之
外，与其在实时流上的 `action_type` 一致）；普通轮次省略该字段。

## 语音

### `POST /comp/voice/{session_id}/turn/stream`

精简的语音频道轮次：进来一条转写好的用户话，出去一条流式文本回复。STT 和 TTS
完全是调用方的事，引擎从不碰音频（见
[voice-call parts 设计文档](superpowers/specs/2026-07-07-voice-call-parts-design.md)）。

返回 `text/event-stream`，帧集合更小：`delta`* 之后接一个终结的 `done`，或者
单独一个 `error`——帧的形状与上面的聊天消息流一致，但**没有** `meta` 帧，也没有
`action_type`。

session 必须是**语音频道** session（否则 `409 wrong_channel`）——通过
`POST /comp/chat/start` 带 `"channel": "voice"` 拿到。语音是每个部署自行选择
开启的：model config 里没有 `[tasks.chat_voice]` 块时，该端点返回
`501 voice_disabled`。

prompt 精简但不再失忆：人格 + 语音指令 + 首轮生成、此后每轮原样重发的
**引导快照（bootstrap snapshot）** + 一行由该 session 好感度推导出的关系描述
（bond/chemistry 档位）+ 本轮的**召回块**。历史窗口是最近 8 条消息（4 个来回）
——比聊天路径的窗口短，因为更长程的记忆改由引导快照和召回承担。语音
**单轮**不写入任何记忆（没有 insight 抽取，也没有向量写入），但一通结束
的**通话**会写：session 空闲之后，dreaming-lite 清扫器会把它的通话记录蒸馏成
画像层记忆，之后的语音通话和文字聊天都能召回。部署方可以用
`DREAMING_VOICE_DISABLED=1` 关掉——见
[memory-layers.zh.md](memory-layers.zh.md#语音轮次)。

**引导快照**（仅首轮组装，随后冻结进 `chat_sessions.metadata.voice_bootstrap`，
此后每轮原样重新注入——provider 是无状态的，线路上不存在"只注入一次"这回事）：
一个 `[关于他]` 块（`human_insights` bullets，默认中性档，见下面的
`memory_scope`）加一个 `[上次通话]` 块（上一通语音通话最后 8 条消息渲染出的
文字记录）。两部分各自独立、静默降级——组装失败时标记位不落，下一轮重试。

**每轮召回**（每轮都跑，只读）：对聊天路径同样的 `companion_memories` 分层
做一次更小的向量检索，受 `memory_scope` 门控，300 ms 预算内完成——超时或
检索失败只是丢掉这一轮的召回块，绝不报错。去掉空白和标点后不足 4 个字母
数字字符的话语（嗯 / 好啊 / 哈哈这类应和词）直接跳过召回，不发起 embedding
调用。部署方可以用 `[tasks.chat_voice] recall = false`（默认 `true`）强制
关闭召回，不管请求里传了什么 `memory_scope`——详见
[model-config.zh.md](model-config.zh.md)。

Body 字段：

- `content` —— 用户说的那句话。最长 4096 字符。
- `client_msg_id` —— 26..36 个 ASCII 可打印字符（任意 UUID 或 ULID）。同一组
  `(session_id, client_msg_id)` 重放，**只有在该轮次已经产生了结果时**才算
  冲突：已经存在一条 assistant 回复（重试会重复计费），或者该轮次被主动
  打断过（见下面的
  [`turn/interrupt`](#post-compvoicesession_idturninterrupt)）——这两种情况
  都返回 `409 duplicate`。两者都不成立时——异常断连，或者候选模型全部失败、
  上游调用耗尽——重放会**重新生成**：复用已落库的 user 行，发起一次新的调用，
  而不是报错。走这条修复路径时，请求体里的 `content` 会被**忽略**；已落库
  的那句话才是权威版本（内容不一致只会记一条 warn 日志，绝不会拒绝请求），
  因为每轮召回会把这段文字当作查询向量，同一轮次的多次尝试之间不能漂移。
  这也意味着：客户端在收到 `Error { retryable: true }` 帧之后再重试，现在
  真的能成功了，不会再被本该放行的重复检查挡回去。
- `affinity_scope`（可选）—— 字段名、取值空间、默认值（`"bond"`）都与
  [聊天消息流](#post-compchatsession_idmessagestream) 相同：命名值
  `"full" | "bond_and_chemistry" | "bond" | "chemistry" | "none"`，或轴名
  数组（如 `["warmth", "trust"]`）。语音按「半句」粒度注入，解析出的轴会
  压平成关系描述的两半：bond 半（warmth / intimacy / tension 任一激活）与
  chemistry 半（trust / intrigue / patience 任一激活）。审计：**user** 行在
  `metadata.affinity_scope_raw`（及 `metadata.memory_scope_raw`）记录原样
  取值，均为请求带了该字段才写；**assistant** 行保留解析后的
  **`metadata.affinity_scope`**——同一个 6-bool 对象（`warmth` / `trust` /
  `intrigue` / `intimacy` / `patience` / `tension`），与
  [聊天消息流](#post-compchatsession_idmessagestream) 写入的形状逐字节
  一致——并新增解析后的 `metadata.memory_scope`。
- `memory_scope`（可选）—— 字段名、枚举值、默认值
  （`"neutral_and_relationship"`）都与
  [聊天消息流](#post-compchatsession_idmessagestream) 相同。session 里
  **首个成功组装快照的轮次**中，解析出的 insight 档位会决定引导快照
  `[关于他]` 部分的档位，并为整通电话冻结；每一轮它还会门控当轮的召回块。
  之后的轮次改不了快照的档位。

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"你今天在干嘛？","client_msg_id":"01JABCDEFGHJKMNPQRSTVWXYZ0","affinity_scope":"bond","memory_scope":"neutral_and_relationship"}' \
  http://localhost:8080/comp/voice/{session_id}/turn/stream
```

### `POST /comp/voice/{session_id}/turn/interrupt`

上报一次主动打断（barge-in）：companion 的回复还在播放的时候，用户开始说话了。

**这个端点不负责停止生成。** 客户端断开 `turn/stream` 的 SSE 连接就已经做到
这件事——stream 生成器在当前的 await 点被 drop，连带把上游连接也关掉了。这个
端点唯一的任务是记录**用户实际听到了什么**，这正是被 drop 掉的生成器再也做不
到的事：它的落库步骤在流式循环之后才跑，drop 时根本不会执行到。这里是纯 JSON
的请求/响应——不是 SSE 路由。

**没有 `501 voice_disabled` 门槛**，跟 `turn/stream` 不一样。这个端点不发起
任何 LLM 调用，只是写几行数据库记录；把它也挂在 `[tasks.chat_voice]` 上，会
导致部署方运行中途改配置时，一通正在进行的电话的打断请求跟着失败。

Body：

```json
{ "client_msg_id": "01JABCDEFGHJKMNPQRSTVWXYZ0", "spoken_text": "你今天过得" }
```

- `client_msg_id` —— 被打断的那个轮次，格式跟 `turn/stream` 一样：26..36 个
  ASCII 可打印字符。**必须是该 session 最新的一个 user 轮次**——见下面的守卫。
- `spoken_text` —— TTS 实际播放出来的内容，原样传。可以是空字符串（用户在
  任何声音播出之前就打断了）；空字符串不会写入任何 assistant 内容——只有
  user 行上的标记会记录下发生过一次打断。最长 4096 字符。

Response `200`：

```json
{ "message_id": "01JABCDEFGHJKMNPQRSTVWXYZ0" }
```

`message_id` 是现在持有这段话的 assistant 行 id；如果什么都没播出、也没有
回复行可指，就是 `null`。

**最新轮次守卫。** 如果 `client_msg_id` 指向的不是该 session 最新的 user 行，
会被 `409 not_latest_turn` 拒绝。没有这层守卫，下面的 upsert 就能让客户端改写
**任意一条历史** assistant 回复的 `content`——你只能打断正在说的那句话。这给
客户端加了一条顺序要求：**打断请求要在下一轮开始之前发出。** 迟到的打断会被
拒绝，该轮次就退化成异常断连状态（可以走上面 `turn/stream` 的重新生成路径
恢复），而不会去改写历史。

**Upsert 语义（完成竞态）。** 断连和打断 POST 是两次独立的往返请求，服务器
可能还没处理完 SSE 的断开信号，`turn/stream` 自己在流式结束后的落库就有可能
同时落地。两个写入者落的是同一条 assistant 行（以 user 行的 id 为键，
`ON CONFLICT (user_message_id) WHERE role='assistant' AND channel='voice'`），
所以不管到达顺序如何，最终都只会剩一行，而它的 `content` 永远以打断上报的为
准：

| assistant 行 | `spoken_text` | 结果 |
|---|---|---|
| 不存在 | 非空 | 插入一行，`truncated = true` |
| 不存在 | 空 | 不写 assistant 行 |
| 已存在（竞态） | 非空 | 覆盖 `content`，`truncated = true`；保留 `model` / `usage` / `generation_id` 和 `affinity_scope` / `memory_scope` 元数据 |
| 已存在（竞态） | 空 | `content` 保持不动 |

同一轮次重复调用打断接口是幂等的——标记和 upsert 都以 user 行的 id 为键，重试
不会让行数翻倍。

**这张表之外还有一种竞态：同一轮次的两个 `turn/stream` 生成器。** 比如一个
因断连而孤儿化、但还存活的生成器（TCP 重传会让它再活几十秒），撞上了客户端
重试发起的另一个生成器——这里没有打断参与，上面四行都不适用，因为没有标记
能让某一方权威。这种情况是 `content`、`truncated` 和审计列**一起**由后落地
的那一次调用说了算（last-writer-wins），绝不会出现某一次生成的正文配上另一
次生成的 `generation_id`；实现见
`crates/eros-engine-store/src/chat.rs` 里 `insert_voice_assistant_message`
的 `ON CONFLICT` 子句。

状态码梯度：

| 状态码 | code | 何时 |
|---|---|---|
| 200 | — | 打断已记录（见上面的 body） |
| 400 | `invalid_payload` | `client_msg_id` 不在 26..36 个 ASCII 可打印字符范围内 |
| 401 | `unauthorized` | JWT 缺失 / 格式错 / 过期 / 密钥不符 |
| 403 | `session_forbidden` | session 不属于该 JWT 用户 |
| 404 | `session_not_found` | `session_id` 不存在 |
| 404 | `turn_not_found` | `client_msg_id` 在该 session 里找不到对应的 user 行 |
| 409 | `wrong_channel` | session 不是语音频道 session |
| 409 | `not_latest_turn` | 指定的轮次不是该 session 最新的 user 行 |
| 422 | `unprocessable` | `spoken_text` 超过 4096 字符 |

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"client_msg_id":"01JABCDEFGHJKMNPQRSTVWXYZ0","spoken_text":"你今天过得"}' \
  http://localhost:8080/comp/voice/{session_id}/turn/interrupt
```

## Persona

### `POST /persona/{instance_id}/image/compose`

面向单个角色实例的独立图片提示词合成——给「想为任意文本拿一段提示词、而不是走
一轮聊天」的消费方用。**不碰任何聊天状态——不落 session，不落 message，不跑
好感度，不写记忆。** 但每次调用都会被审计——流式模式在客户端断连时有一个
例外（见下文）。实例必须属于 JWT 用户（否则 `403`；不存在时 `404`）。要求配置
`[tasks.chat_image_prompt_compose]`（没有则 `501 compose_disabled`）。

这个端点同时是合成器的调试面：响应携带 `model` 和 `generation_id`，流式模式
把合成器的原始输出逐字透传——调 `filter_prompt` 时最常见的失败是模型没吐出合法
JSON，运维需要看到它实际返回了什么。

Body 字段：

| 字段 | 类型 | 必填 | 备注 |
|---|---|---|---|
| `content` | `String` | 是 | trim 后非空，最长 4096 字符。落入合成器的 `[对方最新消息]` 槽位。 |
| `scene` | `String` | 否 | 落入 `[最近场景]`；省略或空白 ⇒ `（无）`。最长 8192 字符（超出 `422`）。它是合成器的*输入*，不是提示词——引擎绝不会把它复制进 `composed_prompt`，只组装合成器自己的输出。 |
| `style` | `String` | 否 | 与聊天路径相同的三种预设；默认 `realistic`。 |
| `aspect_ratio` | `String` | 否 | 与聊天路径相同的允许集；其他值 `422`。 |
| `prompt_variant` | `String` | 否 | 与聊天路径相同的变体选择规则，包括「未命中的 key 回退到内置提示词」。 |
| `stream` | `bool` | 否 | 默认 `true`。 |

合成器载荷与聊天路径的五个槽位完全一致，同一份 `filter_prompt` 契约同时服务两个
调用方（见 [model-config.zh.md](model-config.zh.md)）。

每一次调用——不管成功失败——都会记录进 `engine.chat_images_events`
（`source = "compose_endpoint"` 或 `"compose_endpoint_stream"`）：非流式模式在
HTTP 响应返回之前就同步写完，没有缺口。流式模式的写入是写在 SSE generator
内部的，客户端如果在 generator 走到某次写入之前就断连，那一行照样会丢——
即使那次调用已经计费了。见
[LLM audit → 图片链路事件表](llm-audit.zh.md#图片链路事件表)。

两种模式返回同样的五个字段：

| 字段 | 含义 |
|---|---|
| `composed_prompt` | 风格预设 + 角色外观 + 主题——直接交给图像供应商的字符串 |
| `subject` | 合成器自己写的 prompt 字段，组装之前的原文 |
| `caption` | 合成器的一句话描述，没有则为 `null` |
| `model` | 实际应答的模型 |
| `generation_id` | 用于与供应商日志对账 |

`stream: false` 时以单个 JSON body 返回。`stream: true` 时返回
`text/event-stream`：

```
data: {"type":"delta","content":"{\"prompt\":\"…"}
data: {"type":"done","composed_prompt":"…","subject":"…","caption":"…","model":"…","generation_id":"…"}
```

- `delta` 帧逐字透传合成器的原始输出，不做任何解析。
- 一个终结的 `done` 帧携带五个字段——其载荷去掉 `type` 判别符后与
  `stream: false` 的 body 逐字节一致。
- 开流之后失败时发送单个 `{"type":"error",…}` 帧，形状与聊天流的带内错误一致
  （`code`、`retryable`、`message`、`user_message`）。

没有 `meta` 帧。只要结果的消费方可以忽略 delta、只读终结帧。

合成器成功返回但不是 JSON 时，保持与聊天路径一致的行为：`subject` 是整段原始
回复，`caption` 为 `null`，`composed_prompt` 照常由它组装。

**输出要当作模型生成的内容，而不是已净化的内容。**「引擎绝不把 `content` /
`scene` 复制进 `composed_prompt`」说的是数据流向，不是安全边界：合成器是一个
读取调用方文本的语言模型，这些槽位可以左右它，它也可以把内容原样回显到
`delta` 帧和 `subject` 里。长度上限限制的是成本，不是影响力。把
`composed_prompt` 转发给图像供应商的调用方，自行承担该供应商要求的内容策略。

失败模式：

| 条件 | 响应 |
|---|---|
| 未配置 `[tasks.chat_image_prompt_compose]` | `501 compose_disabled` |
| 实例不属于 JWT 用户 | `403` |
| 实例不存在 | `404` |
| `content` 空白、`scene` 超长、`aspect_ratio` 非法 | `422` |
| 超出每用户并发上限（与聊天/语音共享，≤3） | `429` |
| 合成器链条全部失败 | `502`——`{"error":"upstream","message":…}`——开流之后则为带内 `error` 帧。**这里没有肖像回退**：回退的存在是为了让聊天轮次继续走下去，而这个端点没有轮次要保护。 |

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"在海边，黄昏","style":"realistic","aspect_ratio":"3:4","stream":false}' \
  http://localhost:8080/persona/{instance_id}/image/compose
```

## 用戶畫像

### `GET /comp/chat/{user_id}/sessions`

該 `user_id` 名下的所有 chat sessions。路徑裡的 `user_id` **必須** 等於 JWT 裡的 user_id；否則 403。

### `GET /comp/user/{user_id}/profile`

这个用户扁平化、类型化的 `human_insights` 行——insight 抽取器每轮增量 UPSERT 的那些列。`user_id` 等值检查同上。

```json
{
  "user_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c",
  "city": "Hong Kong",
  "location": null,
  "hometown": null,
  "nationality": null,
  "occupation": "graphic designer",
  "mbti_guess": "INFP",
  "love_values": null,
  "emotional_needs": null,
  "life_rhythm": null,
  "interests": ["jazz", "long walks"],
  "personality_traits": [],
  "preferred_gender": null,
  "age_min": null,
  "age_max": null,
  "deal_breakers": [],
  "education": null,
  "family": null,
  "relationship_history": null,
  "social_pattern": null,
  "future_plans": null,
  "finance_status": null,
  "updated_at": "2026-08-11T12:00:00Z"
}
```

`updated_at` 为 `null` 表示这个用户还没有 `human_insights` 行——还没跑过任何抽取——这时其余字段也都是 `null`/`[]`。现在没有汇总的"训练程度"分数了：`agent_training_level` 和原始的 `companion_insights` JSONB 都随 companion_insights 拆除（spec 2026-08-11）一起移除，上面这些类型化字段就是现在的全部返回内容。

> **打赏取代了礼物事件。** 独立的礼物路由
> （`POST /comp/chat/{session_id}/event/gift`、`GET /comp/chat/{session_id}/gifts`）
> 已移除。打赏现在是普通 stream 轮的一部分 —— 在
> `POST /comp/chat/{session_id}/message/stream` 上设 `tips_amount_usd`（见上文）。

## Debug

### `GET /comp/affinity/{session_id}`

实时 6 轴向量 + Bond/Chemistry 分数与标签 + ghost 统计 + 遗留关系标签。受 `EXPOSE_AFFINITY_DEBUG=true` 环境变量控制；关闭时返 404。

```json
{
  "warmth": 0.42,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.55,
  "tension": 0.04,
  "bond": 0.21,
  "chemistry": 0.17,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` —— 真实存储的合成分数（0–1）；不套任何显示曲线
  （节奏的非线性在写侧的分档衰减里——见
  [affinity-model.zh.md](affinity-model.zh.md)）。
- `bond_label` ∈ `acquaintance | friend | close_friend | confidant | soulmate`
- `chemistry_label` ∈ `spark | flirtation | crush | lover | beloved`
- `relationship_label` —— 遗留映射值（`stranger | friend | slow_burn | romantic`；`frenemy` 已停止输出）。

生产部署通常关着。若前端需要渲染实时雷达图或检查衍生线，再打开。

### `GET /comp/affinity/{session_id}/event?limit=20&offset=0&event_type=message`

该 session 的好感度**事件日志**，分页、最新在前。和向量路由一样受
`EXPOSE_AFFINITY_DEBUG=true` 控制（关闭时 404）。每条同时带每轮的
`deltas`（本轮原始分：档位换算 + 规则微调，衰减前）、实际应用的
`effective_deltas`（`after − before`；被阈值门控缓存的轮次全为零）、折叠后的
`effective_deltas_computed`，以及档位跨越时的 `label_changes`。`event_type`
可选用于过滤；`limit` 默认 20（上限 100）。

```json
{
  "events": [
    {
      "event_id": "…",
      "event_type": "message",
      "deltas":           { "warmth": 0.06, "trust": 0.02, "intrigue": 0.0, "intimacy": 0.0, "patience": 0.0, "tension": -0.02 },
      "effective_deltas": { "warmth": 0.042, "trust": 0.014, "intrigue": 0.0, "intimacy": 0.0, "patience": 0.0, "tension": -0.02 },
      "effective_deltas_computed": { "bond": 0.019, "chemistry": 0.007 },
      "label_changes": null,
      "created_at": "…"
    }
  ]
}
```

`event_type` 过滤可取 `message | gift | proactive | ghost | time_decay`
（`time_decay` 为预留，当前代码不写入）。若要一个**不受** debug 开关控制、
只返回最新一条（仅实际应用变化）的前端用面板，用下面的 BFF 路由
`GET /bff/v1/comp/affinity/{session_id}/event`。

## BFF（`/bff/v1/*`）

面向第一方前端、把部分 `/comp/*` 路由重塑成前端形狀的鏡像層。鑒權與
canonical 路由完全相同（同樣的 Supabase JWT、同樣的 per-user ownership
檢查），只有 **響應形狀** 不同（更精簡的 DTO、打包好的 payload）。
canonical `/comp/*` 路由永遠不會為了遷就前端而被改形狀——而是在旁邊
新增一條 BFF 路由。目前有三條。

### `POST /bff/v1/comp/chat/start`

冷啟動打包：一個 round-trip 內既解析（或創建）session，又返回它最近的
歷史，把前端原本分開的 `start` + `history` 兩個調用合成一個。同一用戶 +
同一輸入，會解析到與 canonical `POST /comp/chat/start` 完全相同的 session。

請求體 = canonical start 請求體，外加一個 BFF-only 字段：

- `genome_id` / `instance_id` —— 標識人格（同 canonical）。
- `is_demo` —— 可選，同 canonical。
- `history_limit` —— 可選，打包歷史的頁大小；默認 50，上限 50。
- `force_new` —— 可选，同 canonical。透传给 `StartChatRequest::force_new`
  ——跳过恢复，总是创建一个全新 session（`is_new: true`）；语音通话推荐
  使用（见上面的[语音小节](#post-compvoicesession_idturnstream)）。

```json
{
  "session_id": "5f7e…",
  "instance_id": "…",
  "persona_name": "Aria",
  "is_new": false,
  "history": [
    { "id": "3cc06c53-…", "client_msg_id": "c_abc", "role": "user",      "content": "hello",   "sent_at": "…" },
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "hi back", "sent_at": "…" }
  ]
}
```

這裡 **不會** 打包 affinity——前端單獨讀取（見下面的 affinity event
路由），這樣 bootstrap 就與 `EXPOSE_AFFINITY_DEBUG` 解耦。

### `GET /bff/v1/comp/chat/{session_id}/history?limit=50&offset=0`

給聊天屏用的精簡歷史投影：`id` / `client_msg_id` / `role` / `content` /
`sent_at`（不含 `extracted_facts`），打赏行另带 `tips_amount_usd`（仅在
`role = gift_user` 时出现，否则省略），以及可选的 `channel` 字段——
`"product_qa"` 标记出戏产品问答（排除在伴侣上下文/记忆之外）；普通轮次
省略该字段。`id` 是 `chat_messages` 行的主鍵（UUID）；
`client_msg_id` 是前端串流時帶上的 id（沒帶的行為 `null`，例如 assistant 回合）。
鑒權、ownership 檢查、`limit ∈ [1, 50]` 夾取
都與 canonical history 路由相同。**刻意差異：** 默認 `limit` 是 50
（canonical 默認 20），因為 BFF 是為「冷啟動一次拉一整屏 backscroll」設計的。

```json
{
  "session_id": "…",
  "messages": [
    { "id": "3cc06c53-…", "client_msg_id": "c_abc", "role": "user",      "content": "alpha", "sent_at": "…" },
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "beta",  "sent_at": "…" },
    { "id": "a1b2c3d4-…", "client_msg_id": null,    "role": "assistant", "content": "gamma", "sent_at": "…", "channel": "product_qa" }
  ],
  "total": 3
}
```

`total` 是 **本次** 響應裡 `messages` 的條數（`== messages.len()`），
不是該 session 的總行數。

### `GET /bff/v1/comp/affinity/{session_id}/event`

最近一次用户轮次的好感度 delta（实际应用的每轴变化），供前端做逐轮观测。与
canonical 的 `/comp/affinity/{session_id}` debug 路由不同，它**不受**
`EXPOSE_AFFINITY_DEBUG` 控制（这块归前端所有）——但仍做 JWT + ownership 检查。

查询参数（均可选）：

- `after` —— 长轮询基线：调用方手上已有的 `event_id`。只要该 session 最新的
  用户轮事件仍等于它（或还没有任何事件），请求就被挂起，直到有更新的事件落库
  或 `wait` 超时——超时响应返回未变的状态，形状与立即返回路径相同。缺省 ⇒
  立即返回最新事件。
- `wait` —— 请求最长挂起多少毫秒。仅在带 `after` 时有意义。默认 10000，
  服务端上限 25000。

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.03, "trust": 0.01, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.0, "tension": -0.01
    },
    "effective_deltas_computed": {
      "bond": 0.013,
      "chemistry": 0.006
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "created_at": "…"
  }
}
```

`event` 为 `null` 的情况：还没有任何用户轮次事件（全新 session，或只有
time-decay），或最近一次事件早于 affinity migration `0014`。`event_type`
∈ `message | gift | proactive | ghost`；ghost 轮次的 `effective_deltas`
全为零。

- `effective_deltas_computed` —— 精确的每轮行增量，在持久化时从取下界前后的 bond/chemistry 分数计算得出，存储于事件行。单位为合成分增量——与快照的 `bond`/`chemistry` 同一 0..1 刻度——适合每轮 "+X bond / +Y chemistry" 脉冲显示。迁移前的旧行可能缺省。
- `label_changes` —— 引擎权威的档位变化（本轮无档位跨越时为 `null` / 缺省）。前端无需自行计算变化。

## 錯誤響應

大多数错误是 JSON 形状 `{"error": "<code>", "message": "<人类可读>"}`。
流式路由（`POST /comp/chat/{session_id}/message/stream`、`POST
/comp/voice/{session_id}/turn/stream`、`POST
/persona/{instance_id}/image/compose`）是例外：这三个路由的大多数失败情形
用的是上文"流前错误"段落描述的 `code` / `message` /
`user_message` 形状，没有 `"error"` 字段。`POST
/comp/voice/{session_id}/turn/interrupt` 本身不是流式接口（成功时返回的是
普通 JSON），但复用了语音轮次的前置检查和错误类型，错误响应用的也是这同一套
形状。下表覆盖的是普通形状：

| 狀態碼 | code | 何時 |
|--------|------|------|
| 400 | `bad_request` | 請求體格式錯、UUID 無效、缺必填字段 |
| 401 | `unauthorized` | JWT 缺失 / 格式錯 / 過期 / 密鑰不符 |
| 403 | `forbidden` | 路徑 user 跟 JWT user 不匹配，或想讀別人的 session |
| 404 | `not_found` | session / 人格 / 消息 id 不存在 |
| 500 | `internal` | 其餘一切（DB 錯、LLM API 錯等） |
| 502 | `upstream` | 上游供应商调用失败（目前仅 persona compose 端点——合成器链条全部失败） |

## 源码

- `crates/eros-engine-server/src/routes/companion.rs`——对话生命周期 / 画像 handler
- `crates/eros-engine-server/src/routes/companion_stream.rs`——流式对话轮（`message/stream`），含打赏 + `image_url` 处理
- `crates/eros-engine-server/src/routes/voice.rs`——语音频道轮（`voice/{session_id}/turn/stream`）
- `crates/eros-engine-server/src/routes/persona.rs`——独立图片提示词合成（`/persona/{instance_id}/image/compose`）
- `crates/eros-engine-server/src/routes/bff/companion.rs`——BFF `/bff/v1/comp/chat/*`
- `crates/eros-engine-server/src/routes/bff/affinity.rs`——BFF `/bff/v1/comp/affinity/*`
- `crates/eros-engine-server/src/routes/debug.rs`——好感度 debug 路由（向量 + 事件日志）
- `crates/eros-engine-server/src/routes/health.rs`——`/healthz`
- `crates/eros-engine-server/src/openapi.rs`——Scalar UI spec 元数据
