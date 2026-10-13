# 群聊

[English](group-chat.md) · [中文](group-chat.zh.md)

一位用户和至多三位角色在同一场对话里聊天。用户说一句话，有理由回应的角色按顺序开口，
每位都用自己的声音，带着自己和用户已有的关系。后面的发言者能听到前面的发言者刚说了什么。

群不会自己建立关系。每位角色的好感度留在它的 1:1 会话里，群里只读取它。

## 群是什么

- 群是一条 `channel = 'group'` 的 chat session，没有角色实例（`instance_id` 为 NULL）。
  它沿用其他 session 的轮次队列、历史游标、已读标记、图片请求恢复和 `llm_generations` 归因。
- 成员是调用者自己的 1:1 文本 session，**至多三个**（`MAX_GROUP_MEMBERS`）。成员顺序按
  `joined_at`。同一个角色实例在一个群里只能出现一次。
- 群有一个可空的 `title`：去掉首尾空白后 1 到 64 个字符，不含控制字符。没有标题的群仍然是群，
  客户端可以用成员名字拼一个显示名。`PATCH` 修改标题，`null` 清空。
- 解散群就是设置 `archived = true`，成员 session 不受影响。
- 归档一段关系（`DELETE /comp/instance/{instance_id}/sessions`）会在同一个事务里把该角色从所有群里
  移除，连同它的好感度和记忆。之后的轮次只在剩下的成员里选发言者。
- 群的存储是 `engine.chat_sessions`（`channel = 'group'`、`title`）、
  `engine.chat_session_members`（群、成员、`joined_at`）和
  `engine.chat_messages.speaker_session_id`（每条 assistant 消息是谁说的）。

## 路由

所有路由都要求 JWT 用户拥有该群，否则 `404 group_not_found`。这些路由的拒绝响应体都是
`{code, message, user_message}`。

### 管理

| 方法 | 路径 | 请求体 / 效果 |
|---|---|---|
| `POST` | `/v2/comp/group` | `{title?, member_session_ids: [1..=3]}` → 群 DTO |
| `GET` | `/v2/comp/groups` | 调用者未归档的群，按 `last_active_at` 倒序 |
| `GET` | `/v2/comp/group/{group_id}` | 群 DTO |
| `PATCH` | `/v2/comp/group/{group_id}` | `{title}`；`null` 清空 |
| `POST` | `/v2/comp/group/{group_id}/members` | `{member_session_id}` |
| `DELETE` | `/v2/comp/group/{group_id}/members/{member_session_id}` | 踢出成员 |
| `DELETE` | `/v2/comp/group/{group_id}` | 解散 |

群 DTO：

```json
{"group_id": "…", "title": "周末爬山",
 "members": [{"member_session_id": "…", "instance_id": "…", "joined_at": "…"}],
 "created_at": "…", "last_active_at": "…"}
```

创建与加人时的拒绝情形：

| 条件 | 响应 |
|---|---|
| 群里已有三位成员 | `409 group_full` |
| 成员 session 不存在或不属于调用者 | `404 session_not_found` |
| 成员 session 不是 `text` session | `409 wrong_channel` |
| 成员 session 已归档 | `409 session_archived` |
| 另一位成员是同一个角色实例 | `409 instance_already_member` |
| 标题为空白、过长或含控制字符 | `422 invalid_title` |
| 创建时 `member_session_ids` 为空 | `422 unprocessable` |

踢出一个不在群里的 session 返回 `404 member_not_found`。

被踢出的角色过去说过的话仍通过 `speaker_session_id` 保持归属。解散群时，它产生的记忆行保留：
它们属于成员各自的关系。

### 发送

`POST /v2/comp/group/{group_id}/message/stream` → SSE。只有流式，没有异步版本。

请求体：`content`、`client_msg_id`、`tier`、`prompt_traits`、
`user_timezone` / `user_country` / `user_region`、1:1 的 `image` 块、1:1 的 `audit` 块，
以及可选的 `mentions: [member_session_id]`（客户端 @ 用）。

- 点名必须是群里的成员，未知 id 返回 `422 unprocessable`。
- `audit` 沿用 1:1 的上限（超限返回 `400 invalid_payload`），随每位发言者的回复调用发出；
  见 [llm-audit.zh.md](llm-audit.zh.md)。
- `image` 的含义与 1:1 路由相同：带了它，并且配置了 `[tasks.chat_image_prompt_compose]`，本轮就可以发图。
  `image.force = true` 返回 `400 invalid_payload`，因为群聊轮次没有可以强制的单个角色。
