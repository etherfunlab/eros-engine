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
  "version": "0.6.x",
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
  "persona_name": "Aria",
  "is_new": true
}
```

`is_new=false` 表示同一用戶用同一個 `genome_id` 再調 `/start`——引擎恢復已有 session 而不是建重複的。

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

data: {"type":"final","lead_score":0.42,"should_show_cta":false,"agent_training_level":0.18,"filtered":false,"prompt_injected":null,"tier":null,"retries_chat":0,"retries_filter":0}
```

帧字段说明：

- **`meta`** —— `message_id`、`action_type`、`model`（实际服务的模型 id，可能省略），以及 `continues_from`（可选，本轮续接重试链时为上一条消息 id）。
- **`done`** —— `truncated`、`usage`（经 `OPENROUTER_USAGE_HIDDEN_KEYS` 过滤后，可能省略）、`generation_id`（可选的 OpenRouter id）。
- **`final`** —— 本轮汇总：`lead_score`、`should_show_cta`、`agent_training_level`，外加 `filtered`（bool，回复是否被输出过滤）、`prompt_injected`（本轮注入的 trait tag 数组，无则为 `null`）、`tier`（回显请求的 `tier`，未传为 `null`）、`retries_chat`（命中的对话尝试下标，从 0 起）、`retries_filter`（实际服务的过滤模型尝试下标）。

每个用户最多 3 条并发活跃流。保活心跳（`: ping`）每 15 秒发一次，
防止反向代理因空闲超时断开连接。

