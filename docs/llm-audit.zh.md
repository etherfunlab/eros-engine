# LLM audit 透传

eros-engine 在流式 chat 路由上暴露一个不透明的 OpenRouter 透传层。
三个 caller 提供的字段原样发给
`openrouter.ai/api/v1/chat/completions`，三个 OpenRouter 的 wire 回显
在 SSE `done` 帧里带回来，一个 deployer 声明的 `headers` 表会给每次出站
调用都带上 app-attribution headers。

引擎不解读内容。PII 脱敏、hash、metadata 语义都是 caller 的责任。

## Inbound：请求体的 `audit` 字段

`POST /comp/chat/{session_id}/message/stream` 在必填的 `content` /
`client_msg_id` 之外，接受可选的 `audit` 对象：

```jsonc
{
  "content": "...",
  "client_msg_id": "01J3333333333333333333333A",
  "audit": {
    "user": "u_<hash-of-internal-id>",     // 可选
    "session_id": "conv_xyz",               // 可选，与 URL 上的 session UUID 不同
    "metadata": {                           // 可选
      "feature": "chat",
      "plan": "pro"
    }
  }
}
```

引擎在转发前强制的上限：

| 字段                    | 上限                                                  |
|-------------------------|-------------------------------------------------------|
| `audit.user`            | `chars ≤ 256`                                         |
| `audit.session_id`      | `chars ≤ 256`                                         |
| `audit.metadata` key 数 | `≤ 16`                                                |
| `audit.metadata` key    | 正则 `^[A-Za-z0-9_.-]{1,64}$`                         |
| `audit.metadata` value  | JSON string，`chars ≤ 512`                            |

违反作为 pre-stream 错误返回 `400 BadRequest`，且不会写任何 user
message 行。

**隐私：**不要把原始 email / 钱包地址 / 真实姓名放进 `user` ——
要送 hash。OpenRouter 默认保留 request metadata（token 数、延迟），
但不保留 prompt / response 内容。

## Outbound：SSE `done` 帧里的 `usage` 回显

流式端点的 `done` 帧带三个可选字段：

| 字段            | 类型      | 含义                                                                                       |
|-----------------|-----------|--------------------------------------------------------------------------------------------|
| `usage`         | `object?` | OpenRouter 的 `usage` 块原样（tokens / cost / cached / reasoning）。引擎不展平。           |
| `generation_id` | `string?` | OpenRouter `response.id`。之后可以用它查 `/api/v1/generation` 拿完整 metadata。          |
| `model`         | `string?` | OpenRouter 实际服务的模型。`fallback_model` 命中时，这里是 fallback。                    |

这三个字段出现在 `done` 帧（`final` 之前的 per-turn 终止帧）。后台路径
（dreaming / post_process）**不**把它们返回给 client。

### 从响应里剔除字段

Deployer 可以在服务器上设
`OPENROUTER_USAGE_HIDDEN_KEYS`（逗号分隔）来把 `usage` 回显里指定的顶层
key 剔除掉。典型用途：把批发的 `cost` / `cost_details` 对下游客户隐藏，
同时不影响运维侧可见性。

```bash
OPENROUTER_USAGE_HIDDEN_KEYS=cost,cost_details
```

行为：

- 对 SSE 流式 `done` 帧生效，覆盖 `/comp/chat/{id}/message/stream`
  和 `/comp/voice/{session_id}/turn/stream` 两个路由。
- 完整未过滤的 `usage` 仍会落库；只过滤面向 client 的负载。
- **不**影响 `tracing::info!` 输出 —— 运维可见性照旧。
- 后台路径（dreaming / post_process）本来就不把 `usage` 返回给
  client，env 设了也没区别。
- 只剔除顶层 key；要把整个子树抹掉就列父 key（`cost_details` 会把整个
  对象删掉，而不是只删它内部的字段）。
- 未设或空 → 维持现状（完整透传）。

后台路径（`pipeline::dreaming`、`pipeline::post_process`、
`pipeline::world`、`pipeline::story`）的 usage 只通过 `tracing::info!` 字段输出：

```
openrouter: call completed session=… generation_id=… model=…
prompt_tokens=… completion_tokens=… total_tokens=… cost=…
```

- `world_director` —— World Memories 导演清扫（后台）。每个已加入的 owner 每
  `interval_hours` 调一次。`user` = `11111111-1111-1111-1111-111111111112`
  （world 子系统哨兵，与 dreaming 的 `11111111-1111-1111-1111-111111111111`
  不同）。Usage/cost 通过 tracing 字段输出，走
  `log_openrouter_usage("world_director", None, …)`；不出现在任何 client 帧上。
- `world_comment` —— World Town 每小时评论轮（后台）。每个有新动态的 owner
  批量调一次。`user` = `11111111-1111-1111-1111-111111111112`（world 子系统
  共享哨兵）。Usage/cost 通过 tracing 字段输出，走
  `log_openrouter_usage("world_comment", None, …)`；不出现在任何 client 帧上。
