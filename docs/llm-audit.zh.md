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
  `companion_insights_events`，实验性角色链（见下文"角色画像"一节）的每次
  调用落在 `engine.character_insights_events`，每次 `pde_decision` judge 运行落在
  `companion_decision_events`，每次图片合成器调用落在
  `chat_images_events`，每次 `chat_vision` describe 调用落在
  `chat_vision_events`——详见下面的[图片链路事件表](#图片链路事件表)；
  `chat_messages.metadata.image` 仍保留现有的 `compose_*` 键，只是多加了一个
  指向 `chat_images_events` 的 `compose_event_id` 指针）——参见上面 `usage`
  过滤那节。companion_insights 拆除（spec 2026-08-11）之后，`companion_insights_events`
  的行里可能出现类型化 `human_insights` 表从不落库的 key：抽取器的
  `existing_insights` 上下文现在是从 `human_insights` 反向投影出来的（不再是
  已废弃的 JSONB blob），LLM 吐出的任何 schema 之外的 key 仍然会进事件
  payload，但不会落在其它任何地方——以后做 events↔store 对账时，要按
  payload key 与 `human_insights` 列集合的交集比较，不能直接判等。
- **不 hash。**引擎不会变换 `user` —— caller 负责送 hash。
- **不消毒。**`metadata` 的 key / value 只检查 size / shape，不查内容。
- **不解读。**引擎不会按 audit 字段分组、聚合、报警。Caller 自己接。

## 角色画像（实验特性）

`engine.character_insights_events` 是实验性角色画像链（v1.3.0，
[设计文档](superpowers/specs/2026-08-15-character-insights-design.md)）的审计
轨迹——是 `companion_insights_events` 的镜像，只是对象换成了 AI 角色而不是
真人用户。这正是这条链用两个独立配置任务名
（`character_insight_extraction` / `character_insight_structuring`）而不是像
人类链那样两个 stage 共用一个 `insight_extraction` 的意义所在：这样
OpenRouter 计费和这张表才能分清哪次是抽取调用、哪次是结构化调用。具体来说：

- **`stage`** 取值 `'extraction'` 或 `'structuring'`——直接对应各自的配置块
  名字，看到 `stage='structuring'` 就知道该去调哪个 `[tasks.*]` 块，中间不需要
  查任何映射表。这一点和人类链的 `companion_insights_events.stage`（取值
  `'facts'` / `'structured'`，早于配置拆分那次改动）不一样。
- **同一次抽取的两行共享同一个 `run_id`**——抽取调用和它喂给的结构化调用不
  需要经过 `session_id`/`message_id` 就能直接关联。

## 图片链路事件表

两张 append-only 表分别记录每一次图片合成器调用和 `chat_vision` describe
调用。两张表都是**尽力而为的遥测，不是有保证的账本**：INSERT 会被 await，
但有一个较短的 `AUDIT_WRITE_TIMEOUT` 兜底，出错或超时都会 `warn!` 一条日志
然后丢弃——一次审计写入失败或卡住只是丢掉这一行事件，绝不影响这一轮对话
（与 `companion_decision_events` 同样的纪律）。两张表都没有外键：一行可能
比它指向的东西活得更久，也可能先于它存在。

与 `chat_messages`（从 `chat_sessions` 级联删除）不同，删除一个 session
并不会连带删掉这两张表里的行——两张表都保存着原文用户文本
（`chat_images_events.inputs.latest_user_msg` / `.recent_scene`；
`chat_vision_events.image_url`，往往是带签名、带 token 的 URL）。
**deployer 必须把这两张表纳入自己的用户数据擦除流程**——引擎不会替你做这件
事；擦除策略是 deployer 的责任，不是引擎的。两张表也都没有配套的清理策略：
只要部署一直跑着，行数就会无限增长，如果这对你的部署有影响，请自行规划
分区方案或清理 cron。

### `engine.chat_images_events`

每次图片合成器 LLM 调用一行，来自**任意** caller——聊天轮次里委托的图片
prompt，或者独立端点 `POST /persona/{instance_id}/image/compose` 的任一种
流式模式。

| 列 | 类型 | 含义 |
|---|---|---|
| `id` | `UUID` | 行 id——见下面的关联关系。 |
| `source` | `TEXT` | `chat_reply_text_image` \| `chat_reply_image` \| `compose_endpoint` \| `compose_endpoint_stream`。 |
| `user_id` | `UUID` | |
| `instance_id` | `UUID?` | 角色实例；caller 没有实例上下文时为 NULL。 |
| `session_id` | `UUID?` | 聊天 session；独立端点没有 session，恒为 NULL。 |
| `status` | `TEXT` | `ok` \| `exhausted` \| `not_configured`。 |
| `inputs` | `JSONB` | 合成器的五个槽位，结构化保存：`{appearance, recent_scene, latest_user_msg, style, aspect_ratio}`。空槽位记为 `""`，不是 prompt 渲染时用的 `（无）` 占位符——那个替换是渲染细节，不是输入本身。`latest_user_msg` 按 `source` 不同而不同：`chat_reply_image` 传的是原始的 `user_msg.content`，而 `chat_reply_text_image` 传的是 `effective_user_msg`——input filter 改写之后的版本。两者都如实记录了合成器那一轮实际看到的内容；跨这两个 source 比对行的运维应该预期这个差异，不要当成不一致。 |
| `subject` | `TEXT?` | 合成器自己的 `prompt` 字段。`status = "ok"` 之外恒为 NULL。 |
| `caption` | `TEXT?` | 合成器没写 caption 时为 NULL，包括非 JSON 回退的情形——此时整段回复变成 `subject`。 |
| `composed_prompt` | `TEXT?` | 拼装出的线上 wire 字符串——style 预设 + 角色外观 + subject，也就是下游消费方实际拿到的那个字符串。**每一行只要产出过它就会保存**，包括聊天路径上的 `exhausted` 和 `not_configured`（肖像回退仍会拼出一个 wire prompt，这一列就是唯一记录了当时画的到底是什么的地方）；只有独立端点的 `exhausted` 行是 NULL——它失败时根本没拼装任何东西。 |
| `variant` | `TEXT?` | 解析出的 `prompt_variant` key；`"raw"` 是普通 key，不是跳过标记。 |
| `model` | `TEXT?` | 成功时是应答的模型。`exhausted` 时也可能有值——只要最后一次尝试确实有应答，只是内容不可用：聊天路径和非流式端点共用的 `empty`/`empty_prompt`（都走 `run_image_prompt_compose` 那一套共享的链路遍历），以及独立端点流式模式的 `empty`/`empty_prompt`/`stream_died_midway`。流式模式下，「provider 到底有没有应答」这条判据在候选**开流之前**（即产出第一个内容块之前）也同样适用：peek 循环里，一个候选先流出了 metadata（model/generation_id/usage），之后却没有任何内容就结束，记 `empty`；先流出 metadata、之后在第一个内容块前就死掉，记 `stream_died_midway`——两者都会保留那份 metadata，跟开流之后才出现的同名分支一样。只有在任意路径上完全没有应答回来时才是 NULL：传输层失败（`model_error`/`timeout`），或者流式端点的 `stream_open_failed`——现在它专指候选**什么都没捕获到**的情形（开流本身失败/超时，或者在第一个内容块之前就死掉/超时、且从未收到过任何 metadata 块），又或者 `not_configured`（压根没发起调用）。 |
| `usage` | `JSONB?` | 完整未过滤的 OpenRouter usage 块，`serde_json::to_value` 出来的——`OPENROUTER_USAGE_HIDDEN_KEYS` 只过滤 wire 上那份回显，从不影响这里。与 `model` 同步：`model` 有值的地方它才有值。 |
| `generation_id` | `TEXT?` | 与 `model` 同步。 |
| `attempts` | `SMALLINT` | 实际调用了 `[primary, ...fallback]` 里多少个模型；`not_configured` 时为 `0`。 |
| `last_failure` | `TEXT?` | 最后一次尝试为什么失败；`status = "ok"` 或 `"not_configured"`（压根没发起尝试，也就无所谓失败）时为 NULL。取值：`model_error` \| `timeout` \| `empty` \| `empty_prompt` \| `stream_open_failed` \| `stream_died_midway`。自由文本列，不是 CHECK——这个词表会随新失败模式的出现而增长。`stream_open_failed` / `stream_died_midway` 只出现在独立端点的流式模式；聊天路径和该端点的非流式模式共用同一套链路遍历逻辑，只会报另外四个值。**`empty` 在流式模式下也能出现**，不止链路遍历那两条路径——一个候选压根没开流（没有内容块），但确实先流出过一个 metadata 块，也记 `empty`，跟链路遍历里内容层面「空回复」那一支同名，因为两者说的是同一件事：provider 应答了，可能已经计费。 |
| `created_at` | `TIMESTAMPTZ` | |

`status` 刻意只有三个值：`exhausted` 表示「这次调用没产出可用的合成结果」，
涵盖包括流式端点中途死掉在内的所有原因，具体区分交给 `last_failure`。

**在当前的门控下，`not_configured` 在这张表上不可达。**
`build_delegated_image_prompt` 只在 `ReplyImage`/`ReplyTextImage` 时跑，而
action-plan 的 guard 只有在配置了 `[tasks.chat_image_prompt_compose]`
时才会产出这两个 action（`model_config` 是启动时定死的 `Arc`，不支持热加载）；
一次没配置该 task 的强制出图请求，会在路由层就 422，根本走不到合成器调用。
非流式端点同样会在缺任务时先 501，不做任何实际工作。这个取值继续留在
CHECK 里——事后收窄 CHECK 要过一次 migration，而这个仓库的一贯原则是保留
能力而不是拔掉它——它是留给未来某个不受此门控约束的调用方的，不是你今天在
线上部署里会实际观察到的状态。对照下面的 `chat_vision_events`：那张表的
`not_configured` 既可达，还是最有价值的一个状态。两张表不要混着理解。

**没有 `message_id` 列。** 合成器在 assistant 行存在*之前*就跑完了——聊天
路径上合成是在聊天调用之前 `tokio::spawn` 出去的，好让它的延迟藏在聊天调用
底下，assistant 的 message id 要到后面的 join 点才会真正出现。与其把审计
写入推迟到那个 join 点，合成器选择**先写自己的事件行、返回行 id**，随后把
这个 id 盖到 assistant 行的 `chat_messages.metadata.image.compose_event_id`
上。反向查找是 **assistant 行 → `compose_event_id` → 这张表**，绝不是反过来；
`chat_messages.metadata.image` 原有的 `compose_variant` / `compose_model` /
`compose_generation_id` 三个键保持不变。这个方向还让表对完全没有消息可挂的
调用方也保持可达（独立端点的 `session_id` 恒为 NULL 正是这个原因），并且
**在聊天路径上**扩大了覆盖面：因为 `build_delegated_image_prompt` 在返回给
调用方之前就写好了这一行——独立于聊天 SSE 流本身——一次聊天轮次里图片最终
没有发出去（客户端断连，或者调用返回之后才触发的 ghost 回退），这一行照样
会留下来——「模型调用已经付费，图片没有发出去」变得可见。**这个保证不延伸
到独立端点自己的流式模式**（`compose_endpoint_stream`）：它的
`record_compose_event` 调用是写在 SSE generator 内部的
（`routes/persona.rs::compose_stream`），客户端如果在 generator 走到某次写入
之前就断连，那一行照样会丢——即使那次调用已经计费了。该端点的非流式模式
（`compose_endpoint`）在 HTTP 响应返回之前就同步写完了，不受这个问题影响。

### `engine.chat_vision_events`

每一个带图片、且不是打赏、**并且走到了文字回复路径**的聊天轮次一行，
记录这次 `chat_vision` describe 调用。没有图片的轮次不写任何行——
「带图片」是分母，文字轮次不算一次「漏掉的 describe」。压根没走到文字
回复路径的轮次同样不写：被 ghost 掉的轮次、路由去 `product_qa` 的轮次、
纯图片回复的轮次都会跳过这次写入——因为 describe 在这些路径上本来就不会
跑（在 ghost 轮次上跑一次 describe 只是白白多花一次调用的钱）。用这张表
算 describe 成功率时，分母要按「走到了文字回复路径的带图轮次」算，不是
所有带图轮次——不排除掉 ghost / product_qa / 纯图片回复轮次的查询会高估
覆盖率。

| 列 | 类型 | 含义 |
|---|---|---|
| `id` | `UUID` | |
| `user_id` | `UUID` | |
| `session_id` | `UUID` | |
| `message_id` | `UUID` | 携带这张图片的 `role='user'` 行。 |
| `status` | `TEXT` | `ok` \| `exhausted` \| `not_configured`。 |
| `image_url` | `TEXT` | |
| `vision` | `JSONB?` | 解析出的 describe 结果（`description` / `ocr_text` / `people` / `scene`）。成功时与 `chat_messages.metadata.vision` 重复——这份冗余的代价是可以接受的：不用 join `chat_messages` 建立分母，这张表就能直接回答「跑了多少次 describe、describe 的是什么、成功率多少」。 |
| `model` | `TEXT?` | 成功时是应答的模型。`exhausted` 时也可能有值——只要最后一次尝试确实有应答，只是内容不可用（`empty` / `unparseable` / `content_filter` / `blank_description` / `refusal_pattern`）。只有纯传输层失败（`model_error` / `timeout`，压根没有应答回来）或 `not_configured`（压根没发起调用）时才是 NULL。 |
| `usage` | `JSONB?` | 完整未过滤的 usage 块，规则同 `chat_images_events.usage`。 |
| `generation_id` | `TEXT?` | |
| `attempts` | `SMALLINT` | 实际调用了 `[primary, ...fallback]` 里多少个模型；`[tasks.chat_vision]` 未配置时为 `0`。 |
| `last_failure` | `TEXT?` | `status = "ok"` 或 `"not_configured"` 时为 NULL。取值：`model_error` \| `timeout` \| `empty` \| `unparseable` \| `content_filter` \| `blank_description` \| `refusal_pattern`——最后三个直接复用 `image_vision_invalidity` 现成的 reason 字符串。 |
| `created_at` | `TIMESTAMPTZ` | |

**保留 `message_id`，故意打破与 `chat_images_events` 的对称。** 两张表的
结构本来就不一样：vision 是在 `role='user'` 行已经存在*之后*才跑的，
`message_id` 那时已经在手上，而且只有唯一一个调用点，不存在独立 vision
入口的可能。用户随附的文字**不会**在这里重复保存——`message_id` 指向
`chat_messages.content`，要看原文去那边查。

即使 describe 根本没跑，也会写一行——`not_configured`，`attempts = 0`——
因为这是 `chat_messages.metadata.vision` 缺失的三种原因之一，也是唯一能把
它和「describe 跑了但每个 chain 模型都失败了」（`exhausted`）区分开的办法。

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