流前错误（第一个 SSE 字节写出之前的 HTTP 4xx/5xx）携带含 `code`、
`message`、`user_message` 字段的 JSON 响应体；`409 duplicate_in_progress`
时还会带 `original_user_message_id`。完整错误码表见
[设计文档](superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md#13-pre-stream-errors-http-status-json-body)。

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
`400 BadRequest`。仅当 `[tasks.chat_vision]` 配了非空 `filter_prompt` 时
vision 才生效（见 [model-config.md](model-config.md)）。

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

**可选：伴侣图片回复。** 请求体可附加 `image` 对象（`ImageReplyParams`），请求或强制本轮生成一张伴侣发送的图片。需要配置 `[tasks.chat_image_generation]`（见 [model-config.zh.md](model-config.zh.md)）；执行器默认关闭。`image` 块同时是本轮的 opt-in 开关：**省略它即可关闭本轮的图片生成**（此时 PDE 只能 `reply_text` / `ghost`），或发送 `image: {}` 用配置里的模型启用。这样调用方可以用自己的 per-turn 策略独立于 PDE 的内容决策来控制是否出图。

```bash
curl -N -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{
        "content": "给我看个笑脸",
        "client_msg_id": "01J3333333333333333333333A",
        "image": {
          "force": true,
          "mode": "text_image",
          "style": "realistic",
          "model": "google/gemini-2.5-flash-image",
          "image_prompt": "温暖随拍自拍，室内柔光",
          "aspect_ratio": "3:4",
          "resolution": "1024x1365",
          "face_ref_url": "https://cdn.example/aria_avatar.png"
        }
      }' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

`ImageReplyParams` 字段（全部可选）：

| 字段 | 类型 | 默认值 | 备注 |
|---|---|---|---|
| `force` | `bool` | `false` | 强制本轮发图，覆盖 PDE 决策。`false` 时由 PDE 决定。 |
| `mode` | `"text_image"` \| `"image_only"` | `"text_image"` | `text_image` = 文字 + 图片；`image_only` = 仅图片（允许空 `content`）。 |
| `style` | `"realistic"` \| `"semi_realistic"` \| `"anime"` | 任务 `default_style` | 引擎内置三种风格预设之一。 |
| `model` | `String` | 配置值 | 覆盖配置 `ModelSpec` 的单轮 id。优先于配置 `model`，仍可回退到配置 `fallback`。 |
| `image_prompt` | `String` | PDE 判断 / 用户文本 | 强制路径的图片主题。PDE 路径使用判断器自己的 `image_prompt`。 |
| `aspect_ratio` | `String` | 任务 `default_aspect_ratio` | 允许值：`1:1`、`3:4`、`4:3`、`9:16`、`16:9`。非法时返回 `422`。 |
| `resolution` | `String` | 任务 `default_resolution` | 模型相关的分辨率提示（如 `"1024x1365"`）。仅做形状校验，透传给模型。 |
| `face_ref_url` | `String` | 缺失 | 图生图面部参考图（绝对 `http(s)` URL，≤ 2048 字符）。格式非法时返回 `422`。 |
| `prev_image_url` | `String` | 缺失 | 上一张生成的图片，用于迭代续图（绝对 `http(s)` URL，≤ 2048 字符；校验同 `face_ref_url`）。仅当 PDE 选择 `image_ref = "previous"`（见下）时使用，否则忽略。私有对象存储的调用方应传一个短时效签名 URL——引擎不会去拉取它，而是把 URL 嵌进 OpenRouter 请求体，由画图供应商在生成时拉取。格式非法时返回 `422`。 |

**参考图选择（`image_ref`）。** PDE verdict 带有 `image_ref`（`"face"` \| `"previous"`，默认 `"face"`）。出图时引擎据此选参考图：`previous` 且带有 `prev_image_url` ⇒ 在上一张图上迭代；否则回退到 `face_ref_url`（头像）。所选类型记录在 `metadata.image` 中。

校验：同一轮同时有 `force` 和 `tips_amount_usd` → `422`。`face_ref_url` 或 `prev_image_url` 格式错误、`aspect_ratio` 不在允许集、`resolution` 形状错误，均作为 pre-stream 错误返回 `422 BadRequest`。

**`image` SSE 帧** — 图片生成成功时，在文字的 `done` 帧之后发出：

```text
data: {"type":"image","message_id":"01J...","data_url":"data:image/png;base64,...","mime":"image/png","image_prompt":"温暖随拍自拍，室内柔光","model":"google/gemini-2.5-flash-image","generation_id":"gen-xyz"}
```

| 字段 | 类型 | 备注 |
|---|---|---|
| `type` | `"image"` | 帧类型标识符。 |
| `message_id` | `String` | 与 `meta` 帧的 `message_id` 相同。 |
| `data_url` | `String` | 生成图片的 base64 data-URL（`"data:image/png;base64,..."`）。 |
| `mime` | `String` | 图片 MIME 类型（如 `"image/png"`）。 |
| `image_prompt` | `String` \| `null` | 生成时使用的主题（也会持久化）。 |
| `model` | `String` \| `null` | 实际服务的图片模型。 |
| `generation_id` | `String` \| `null` | OpenRouter 生成 id。 |

**完整 SSE 帧序列：**

- `reply_text_image`：`meta(action_type=reply_text_image) → delta* → done → image → final`
- `reply_image`：`meta(action_type=reply_image) → image → done → final`
- `ghost`：`meta(action_type=ghost) → done → final` — 无 `delta`，`meta` 中无 `model`，`done` 的 `usage` 和 `generation_id` 均为 `null`。该轮伴侣保持沉默，未调用任何 LLM。

**图片失败客户端约定** — 图片失败时不会发出额外的 error 帧。客户端通过 `meta` 帧的 `action_type` 判断预期形状：

- **`reply_text_image`** — `image` 帧在 `done` 之后到达。若流已到达 `final` 但仍未收到 `image` 帧，则图片生成失败（fail-open）；文字已正常投递，渲染即可。
- **`reply_image`** — `image` 帧在 `done` 之前到达。`reply_image` 类型的 `meta` 只在图片确定可投递时才会下发，因此收到该 `meta` 后 `image` 帧必然随后出现。若图片失败，整轮会降级：客户端收到的是 `meta.action_type=reply_text`（而非 `reply_image`）加上普通文字流——降级从 `meta` 帧起即可见，不会出现空的 `reply_image`。

**写回端点** — 收到 `image` 帧后，客户端应将 `data_url` 上传到自有存储，然后把结果 URL 写回引擎：

### `POST /comp/chat/{session_id}/message/{message_id}/image`

存储伴侣生成图片的 CDN URL。由客户端将 `data_url` 上传到自有存储后调用。

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"url":"https://cdn.example/gen/abc.png"}' \
  http://localhost:8080/comp/chat/<session_id>/message/<message_id>/image
```

成功返回 `204 No Content`。`url` 必须是绝对 `http(s)` URL（≤ 2048 字符）。URL 格式错误返回 `422`，`message_id` 不是本 session 的 assistant 行返回 `404`，session 不属于 JWT 用户返回 `403`。该调用幂等——重复 POST 会覆盖同一个键。

### `GET /comp/chat/{session_id}/history?limit=50&offset=0`

分頁讀消息歷史，最新在前。

```json
{
  "messages": [
    { "id": "…", "role": "assistant", "content": "Bishop。", "sent_at": "…" },
    { "id": "…", "role": "user",      "content": "嗨…",     "sent_at": "…" }
  ]
}
```

`role` ∈ `user | assistant | gift_user | system_error`。`gift_user` 是打赏轮
（通过上面 stream 路由的 `tips_amount_usd` 发起）。

## 用戶畫像

### `GET /comp/chat/{user_id}/sessions`

該 `user_id` 名下的所有 chat sessions。路徑裡的 `user_id` **必須** 等於 JWT 裡的 user_id；否則 403。

### `GET /comp/user/{user_id}/profile`

當前的 `companion_insights` JSONB 加上加權的 `training_level`。同樣的 `user_id` 等值檢查。

```json
{
  "insights": {
    "city": "Hong Kong",
    "occupation": "graphic designer",
    "interests": ["jazz", "long walks"],
    "mbti_guess": "INFP"
  },
  "training_level": 0.42
}
```

`training_level` 是九個字段加權後的分數（city 0.05、occupation 0.05、interests 0.10、mbti_guess 0.15、love_values 0.15、emotional_needs 0.15、life_rhythm 0.10、personality_traits 0.15、matching_preferences 0.10）。權重總和為 1.0。

> **打赏取代了礼物事件。** 独立的礼物路由
> （`POST /comp/chat/{session_id}/event/gift`、`GET /comp/chat/{session_id}/gifts`）
> 已移除。打赏现在是普通 stream 轮的一部分 —— 在
> `POST /comp/chat/{session_id}/message/stream` 上设 `tips_amount_usd`（见上文）。

## Debug

### `GET /comp/affinity/{session_id}`

实时 6 轴向量 + Bond/Chemistry 进度条与标签 + ghost 统计 + 遗留关系标签。受 `EXPOSE_AFFINITY_DEBUG=true` 环境变量控制；关闭时返 404。

```json
{
  "warmth": 0.42,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.55,
  "tension": 0.04,
  "bond": 0.32,
  "chemistry": 0.28,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` —— 进度条值（0–1，曲线映射后）。
- `bond_label` ∈ `acquaintance | friend | close_friend | confidant`
- `chemistry_label` ∈ `spark | flirtation | crush | lover`
- `relationship_label` —— 遗留映射值（`stranger | friend | slow_burn | romantic`；`frenemy` 已停止输出）。

生产部署通常关着。若前端需要渲染实时雷达图或检查衍生线，再打开。

### `GET /comp/affinity/{session_id}/event?limit=20&offset=0&event_type=message`

该 session 的好感度**事件日志**，分页、最新在前。和向量路由一样受
`EXPOSE_AFFINITY_DEBUG=true` 控制（关闭时 404）。每条同时带原始的每轮
`deltas`（EMA 前）、实际应用的 `effective_deltas`（EMA 后）、折叠后的
`effective_deltas_computed`，以及档位跨越时的 `label_changes`。`event_type`
可选用于过滤；`limit` 默认 20（上限 100）。

```json
{
  "events": [
    {
      "event_id": "…",
      "event_type": "message",
      "deltas":           { "warmth": 0.06, "trust": 0.02, "intrigue": 0.0, "intimacy": 0.0, "patience": 0.0, "tension": -0.02 },
      "effective_deltas": { "warmth": 0.03, "trust": 0.01, "intrigue": 0.0, "intimacy": 0.0, "patience": 0.0, "tension": -0.01 },
      "effective_deltas_computed": { "bond": 0.02, "chemistry": 0.006 },
      "label_changes": null,
      "created_at": "…"
    }
  ]
}
```

`event_type` 过滤可取 `message | gift | proactive | ghost | time_decay`
（`time_decay` 为预留，当前代码不写入）。若要一个**不受** debug 开关控制、
只返回最新一条（仅 EMA 后）的前端用面板，用下面的 BFF 路由
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
`role = gift_user` 时出现，否则省略）。`id` 是 `chat_messages` 行的主鍵（UUID）；
`client_msg_id` 是前端串流時帶上的 id（沒帶的行為 `null`，例如 assistant 回合）。
鑒權、ownership 檢查、`limit ∈ [1, 50]` 夾取
都與 canonical history 路由相同。**刻意差異：** 默認 `limit` 是 50
（canonical 默認 20），因為 BFF 是為「冷啟動一次拉一整屏 backscroll」設計的。

```json
{
  "session_id": "…",
  "messages": [
    { "id": "3cc06c53-…", "client_msg_id": "c_abc", "role": "user",      "content": "alpha", "sent_at": "…" },
    { "id": "9f2e7a10-…", "client_msg_id": null,    "role": "assistant", "content": "beta",  "sent_at": "…" }
  ],
  "total": 2
}
```

`total` 是 **本次** 響應裡 `messages` 的條數（`== messages.len()`），
不是該 session 的總行數。

### `GET /bff/v1/comp/affinity/{session_id}/event`

最近一次用户轮次的好感度 delta（post-EMA），供前端做逐轮观测。与
canonical 的 `/comp/affinity/{session_id}` debug 路由不同，它**不受**
`EXPOSE_AFFINITY_DEBUG` 控制（这块归前端所有）——但仍做 JWT + ownership 检查。

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

- `effective_deltas_computed` —— `effective_deltas` 折叠到 Bond/Chemistry 两条线（`Δbond = (Δwarmth + Δtrust + Δintrigue) / 3`，Chemistry 同理）。单位为原始合成增量（非进度条百分比），适合每轮 "+X bond / +Y chemistry" 脉冲显示。迁移 0014 之前的事件中缺省。
- `label_changes` —— 引擎权威的档位变化（本轮无档位跨越时为 `null` / 缺省）。前端无需自行计算变化。

## 錯誤響應

所有錯誤都是 JSON 形狀 `{"error": "<code>", "message": "<人類可讀>"}`：

| 狀態碼 | code | 何時 |
|--------|------|------|
| 400 | `bad_request` | 請求體格式錯、UUID 無效、缺必填字段 |
| 401 | `unauthorized` | JWT 缺失 / 格式錯 / 過期 / 密鑰不符 |
| 403 | `forbidden` | 路徑 user 跟 JWT user 不匹配，或想讀別人的 session |
| 404 | `not_found` | session / 人格 / 消息 id 不存在 |
| 500 | `internal` | 其餘一切（DB 錯、LLM API 錯等） |

## 源碼

- `crates/eros-engine-server/src/routes/companion.rs`——对话生命周期 / 画像 handler
- `crates/eros-engine-server/src/routes/companion_stream.rs`——流式对话轮（`message/stream`），含打赏 + `image_url` 处理
- `crates/eros-engine-server/src/routes/bff/companion.rs`——BFF `/bff/v1/comp/chat/*`
- `crates/eros-engine-server/src/routes/bff/affinity.rs`——BFF `/bff/v1/comp/affinity/*`
- `crates/eros-engine-server/src/routes/debug.rs`——好感度 debug 路由（向量 + 事件日志）
- `crates/eros-engine-server/src/routes/health.rs`——`/healthz`
- `crates/eros-engine-server/src/openapi.rs`——Scalar UI spec 元數據