- `world_reply` —— World Town 回复响应器（后台）。每条经防抖的用户留言调一
  次，按 owner 每 UTC 自然日封顶。同一个哨兵 user；usage/cost 通过 tracing
  字段输出，走 `log_openrouter_usage("world_reply", None, …)`；不出现在任何
  client 帧上。
- `world_stories_director` —— World Stories 导演（后台），模块
  `pipeline::story`。作为 `world_director` 同一个 sweeper tick 的第二阶段
  运行；每个被认领的 persona instance 按自己的 `interval_hours` 调一次。
  `user` = `11111111-1111-1111-1111-111111111113`（story 子系统哨兵，延续
  dreaming/world 的序列）。Usage/cost 通过 tracing 字段输出，走
  `log_openrouter_usage("world_stories_director", None, …)`；不出现在任何
  client 帧上。

## App-attribution headers

在模型配置的 `[providers.openrouter]` 下声明一个 `headers` 表（见
`docs/model-config.zh.md` → “通过 `[providers].openrouter` 覆盖内置
端点”），就能给每次出站 OpenRouter 调用加 header：

```toml
[providers.openrouter]
headers = {
  "HTTP-Referer" = "https://eros.example",
  "X-OpenRouter-Title" = "Eros",
  "X-OpenRouter-Categories" = "companion,roleplay",
}
```

| 常用 header                | 用途                                          |
|-----------------------------|-----------------------------------------------|
| `HTTP-Referer`              | OpenRouter 仪表盘上的 app 标识                |
| `X-OpenRouter-Title`        | OpenRouter app analytics 里显示的名字         |
| `X-OpenRouter-Categories`   | 逗号分隔的 marketplace 分类                   |

没有 `[providers.openrouter]` 条目，或者有条目但没写 `headers` → 维持
现状（不发任何 attribution header）。这是 deployment 级别的设置，不是
per-request —— App-Attribution 的目的是 app-level 聚合。Per-user 维度走
`audit.user`。

`X-OpenRouter-Categories` 原样透传；OpenRouter 对无法识别的值静默忽略，
且只有在同时设了 `HTTP-Referer` 时才生效。

header 的 name/value 在启动时就会校验：引擎自有的 name
（`Authorization`、`Content-Type`，不分大小写）或者不合法的 HTTP header
material 会直接拒绝加载，而不是像以前那样在构造时 warn-and-drop。

**迁移提示：**`OPENROUTER_APP_REFERER` / `OPENROUTER_APP_TITLE` /
`OPENROUTER_APP_CATEGORIES` 环境变量已软废弃——仍然设置也会被静默忽略，
不是启动报错，但也不再起任何作用。请把同样的 header 按上面的写法迁到
`[providers.openrouter].headers` 下。

## 引擎不做的事

- **不持久化 `audit` 对象。**没有任何 DB 列保存 caller 传来的 `audit`
  （`user` / `session_id` / `metadata`）或 attribution 头——它们只是 surface
  字段，转发上游之后就丢弃。但 OpenRouter 的 `model` / `usage` /
  `generation_id` 三元组**是**会落库的：聊天补全落在
  `chat_messages.model` / `.usage` / `.generation_id`（好感度评估镜像在
  `companion_affinity_events`，每次 `insight_extraction` 调用落在
  `companion_insights_events`，每次 `pde_decision` judge 运行落在
  `companion_decision_events`，合成器和 vision 调用落在
  `chat_messages.metadata.image` / `.vision*`）——参见上面 `usage` 过滤那节。
  companion_insights 拆除（spec 2026-08-11）之后，`companion_insights_events`
  的行里可能出现类型化 `human_insights` 表从不落库的 key：抽取器的
  `existing_insights` 上下文现在是从 `human_insights` 反向投影出来的（不再是
  已废弃的 JSONB blob），LLM 吐出的任何 schema 之外的 key 仍然会进事件
  payload，但不会落在其它任何地方——以后做 events↔store 对账时，要按
  payload key 与 `human_insights` 列集合的交集比较，不能直接判等。
- **不 hash。**引擎不会变换 `user` —— caller 负责送 hash。
- **不消毒。**`metadata` 的 key / value 只检查 size / shape，不查内容。
- **不解读。**引擎不会按 audit 字段分组、聚合、报警。Caller 自己接。

## 可观测性

除了主聊天回复（`chat_companion`）和语音回合（`chat_voice`）之外，每次
成功的 OpenRouter 调用，引擎都会打一条 info 级别日志，带
`generation_id` / `model` 以及 best-effort 解析出来的 token / cost。这
两个调用量最大的任务不走这条日志——它们各自的 per-attempt 日志是一条
`stream_metrics` 事件（`model` / `attempt` / `ttft_ms` / `total_ms` /
`outcome`），没有 `generation_id` 也没有 cost 明细。`audit` 对象本身
不写入日志——它只转发给上游，从不回写进引擎日志。

## 为什么不持久化？

引擎的 persona / chat / affinity 表是长期契约。Audit 上下文有意做成
短暂的，让 caller 可以自由实验 `user` hash、metadata schema、不同
deployment 的 analytics，不污染引擎表也不需要 migration。
