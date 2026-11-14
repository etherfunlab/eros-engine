# 活动：约会

[English](activities.md) · [中文](activities.zh.md)

用户可以约角色出去。一次**约会**写明地点、安排和时长。她答应了，两人就离开主聊天，进入一段单独的对话，
一直聊到约会结束。结束时整场约会只评一次：结论挪动这段关系，约会本身连同其中的里程碑成为一段记忆。

这套机制叫**活动**（activity）。约会是唯一的活动，也只有用户能发起。它在一个**子会话**里进行：一条单独的
chat session（`channel = 'activity'`），隶属于发出邀约的那条 1:1 文字会话，即它的**父会话**。约会的消息
存在子会话里；这段关系本身（好感度、余效、记忆）留在父会话。

请求与响应的形状见 [api-reference.zh.md](api-reference.zh.md)，判官的模型配置块见
[model-config.zh.md](model-config.zh.md#tasksactivity_judge--约会判官)。本页讲一次约会从邀约到留下记忆是怎么走的。

## 速览

| | |
|---|---|
| 谁发起 | 用户，在 1:1 文字会话的一个回合上带 `date` 动作 |
| 谁决定 | 先过 engine 的门槛，再由 PDE 判官决定（规则引擎对过了门槛的一律接受） |
| 持续多久 | 从她答应的那条回复起 1 小时（`short`）或 4 小时（`mid`），或到用户提前结束 |
| 在哪进行 | 一个子会话；它的 id 出现在该回合的 `final` 帧、邀约那一行的历史记录和 `GET …/effects` 上 |
| 期间 | 角色的所有 1:1 文字会话都暂停；语音和群聊不暂停 |
| 怎么评 | 结束后由 `[tasks.activity_judge]` 评一次 |
| 挪动什么 | 每条好感度线最多一档，按约会的回合数加权 |
| 留下什么 | 一个片段、一条 `date` 纪事和其中的里程碑，全部是 `real` |
| 范围 | 每个角色实例同一时间只有一场约会，跨它所有的 session |

## 邀约

### 发出邀约

邀约是父会话里一个普通回合上的用户动作，走发送路由：

```json
{"type": "date", "duration": "short", "place": "海边的餐厅", "content": "吃一顿大餐"}
```

| 字段 | 规则 |
|---|---|
| `duration` | 必填：`short`（1 小时）或 `mid`（4 小时）。除非用户提前结束，约会就持续这么久 |
| `place` | 必填：trim 后 1–40 字 |
| `content` | 必填：两人要做什么，trim 后 1–120 字 |

`place` 和 `content` 不能含换行或控制字符。缺字段或超出范围返回 `422`，`code: "unprocessable"`；
`duration` 不是这两个值之一时请求体解析失败，返回不带 code 的 `422`，和其他未知的动作取值一样。
这一回合自己的文字可以为空，和任何动作回合一样。动作存在用户行的 `metadata.action` 上，回合生效之后带上回应。

### 谁能发

发送路由在写入任何东西之前检查下面这些；被拒时什么都不写。

| 情形 | 响应 |
|---|---|
| 会话是语音或群聊会话 | `409 wrong_channel` |
| 会话属于 BYOC 实例 | `400 byoc_unsupported` |
| 角色有一场约会正在进行，不论在她哪个会话 | `409 activity_in_progress` |
| 角色的任一会话里有一个待回答、未过期的生活选择 | `409 choice_pending` |

角色有一场已经结束、还没结算的约会时，先结算它（见[结算](#结算)），这一回合再继续。
用会话里已有的 `client_msg_id` 发来的是重试，不管约会处在什么状态都按已存的结果回答：邀约本身的重试，
在它开启的约会已经开始之后照样拿到她的回复。

### 门槛

回合被驱动时、判官运行之前，engine 按这段关系存储的、经过时间衰减的好感度检查门槛，和这一回合读到的一样：

| `duration` | 门槛 |
|---|---|
| `short` | 无 |
| `mid` | bond 在五档阶梯上达到第 2 档 |

两种时长都还有一条：她联系不上时门槛不通过。也就是有离线插曲正在进行（包括 `taking_space`），或者她还有另一场
没结束的约会。后一种是指：开启那场约会的回合还在生成时，这次邀约已经入队或发出；约会一旦开启，发送路由就会拒绝邀约。
这一对的 `affinity_scope` 和 chemistry 线都不读。

门槛不通过时，邀约像习惯掷骰拒收 give 那样被拒：判官的 `[用户动作]` 行末尾加上 `，这次你不会答应`，
判官照这个结果来写，不管它答什么，实际生效的回应都是 `refuse`。没有 LLM 判官时，规则引擎的接受变成 `refuse`。

### 她的回答

过了门槛，PDE 判官像对待任何动作一样来决定。它的上下文带着

```
[用户动作] 对方邀请你去约会：地点「{place}」，安排「{content}」，大约 {1|4} 小时
```

以及它在回合上读到的其他一切（`[近期事件]`、`[需求]`、她的人设），回答 `accept` 或 `refuse`；没有明确拒绝就算接受。
正在进行的背景（出差、生病）不会让门槛不通过：判官看得到，自己决定。`action_response` 帧带着回答，和任何动作回合一样。

**被拒。** 这一回合是一次普通的、没有代价的被拒动作：她的回复照拒绝来写，逐轮好感度判官照常给这一回合打分，
不降档。其他什么都不写：没有活动、没有子会话、没有事件。没有冷却，用户可以再约。

**被接受。** 她的回复落库之后、这一回合的 `final` 帧之前，一个事务把她的回答存到邀约那一行
（`metadata.action.response = "accept"`），并在角色实例的锁下写入：

1. 子会话：一行 `engine.chat_sessions`，`channel = 'activity'`，用户和实例与父会话相同；
2. `on_a_date` 事件：从她那条回复的 `sent_at` 起，持续 1 或 4 小时。它会结束正在进行的插曲，和被接受的生活选择一样；
   背景照常在下面继续；
3. `engine.activities` 的一行，把邀约、子会话和事件串在一起。

写入时在锁下再查一次：角色没有别的约会还在进行。没通过这次检查的回合什么都不开，她的回复照旧有效；
这期间这段关系被归档的也一样。再次驱动的回合会找到它已经开过的那场约会，不会再开第二场。

这一回合的 `final` 帧带 `activity`：`id`、`sub_session_id`、`duration`、`place`、`content`、`status`（`ongoing`）、
`started_at` 和 `ends_at`。其他回合的 `final` 都没有它。异步发送路由在回复产生之前就已经返回，所以它的客户端和所有客户端
事后一样去找这场约会：历史里邀约那一行的 `activity` 回执，以及 `GET …/effects`。

被接受的这一回合不由逐轮好感度判官打分，因为它属于这场约会，约会在结束时整场评一次。它记为一次跳过的评估
（`eval_skip_reason = "activity"`），不带规则微调。它的后处理其余部分照常运行；它是主聊天的回合，所以照常写进记忆、
计入 dreaming。

邀约从不出图，不管她怎么回答：这一回合是文字回复，`image.force` 在这一回合上被忽略。

## 约会

### 约会回合

约会回合发到 `sub_session_id`，走同样的发送路由（`POST /comp/chat/{session_id}/message/stream` 和
`POST /v2/comp/session/{session_id}/message/async`），约会处于 `ongoing` 时它们接受约会的子会话。
约会回合是一个文字回合，区别如下：

- **不逐轮评判。** 这一回合在父会话的好感度上记为一次跳过的评估（`eval_skip_reason = "activity"`），不带规则微调：
  跳过的回合本来还会加的长消息、长间隔微调也归零。逐轮 delta 路由显示它什么都没挪动。约会在[结算](#结算)时整场评一次。
- **从不 ghost。** 她和用户在一起：判官给出 ghost 时改为回复。
- **没有生活选择。** 约会回合既不触发生活选择，也不回答生活选择。跳过的回合既不计入、也不打断一个选择的连续计数。
- **动作照各自的规则。** 被拒的 `give`、`gift` 或身体动作照样让父会话上对应的线降一档，`strip` 照样要求性向相容的一对。
  约会中再带 `date` 动作返回 `409 activity_in_progress`。
- **不写逐轮原文。** 用户这句话不 embed 进任何一层记忆。
- **不进 dreaming。** 这一回合不计入关系扫描，扫描也从不读子会话的行（见[记忆](#记忆)）。
- **不当故事证据。** World Stories 的故事导演从不读子会话的行。

需求照常评定（有 LLM 判官时），洞察提取照常运行，和任何文字回合一样：用户在约会里说起自己的事，照样进画像。

**关系在父会话上。** 回合管线里按 session 存取的一切，除了消息本身，都在父会话上读写：好感度行和它的 ghost 连击、
give 的余效（约会的回合和父会话的回合一起计数）、浪漫选择读取的 scope，以及 `[emotional_context]` 背后的理由。
约会回合从主聊天留下的状态开始，约会之后的主聊天从约会留下的状态开始。需求本来就是按角色实例算的。

### 约会时她的 prompt

- **历史窗口。** 子会话自己的行，取最新的；剩下的位置用父会话的行补上，补到她答应的那条回复为止（含），
  所以约会一开场就知道刚才说了什么。
- **`[now]`。** `正在经历` 像读任何插曲一样读这场约会，后面跟一行地点和安排：

  ```
  - 正在经历：和对方约会（约 3 小时后结束）
  - 约会：地点「海边的餐厅」；安排「吃一顿大餐」
  ```

  `约会` 这一行只在约会进行中出现：约会结束之后才被驱动的回合（从队列里恢复的，或还在生成的）不会被告知约会正在进行。
- **判官**在 `[近期事件]` 里像读任何插曲一样读这个事件：`和对方约会（不到 1 小时前开始，约 3 小时后结束）`。
- 人设、记忆召回、`[shared_memories]` 等都是这段关系的，不变。

### 暂停的东西

约会处于 `ongoing` 时：

- 发到角色任一 1:1 文字会话的消息返回 `409 activity_in_progress`，什么都不写。会话里已有的那次发送的重试，照样按已存的结果回答。
- 这些会话里的 `/open` 返回 `greeting: null`。
- events 路由什么都不抽。这一条一直持续到约会结算，约会结束之后也一样。
- 不问任何生活选择，哪个会话都不问。

约会开启之前就发出的主聊天消息不受影响：它照样拿到回复，即使回复落在约会期间。语音和群聊会话不暂停。
子会话本身不接受 `/open`、事件抽取、`GET …/effects` 和生活选择的回答：在那里都返回 `409 wrong_channel`。

### 事件

`on_a_date`（标签 `和对方约会`）是一种不可抽取的插曲，和 `bathing_together` 一样。它没有抽取规则，调用方的 `kinds` 里写了它就返回 `422`。
它不是离线类，她不讲述它（她答应的那条回复已经讲了），故事导演也从不读它。它属于 `short` 类别，但时长是约会的，1 或 4 小时。
它和所有事件一样按角色实例计，进行中时以 `event` 出现在 `GET …/effects` 上。事件这一行是约会何时开始、何时结束的唯一记录。

## 怎么结束

- **到点。** 到了 `ends_at`，约会就结束了。不生成任何东西，也不发任何东西。之后再发到子会话，或在那里改图，返回 `409 activity_ended`。
- **提前。** `POST /v2/comp/session/{session_id}/activities/{activity_id}/end`（用父会话的 id）把事件的结束时间挪到现在，
  并在同一次调用里结算这场约会。到点之后再调用，它保留约会原有的结束时间并结算；客户端的计时器到点时调用它，拿到结果。

## 结算

### 什么时候结算

每场约会只结算一次，约会结束之后由下面三者里最先到的那个触发：

- 结束路由；
- 发到角色任一 1:1 文字会话的下一条消息，在那一回合做任何事之前；
- 这类会话里的 `/open`，在它决定她要不要先开口之前。

没有后台清扫。没人回来的约会一直不结算，也不在记忆里留下任何东西；结算之前，events 路由不给这个角色抽任何事件。

三者走的是同一个步骤，前面有一道对活动行的认领。别的调用方持有认领时，结束路由和发送返回 `409 activity_settling`，
`/open` 返回 `greeting: null`；都不等待。超过两分钟的认领视为被放弃，由下一个调用方接手。这个步骤在自己的任务里运行，
所以触发它的请求断开了它也会完成，出错时把认领交还给下一个调用方。已经结算过的约会，结束路由返回它存下的结果，重复调用不改变任何东西。
负责结算的那次发送或 `/open` 要先等判官调用完成，才开始做自己的事。

结算评的是当时已有的行。结算时还在生成的约会回合，或之后才从队列里恢复的，它的行留在子会话里，不参与评判。

### 判官调用

没有有效回合（一行带文字、且至少有一条带文字回复的用户消息）的约会直接标记为已结算，别的什么都不发生：
不调用、不挪动、不写记忆。

否则 engine 调用一次 `[tasks.activity_judge]`；没有这个配置块时，调用用 `[tasks.affinity_evaluation]` 的模型链和参数，
两种情况下 `max_tokens` 都至少是 1200（见 [model-config.zh.md](model-config.zh.md#tasksactivity_judge--约会判官)）。
prompt 由 engine 持有，用她的第一人称写，`reason` 的规矩和逐轮判官相同。它读：

- 逐轮判官读到的关于她和这段关系的内容，不多不少：她的名字，以及四个线轴的档位（`低` / `中` / `高`），从不给原始数值，
  也不给当前的 warmth 和 patience；
- 邀约和她答应的那条回复；
- 约会本身：子会话最新的 120 行，并注明前面省略了多少行。

回复是一个 JSON 对象，分三部分，各读各的：

- **结论**，与逐轮评判同一个形状、由同一个解析器读：`trust`、`intrigue`、`intimacy`、`tension` 各一个 0 到 4 的档位和方向，
  `warmth` 和 `patience` 各一个 1 到 3 的等级，外加一句 `reason`。档位描述的是整场约会。
- **`episode`**：这场约会发生了什么，最多 400 字，用扫描片段那种第三人称口吻。
- **`moments`**：零个或多个 `{kind, summary, due_on}`。`kind` 取 `confession`、`first_intimacy`、`conflict`、`reconciliation`、
  `promise`、`shared_plan`、`user_shared_milestone` 之一；`summary` 是一句话，最多 200 字；`due_on`（`YYYY-MM-DD`）只在 `promise`
  或 `shared_plan` 上读。时刻不写 realm，也不写日期：两者都由 engine 补上。列表里没有 `gift`，因为被接受的 give 在它自己的回合上已经记进纪事。

结论解析不了时什么都不挪动，和逐轮评估失败时保留存储状态一样，片段和时刻照样写入。其他种类的时刻，或摘要为空、过长的时刻，丢弃；
`due_on` 不是日期的，丢掉 `due_on`、保留时刻。片段缺失、为空或过长时不写片段。调用失败，或超过 45 秒，约会按不挪动、无片段、
无时刻结算；失败的尝试记在这场约会的好感度事件上，不重试。

### 挪动什么

结论按加权走一次逐轮写入管线：

```
weight = min(turns, cap(duration)) × AFFINITY_ACTIVITY_TURN_SHARE
```

| 环境变量 | 默认 | 含义 |
|---|---|---|
| `AFFINITY_ACTIVITY_TURN_SHARE` | `0.25` | 约会里一个有效回合的权重 |
| `AFFINITY_ACTIVITY_TURN_CAP_SHORT` | `12` | `short` 约会最多计入的回合数 |
| `AFFINITY_ACTIVITY_TURN_CAP_MID` | `32` | `mid` 约会最多计入的回合数 |

`turns` 是子会话里有效回合的数目。默认值下，`short` 约会的结论最多抵 3 个回合，`mid` 约会最多抵 8 个，只聊了一来一回的约会抵四分之一个回合。
上限设为 `0` 时，这种时长的约会什么都不挪动。非法值保持默认并打一条警告。

写入从结算时存储的状态开始，经过时间衰减，所以约会期间被拒动作降掉的档仍然降着。每一项原始分都乘 `weight`，跨线惩罚也一样；
负分乘 `AFFINITY_NEG_FACTOR`；正分乘这条线结算时所在档的档位衰减；阈值门、clamp、demo 加成和分开之后的放缓都和回合上一样，
没有规则微调。结论里的 `warmth` 和 `patience` 等级覆写存储的等级，两个端点重新派生。各步骤见
[affinity-model.zh.md](affinity-model.zh.md#写入管线仅线轴基础对从不进入)。

**一场约会让每条线最多挪一档。** 管线跑完之后，每条线相对结算时的位置最多挪一档：往上最多到上一档内的相同相对位置，
往下最多到下一档内的相同相对位置，也就是被拒动作的落点规则和它的镜像。被限住的线，两个轴沿各自的增量按同一个比例拉回，
让这条线正好落在上限上。第 1 档的线可以掉到 `0`，第 5 档的线由 clamp 限住。被上限削掉的部分就没了：被限住的线在 `pending_deltas`
里不留任何东西。

加权是因为逐轮判官大多数回合在大多数轴上给 0 档，而约会的结论是对所有轴的一次读数；设上限，是因为按全权重算，一个档位就可能把一条线带过好几档。

**事件行。** 父会话好感度上的一行 `companion_affinity_events`，`event_type = 'activity'`，带着常规的增量、线增量、档位变化和状态快照，
判官调用有失败的尝试时也记在这里。它的 `context` 带 `activity_id`、`turns`、`weight`、判官的 `reason`，以及 `capped`（被上限限住的线，
没有时不带），旁边是常规的档位、门的余额和端点审计。它没有 `user_message_id`：逐轮 delta 路由从不返回它，它不算生活选择连续计数里
被判官评过的回合，它的 `reason` 也从不进 `[emotional_context]`。结算时第一次达到的标签，在同一个事务里写一条 `tier_reached` 纪事，
和回合上一样。父会话还没有好感度行时不写事件。

结束路由返回这场约会（`status: "settled"`）、约会之后的这段关系，以及它的事件提交的增量；没有写事件时为 `null`。

## 记忆

约会是作为整体记住的，不是一句一句记。约会进行时，它的任何东西都不进关系记忆：它的回合不写逐轮原文、不计入 dreaming，
扫描也从不读子会话的行，当时不读，以后也不读。邀约那一回合是主聊天的回合，照主聊天的回合记。

约会至少有一个有效回合时，结算在写它的好感度事件的那个事务里，写下它留下的东西：

| 什么 | 行 | 来自 |
|---|---|---|
| 这场约会 | 一行 `relationship_episodes`；`started_at` / `ended_at` 取子会话的第一行和最后一行 | 判官的 `episode` |
| 约会发生过 | 一行 `relationship_journal`，`kind = 'date'`，摘要 `约会：在{place}，{content}`，指向邀约 | engine，不调 LLM；判官调用失败时也写 |
| 其中的里程碑 | 每个时刻一行 `relationship_journal` | 判官的 `moments` |

- 约会的每条纪事都是 `realm = 'real'`，日期是约会开始那天用户的当地日期（按关系扫描记住的时区；没有时用 UTC）。
  不问判官：用户发出邀约、她答应了，这就是 `real` 的意思。
- 让只该有一行的地方只有一行的键照样生效：约会里的 `first_intimacy` 和这段关系唯一一条真实的第一次亲密竞争，较早的那一行胜出。
  带 `due_on` 的时刻像任何 `promise` 或 `shared_plan` 一样在那天回来；`date` 那一行不会回来。
- 这些行写入时不带向量，之后再 embed，和 engine 自己写的其他纪事一样（见 [memory-layers.zh.md](memory-layers.zh.md#纪事)）。
- 这个片段和其他片段没有区别：它在 `[shared_memories]` 的最新三个片段里，之后的弧线改写会把它折入。结算不跑改写。
- 约会不写画像层记忆。用户在约会里说起自己的事，经洞察提取进入画像。

这些都不需要 `[tasks.dreaming_scan]`。

## 回到主聊天

主聊天的历史窗口只有它自己的行：邀约和她的回答，然后是约会之后的内容。她对这场约会的了解来自记忆。
发送或 `/open` 会在主聊天说任何话之前结算这场约会，所以那时它的片段和纪事已经在 `[shared_memories]` 里了。

在父会话里，从约会结束到她之后第一条带文字的消息为止：

- 回复的 `[now]` 带 `- 刚才：你们刚约会回来（{place}，{content}）`，同一行有离线或虚惊的文字时排在它们后面；
- 判官的 `[近期事件]` 以 `和对方约会（{span}前结束）` 结尾。

`/open` 的问候和事件消息带同样的 `刚才` 行。

## 存储

迁移 `0085`：

- `engine.chat_sessions.channel` 接受 `'activity'`，`engine.companion_affinity_events.event_type` 接受 `'activity'`。
  两条放宽后的 CHECK 都以 `NOT VALID` 加上：已有的行都满足被替换的较窄约束，所以迁移持锁期间不扫这两张表。
- `engine.activities` 每场被接受的约会一行：

  | 列 | 类型 | |
  |---|---|---|
  | `id` | uuid，主键 | |
  | `session_id` | 非空，外键 `chat_sessions`，`ON DELETE CASCADE` | 父会话 |
  | `sub_session_id` | 非空，唯一，外键 `chat_sessions`，`ON DELETE CASCADE` | 约会进行的地方 |
  | `message_id` | 唯一，外键 `chat_messages`，`ON DELETE SET NULL` | 邀约；再次驱动的回合开不出第二场约会 |
  | `event_id` | 外键 `persona_events`，`ON DELETE SET NULL` | `on_a_date` 事件，开始和结束时间都在那里 |
  | `duration` | text，非空 | `short` 或 `mid` |
  | `place`、`content` | text，非空 | |
  | `settle_claimed_at` | timestamptz | 结算的认领 |
  | `settled_at` | timestamptz | |
  | `created_at` | timestamptz，非空 | |

这一行不存用户和实例（父会话决定两者），也不存状态：`ongoing`、`ended`、`settled` 由事件的 `ends_at` 和 `settled_at` 推出。
被拒的邀约没有这一行；用户行的 `metadata.action` 就是它的全部记录。约会的消息是子会话里普通的 `engine.chat_messages` 行。

## API 表面

除了发送路由上新的几种 `409`，其余都是新增。

| 在哪 | 是什么 |
|---|---|
| [发送路由](api-reference.zh.md#post-compchatsession_idmessagestream) | `action.type = "date"`；约会进行时接受约会的子会话 id；`409 activity_in_progress`、`activity_ended`、`activity_settling`、`choice_pending` |
| 流的 `final` 帧 | `activity`，在她答应了邀约的那一回合上 |
| [`POST /v2/comp/session/{session_id}/activities/{activity_id}/end`](api-reference.zh.md#post-v2compsessionsession_idactivitiesactivity_idend) | 提前结束一场约会，或拿到一场已到点的约会的结果，并结算它 |
| [`GET /v2/comp/session/{session_id}/effects`](api-reference.zh.md#get-v2compsessionsession_ideffects) | 角色有一场 `ongoing`，或 `ended` 而未结算的约会时带 `activity` |
| 两套历史接口和启动包 | 邀约那一行用户消息上的 `activity` 回执；子会话的行用它自己的 id 读 |
| [`GET /bff/v1/comp/affinity/{session_id}/event`](api-reference.zh.md#get-bffv1compaffinitysession_idevent) | 接受子会话 id，返回这段关系最新的回合 |
| [改图](api-reference.zh.md#post-v2compsessionsession_idmessagemessage_idimageedit) | 约会进行时可以改约会里的图；之后返回 `409 activity_ended` |
| `GET /comp/chat/{user_id}/sessions` | 不列子会话 |
| `/open`、`/events`、`/effects`、生活选择路由 | 在子会话上返回 `409 wrong_channel` |

`activity` 在哪里出现都是同一个形状：`id`、`sub_session_id`、`duration`、`place`、`content`、`status`（`ongoing`、`ended` 或
`settled`，按响应时刻算）、`started_at` 和 `ends_at`（事件行没了之后两者都是 `null`）。

## 归档与抹除

归档这段关系（`DELETE /comp/instance/{instance_id}/sessions`）会把它的子会话和其他会话一起归档，并结束正在进行的约会。
关系被归档的约会永远不会结算；归档落下时正在结算的约会，只标记为已结算，别的什么都不写。约会的片段和纪事是关系层的行：
归档删除它们，抹除也覆盖它们。子会话的消息和 `engine.activities` 那一行是聊天数据，两者都不动。

## 没有 LLM 判官时

| | 没有 `[tasks.pde_decision].filter_prompt` |
|---|---|
| 门槛 | 不变 |
| 接受还是拒绝 | 规则引擎对过了门槛的一律接受 |
| 约会回合 | 不变，只是没有需求评定 |
| 结算 | 不变：约会判官和好感度评估器是一类，不是 PDE 判官 |

## 源码

- `crates/eros-engine-core/src/types.rs`：`UserAction::Date`、`DateDuration`。
- `crates/eros-engine-core/src/pde.rs`：`date_ruled_out`，即门槛。
- `crates/eros-engine-core/src/events.rs`：`EventKind::OnADate`。
- `crates/eros-engine-core/src/affinity.rs`：`AffinityTuning::activity_weight`、`grade_weighted`、`Affinity::cap_line_shift`、
  `tier_raise_target`。
- `crates/eros-engine-server/src/routes/companion_stream.rs`：`date` 动作和它的校验；`resolve_send_session`，让子会话进入发送路由。
- `crates/eros-engine-server/src/pipeline/activity.rs`：`admit_send`（谁能发）、`turn_session`（约会回合用父会话的状态）、`settle`、
  判官调用和它的解析器。
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`：驱动时的门槛和强制拒绝、在 `final` 之前开启约会、约会回合上
  不 ghost 也不转换生活选择、`[now]` 里的几行。
- `crates/eros-engine-server/src/pipeline/post_process.rs`：`activity` 跳过，约会回合不写逐轮原文、不计入 dreaming。
- `crates/eros-engine-server/src/pipeline/handlers.rs`：约会的历史窗口。
- `crates/eros-engine-server/src/routes/activities.rs`：结束路由。
- `crates/eros-engine-server/src/routes/session_open.rs`、`session_events.rs`、`session_effects.rs`：暂停的东西，以及 effects 上的 `activity`。
- `crates/eros-engine-server/src/prompt.rs`：`activity_judge_system_prompt`、`activity_judge_user_payload`、`action_subject`、
  `date_line`、`just_now_text`、`events_judge_text`。
- `crates/eros-engine-store/src/activity.rs::ActivityRepo`：`accept`、`date_on`、`unsettled_for_instance`、`returned_from`、
  `end_early`、`claim_settlement`、`transcript`、`substantive_turns`。
- `crates/eros-engine-store/src/affinity.rs::AffinityRepo::persist_activity_in_tx`：加权、限档的写入和它的事件行。
- `crates/eros-engine-store/src/journal.rs`：`date`、`date_moment`；`episode.rs::EpisodeRepo::insert_in_tx`。
- `crates/eros-engine-store/src/session_archive.rs`：归档结束约会并归档它的子会话。
- `crates/eros-engine-store/src/story.rs`：`life_events` 与 `chat_evidence` 把约会排除在外。
- `crates/eros-engine-llm/src/model_config.rs`：`resolve_activity_judge`、`ACTIVITY_JUDGE_MIN_TOKENS`。
- `crates/eros-engine-server/src/state.rs`：三个 `AFFINITY_ACTIVITY_*` 旋钮。
- `crates/eros-engine-store/migrations/0085_activities.sql`。
- 设计 spec：[2026-11-12-activity-sub-session-design.md](superpowers/specs/2026-11-12-activity-sub-session-design.md)。