- 带 `byok` 块返回 `400 invalid_payload`。
- 群里没有成员时 `409 group_empty`；session 不是群时 `409 wrong_channel`。
- 群的流计入每用户的并发流上限，达到上限时 `429 rate_limited`。
- 重发同一个 `client_msg_id`：轮次还在进行时返回 `409 duplicate_in_progress`。轮次结算之后则是重放：
  按落库的每一行重发 `meta`（带 `speaker_session_id`）、`delta` 和 `done`，最后是 `final`；
  如果每一行都是失败的尝试，最后是一个终止性的 `error`。重放不带 `cast`、`reasoning`、
  `speaker_error` 和 `image_request`。轮次终止性失败且没有任何 assistant 行时返回
  `409 duplicate_failed`，需要换一个新的 `client_msg_id`。

### 原样复用的路由

下列路由原样接受群 session id：

- `GET /comp/chat/{session_id}/history`；
- `GET /bff/v1/comp/chat/{session_id}/history`，含游标；
- `POST /comp/chat/{session_id}/read`；
- `GET /comp/chat/{session_id}/messages/{message_id}/image-request`。

群的历史条目里，assistant 行带 `speaker_session_id`（其余省略）。客户端通过 session 再到
instance 解析已离群发言者的名字。

### 拒绝群 session 的路由

把群 session 发给 1:1 路由会返回 `409`。1:1 的流式与异步发送、`/v2/comp/session/{id}/open`、
`/events` 和 `/effects` 的响应带 `code: wrong_channel`。reaction 路由和改图用它们的普通响应体
`{"error": "conflict", "message": …}`，`message` 里含 `wrong_channel`。
`GET /comp/chat/{user_id}/sessions` 不列出群，群由 `/v2/comp/groups` 返回。
dreaming 清扫器跳过群 session。

## 一轮

```
POST /v2/comp/group/{gid}/message/stream
  └─ 写入用户行 + 已认领的队列行（群 session）
       └─ 分离执行的轮次
            ├─ 入座：角色 + 好感度（只读、已衰减）→ 档位标签、intimacy 档
            ├─ 导演（流式）──► reasoning* ──► 规整 | 规则兜底
            │     └─ companion_decision_events（payload + reasoning）
            ├─ cast
            ├─ 每位发言者依次：
            │     构建回复请求（该成员自己的键 + [群聊] + 群历史）
            │     chat_companion 流 ──► meta(speaker) delta* done [image_request]
            │     写入 assistant 行（speaker_session_id）
            ├─ final
            ├─ 结算队列行
            └─ 启动 post-process：记忆（每位入座成员）、human insight（一次）、
                                   character/user insight（每位发言者）；不碰好感度
```

轮次是分离执行的：客户端断线不会取消它，之后从历史里读结果。重发同一个 `client_msg_id` 时的响应见
[发送](#发送)。

同一个群的轮次不串行。接力还在进行时发来的新消息会立刻开始第二场接力；两场接力在历史里可能交错，
同一位角色也可能在两场里都开口。

**入座。** 轮次为每位成员加载角色和它的 1:1 好感度。角色实例未激活的成员这一轮不入座。好感度只读：
在内存里做时间衰减和端点刷新，还没有好感度行的成员在内存里取默认值，不会创建或写入任何行。
成员快照在整轮里固定，所以轮次中途被踢出的成员可能把这一轮说完。

**发言者依次运行。** 每位说完、行落库之后，下一位才开始。每个请求都是 1:1 的回复请求，用该成员
自己的键构建：好感度与感受分句、关系层与 profile 层记忆召回、insights、世界与故事上下文、
物品与衣柜、当地时间与节日、按档位过滤的 `prompt_traits`、1:1 的回复计划，以及按该成员好感度
掷出的 `TurnNudges`。穿着来自导演给这位成员的条目。`chat_companion` 的档位路由、重试链、
兜底话术、输出过滤和多气泡串联都不变。发言者写入的每条 assistant 行都带 `speaker_session_id`。

**`[群聊]` 块**追加在每位发言者的 system prompt 后面：

```

[群聊]
你现在在群聊「{title}」里。          ← 没有标题时是：你现在在一个群聊里。
在场的还有：
- {name}：{personality}            ← 没有 personality 时是：- {name}
只说你自己的话，不替别人说话，不在开头写名字。
```

群里只有这位发言者时，`在场的还有：` 那一行和后面的列表换成一行 `群里只有你和对方。`

**发言者看到的历史。** 截至用户行的最近 `HISTORY_WINDOW`（20）条群消息，加上本轮前面的发言者已经
落库的 assistant 行。轮次进行中到达的更新的用户消息不包含在内。这位发言者自己的话是 `assistant`；
用户的话是 `user`，前缀 `用户：`；其他角色的话是 `user`，前缀 `{name}：`（该角色已不在时用
`某位群友`）。导演和图片合成器读取的群转录会跳过内容为空的非图片行；图片行读作
`（发了一张照片）`。

**图片。** 本轮是否可发图是一个值，算法与 1:1 相同。对于 `reply_text_image` 条目，合成器在聊天调用
之前、带着该发言者的上下文启动，`image_request` 帧跟在该发言者的 `done` 之后。导演的条目不带图片
引用和宽高比，所以沿用 1:1 的默认值。

**Prompt 日志。** 设置了 `PROMPT_LOG_DIR` 时，每位发言者各写一个 prompt 文件，以群 session 为键，
发言者的成员 session id 写在 `# speaker:` 头行里，也写在文件名末尾。

### 帧

```
reasoning*  （0..n 条，仅在开启推理时）
cast
( meta → delta* → done → image_request?  |  meta → done{truncated:true} → speaker_error  |  speaker_error )  × 发言者
final
```

失败的发言者有两种帧序列：聊天链用尽时，每次尝试发一对 `meta → done{truncated:true}`，最后发
`speaker_error`；没有留下任何行就失败时（回复请求构建失败，或图片行落库失败），只发一个
`speaker_error`。第一帧之后的任何位置都可能被一个终止性的 `error` 取代。

| 帧 | 载荷 |
|---|---|
| `reasoning` | `{content}`：一段推理增量 |
| `cast` | `{speakers: [{member_session_id, action_type}]}`；`action_type` 用 `meta` 的线上取值 `reply` / `reply_image` / `reply_text_image`。规则兜底之后同样会发，客户端始终能显示谁即将发言 |
| `meta` | 群聊轮次多一个 `speaker_session_id`；1:1 轮次省略，帧不变 |
| `delta` / `done` / `image_request` | 不变，以 `message_id` 为键 |
| `speaker_error` | `{speaker_session_id, code, message}`；非终止。它是新帧而不是 `error` 的变体，因为客户端把 `error` 当作轮次结束 |
| `final` | 与 1:1 形状相同；计数在各发言者之间求和，`filtered` / `prompt_injected` 只要有一位发言者为真就为真，`llm_attempts` / `gateway_errors` 汇总导演、每位发言者和图片合成器失败的尝试 |

引擎只负责发帧。推理是完整展示、折叠还是只显示"思考中"，由客户端决定。

### 失败与结算

| 失败 | 行为 |
|---|---|
| 导演失败、返回空或未配置 | 规则兜底；审计行记录状态 |
| 某位发言者的链用尽 | `meta → done{truncated:true} → speaker_error`（尝试行照 1:1 的方式落库）；接力继续，轮到下一位 |
| 某位发言者的请求构建失败，或图片行落库失败 | 只发一个 `speaker_error`；接力继续，轮到下一位 |
| 图片合成器失败 | 文字部分照常落库，不发 `image_request` |
| 没有任何发言者产出回复 | 流以 `error` 帧结束，不发 `final`，也不运行 post-process |
| `CHAT_QUEUE_GEN_TIMEOUT_SECS` 到期 | 导演和所有发言者共用一份预算。已经说完的发言者保留，其余跳过，终止帧是 `error generation_timeout`。不运行 post-process：已说完的发言者的话留在历史里，但这一轮用户说的话不会进入任何成员的记忆和 insights |
| 结算 | 只要有一条 assistant 行落库，这一轮就结算为 `done`；一条都没有时才写 `system_error` 通知，该通知不带发言者 |
| 客户端断线 | 轮次照常完成并落库 |
| 进程在轮次中途死亡 | 陈旧认领回收器释放认领，聊天队列 worker 把被释放的群聊轮次结算为无法服务：失败，附一条通知。群聊轮次不会在后台被重新驱动 |

某位发言者失败不会中断接力：其他成员是独立的，照样回答。群聊轮次只尝试一次、不重试，每位发言者也只尝试一次。

## 导演

一次 `[tasks.group_director]` 调用在群聊轮次里取代 1:1 的 PDE 判官：它返回一个有序的发言者列表，
每位发言者带各自的动作。

**配置。** 任务支持 `model`、`fallback`、`temperature`、`max_tokens`、`structured_output`（默认开）
和决定导演口味的 `filter_prompt`。随仓库发布的 `examples/model_config.toml` 带了一份已启用的示例。

- 任务块缺失：走规则兜底，群聊照常工作。
- 任务块存在但 `filter_prompt` 为空白：服务器拒绝启动。

每次尝试受聊天流的单次尝试常量约束，OpenRouter 的 keepalive 算作流字节，所以推理很久的模型不会被切断。

**推理**由部署方通过 body 规则打开，没有单独的 key：

```toml
[[providers.openrouter.body]]
tasks  = ["group_director"]
params = { reasoning = { effort = "medium" } }
```

推理 token 以 `reasoning` 帧流给客户端，完整文本保存在决策审计行里。内容增量会被缓冲，
流结束后再解析。调用回退到别的模型时，那个模型的推理继续追加。推理只用于展示：不会传给发言者，
也不决定任何事。`reasoning = { exclude = true }` 表示模型照常推理但不返回文本，因此不发帧。
推理 token 计入 `max_tokens`；用这条规则（`effort` 或 `reasoning.max_tokens`）去限制，
而不是调低 `max_tokens`。

**用户消息**由引擎构建，不可配置，因为引擎要解析返回内容。各段按下面的顺序出现；可选的段落在没有
内容时整段省略。

```
[群聊] {title}                                         ← 仅在设置了标题时
[成员]
- m1：{name}（{personality}）bond={bond_key} chemistry={chem_key} 亲密度=第 {rung} 档
- m2：{name} bond={bond_key} chemistry={chem_key} 亲密度=第 {rung} 档   ← 没有 personality 就没有 （…）
[图片能力] 本轮可发图={是|否}
[最近群聊]
{transcript}                                           ← 为空时写 "（无）"
[用户点名] {name}、{name}                               ← 仅在有点名时

[用户最新消息]
{content}
```

句柄 `m1`、`m2`、`m3` 按入座顺序。`bond_key` 和 `chem_key` 是 Bond 与 Chemistry 的档位 key；
好感度以档位标签和 intimacy 档的形式交给导演，从不给数值。转录行是 `用户：{content}` 和
`{name}：{content}`（图片行内容为空时是 `{name}：（发了一张照片）`），用 `\n` 连接。

**输出：**

```json
{"speakers": [{"member": "m2", "action": "reply_text_image",
               "inner_state": "…", "reply_mode": null, "clothing": null}],
 "reason": "…"}
```

`member` 是当前存在的句柄上的枚举。`action` 是 `reply_text`、`reply_image` 或 `reply_text_image`。
`inner_state`、`reply_mode`、`clothing` 可选，语义与 1:1 判官相同。

**规整**（纯函数，在 `eros-engine-core`）：未知或未入座的句柄丢弃；重复的句柄保留第一次出现；
没有图片能力的轮次上，图片动作降为 `reply_text`（动作只降不升）；规整后列表为空，按导演失败处理。

**规则兜底。** 每位入座成员按加入顺序各说一句 `reply_text`。导演失败、超时、返回无法解析的输出或
空列表时，以及 `[tasks.group_director]` 缺失时，都会走它。

**审计。** 每次导演运行写一行 `companion_decision_events`：解析后的裁决、一个 `reasoning` 键、导演
当时看到的名单，以及导演的 `generation_id`。见 [llm-audit.zh.md](llm-audit.zh.md#群聊导演)。

## 群从不做的事

- 它**不跑好感度判官**，**不写好感度**，**不碰 ghost streak**。任何成员的 `companion_affinity` 行
  都不会被创建或写入。
- 好感度从各成员的 1:1 会话读取：1:1 关系有变化，这位角色在群里从下一轮起就换了口吻。
- 只在群里维系的关系不会在群里升温，并且会按 1:1 的时间衰减变冷，因为没有任何写入刷新它的
  `updated_at`。
- 只在群里活跃的角色，不算 stories 意义上的活跃。
- 成员 session 的 `last_active_at` 不会变动，群的会变。

## 进入 1:1 的内容

post-process 在 `final` 之后启动一次，从不阻塞流。没有走到 `final` 就结束的轮次（没有回复，或生成超时）
不运行 post-process。

- **记忆。** 用户消息只 embed 一次。Profile 层写一行（`instance_id` 为 NULL）；关系层为**每位入座
  成员**各写一行，不论它有没有开口，内容是 `（群聊）用户：{message}`，`session_id` 是群。每次成员写入
  都会复查该成员 session 没有被归档。见 [memory-layers.zh.md](memory-layers.zh.md)。
- **Insights。** `human_insights` 每轮运行一次，输入是用户消息加上每条产出的回复，格式
  `{name}：{full_text}`。`character_insights` 和 `user_insights` 每位发言者各运行一次，输入是用户消息和
  该发言者的回复，以该发言者的 instance 为键。写入之前，每一次都会复查该发言者的成员 session 没有被归档。
  N 位发言者的一轮会发起 1 + 2N 次抽取调用。
- **不运行：**好感度评估、感受分句摘要和 ghost streak 更新。

## v1 不包含

- 异步发送、语音，以及用户上传图片（vision）。
- 用户动作、礼物和打赏。
- 角色的余效、需求和生活事件：发言者的 prompt 和导演的上下文都不带这些。
- Reaction 和改图。
- 群路由上的 BYOK。
- 群里的主动消息：导演只回应用户。
- 每个用户轮次多于一轮发言：每位成员最多说一次。
- 按好感度加权的必说 / 可说骰子。
- 转录里的进群 / 退群提示行：名单本身说明了谁在场。
- 对群 session 做 dreaming。
- 角色与角色之间的关系状态。
