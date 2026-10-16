# 生活事件与需求

[English](life-events-needs.md) · [中文](life-events-needs.zh.md)

角色有自己的一天，有两类状态跟着这一天走。**需求**是四项基本水平（饥饿、社交欲、体力、情感需求），
由 PDE 判官评定，由 engine 随时间往前推。**生活事件**是发生在她身上的事，调用方来要的时候才抽：
一趟出差、一场病、路上遇到的一只流浪猫、一个小时没看手机。两者都以平实的当前事实交给判官和回复，
都不告诉她该怎么表现；唯一的例外是一种本身就是沉默的事件（`away_from_phone`）。

请求与响应的形状见 [api-reference.zh.md](api-reference.zh.md)，判官的上下文和 verdict 见
[model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)。
本页讲各部分怎么运转、怎么配合。

## 速览

| | 需求 | 生活事件 |
|---|---|---|
| 是什么 | 四项水平：`hunger`、`social`、`stamina`、`emotional`，各为 `low` / `mid` / `high` | 发生在她一天里的事，分四个类别 |
| 谁写入 | PDE 判官，在被判官评过的文字回合 | 调用方的 `POST /v2/comp/session/{session_id}/events`，种类由 engine 选 |
| 存在哪 | 被评定那一行用户消息的 `metadata.needs` | `engine.persona_events`，外加讲述它的那条消息 |
| 范围 | 每个角色实例一份状态，跨它所有的 session | 同左 |
| 时间 | 两次评定之间漂移，读取时现算 | `started_at` 与 `ends_at`；冷却从开始时刻算起 |
| 需要 LLM 判官 | 需要 | 不需要 |
| 判官看到 | `[需求]` 行 | `[近期事件]` 行 |
| 回复看到 | `- 此刻的需要：…` | `- 正在经历：…` 与 `- 刚才：…` |
| 调用方拿到 | `body_state` 帧和 `GET …/effects` 上的 `needs` | events 路由的响应；`GET …/effects` 上的 `event` 与 `background_event` |

语音通话读需求、不读事件。群聊轮次两者都不读。细节见下文。

## 需求

### 四项需求

| 需求 | `low` | `high` | 回复里的措辞，`low` / `high` |
|---|---|---|---|
| `hunger` | 不饿 | 很饿 | 不饿 / 饿了 |
| `social` | 不想有人陪 | 想找人说话 | 不想跟人打交道 / 想找人说说话 |
| `stamina` | 很累 | 精力充沛 | 很累 / 精神很好 |
| `emotional` | 心里满足 | 很想被在乎 | 心里挺满足 / 很想被在乎 |

水平是有序的档位。判官从不读也不写数字，`mid` 就是不值一提的中间。

### 谁来判定

需求属于可选的 LLM 判官（配置了 `[tasks.pde_decision].filter_prompt`）。没有判官就没有需求：
没有 `[需求]` 行，回复里没有对应的行，帧和 effects 路由上的 `needs` 为 `null`，事件抽取里没有需求加权，
语音通话里也没有。

- **verdict。** 判官的 JSON 可以带 `needs`：四个键齐全的对象，或 `null`，意思是“不变，沿用上次的判定”。
  解析不了的值按 `null` 处理，永远不会让 verdict 失败。严格的 `response_format` 要求非空对象里四个键齐全。
- **评谁。** 只评角色本人，不评你们之间的关系；关系由好感度评估器评。
- **存在哪。** 存在判官评过的那个回合的用户消息行上，键为 `metadata.needs`，不管这一回合的结局是回复、
  图片、ghost 还是产品咨询。写入会等它完成，回合才继续。写入失败只记日志，回合带着 verdict 里的水平继续往下走。
- **什么情况不写。** 规则引擎回合（判官关闭或失败）、`needs: null` 的 verdict、被 `away_from_phone`
  压成沉默的回合（它不跑判官），以及所有群聊和语音回合。
- **谁的状态。** 角色实例的。读取时跨该实例的所有 session 找最新一次评定，所以再开一个会话不会让她的饥饿清零。

判官给出的 `needs` 和它看到的水平，会随 verdict 一起进审计；见
[llm-audit.zh.md](llm-audit.zh.md#pde-判官的-payload)。

### 两次评定之间的漂移

两次评定之间什么都不存。当前状态就是最新一次评定加上此后经过的时间。自评定起每满一个周期，
每项需求朝它的静息水平挪一档，到了就停：

| 需求 | 静息水平 | 每隔多久一档 |
|---|---|---|
| `hunger` | `high` | 4 小时 |
| `social` | `high` | 12 小时 |
| `stamina` | `mid` | 8 小时 |
| `emotional` | `mid` | 12 小时 |

离开得越久，她越饿、越想找人说话；疲惫和情感需求则向中间回落。中午评成 `low` 的饥饿，16:00 读作 `mid`，
20:00 起读作 `high`。常量在 `crates/eros-engine-core/src/needs.rs`。

评定被推到哪个时刻，取决于谁在读：

| 读取方 | 推到 |
|---|---|
| 文字回合：判官那一行、回复、`body_state` 帧 | 这一回合自己那行用户消息的发送时间 |
| `GET …/effects`、一次事件抽取 | 调用发生的时刻 |
| 语音通话 | 通话的第一轮，之后冻结 |

文字回合读的是严格早于自己这一行的最新评定，所以被重新驱动的回合，判官看到的还是同一行。

### 需求出现在哪

- **判官**在每个被判官评过的回合得到一行 `[需求]`，排在 `[近期事件]` 和 `[身体状态]` 之前：

  ```
  [需求] 饥饿=高 社交欲=低 体力=中 情感需求=中
  ```

  第一次评定之前读作 `[需求] 尚未判定`。这一行永远在，因为缺这一行会被读成“全是低”。
- **回复**得到不是 `mid` 的那些水平，按上表的顺序，用 `；` 连起来，作为 `[character_state]` 里的一行：

  ```
  [character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
  - 此刻的需要：饿了；很累
  ```

  四项都是 `mid`，或还没有评定，就什么都不加，prompt 保持不变。被判官评过的回合，回复按这一回合最终的水平来写：
  判官评了就用 verdict 里的，否则用判官看到的那组推算后的水平。配置了判官的部署里，规则引擎回合用推算后的水平。
- **调用方**在 `body_state` 帧上拿到同样的水平（`needs`，对象或 `null`）。有 give 余效在起作用，或需求已知时发送，
  幂等重放时重发。`GET …/effects` 返回推到调用时刻的水平，没有评定或没有判官时为 `null`。见
  [消息流](api-reference.zh.md#post-compchatsession_idmessagestream)和
  [effects 路由](api-reference.zh.md#get-v2compsessionsession_ideffects)。
- **语音通话**把推到通话第一轮的水平冻结进 bootstrap 快照。它们渲染成 `[身体状态]` 块里的
  `此刻的需要：…`，每一轮的 `body_state` 帧带的都是冻结的水平。通话不评定也不推进它们。见
  [语音回合](api-reference.zh.md#post-compvoicesession_idturnstream)。
- **看不到需求的地方：**`/open` 开场白、事件消息、改图和群聊轮次。

engine 用到需求的地方只有一处：事件抽取的权重（见下）。没有任何 ghost 规则或图片规则读它们。累了的角色
不会因为规则更容易 ghost；判官看到这些水平，自己判断。

## 生活事件

### 类别

事件共 28 种，每一种属于一个类别，类别决定它的形态：

| `category` | 形态 | 持续 | 类别权重 | 类别冷却 |
|---|---|---|---|---|
| `short` | 插曲 | 至多 1 小时 | 5 | 无 |
| `mid` | 插曲 | 至多 4 小时 | 4 | 无 |
| `body` | 背景 | 1 到 7 天 | 0.5 | 14 天 |
| `trip` | 背景 | 1 到 7 天 | 0.5 | 7 天 |

**插曲**是前台事件：她被叫去加班、前任发来消息、她在看书、在洗澡、没看手机。**背景**是持续数天的状态
（出差、生病、经期），期间插曲照样会发生。各种类、时长和含义见
[events 路由](api-reference.zh.md#post-v2compsessionsession_idevents)。每个种类的权重、冷却和时长区间是
`crates/eros-engine-core/src/events.rs` 里的 `const`。

时长写成区间的，在事件开始时按整小时均匀抽定。

### 抽取

抽取是输入加三个随机数（两段各一个，时长一个）的纯函数。

1. **资格。**一个种类同时满足下面几条才有资格：
   - 在调用方的 `kinds` 里（不带表示全部 28 种，`[]` 表示一种都不要）；
   - 过了它的门槛（见下）；
   - 它自己的冷却已过；
   - 它所属类别的冷却已过；
   - 背景规则允许它（见下）。
2. **类别。**每个至少有一个合格种类的类别，权重是类别权重乘以角色系数。掷一次随机数，按累计权重选一个。
3. **种类。**该类别里每个合格种类的权重是基础权重乘以它的[需求系数](#1-需求给种类加权)再乘以
   `prefer_events` 系数。再掷一次，选一个。
4. **时长。**固定时长照用；区间则掷出来。

没有合格种类就没有事件，响应是 `{"event": null, "message": null}`。只要有合格种类，抽取就一定得到事件。
她的一天有多热闹，取决于调用方什么时候来要，以及冷却。

因为分两段，一个类别的占比不取决于它里面有多少种类。四个类别都合格、角色没有任何设置时，一次抽取有一半落在
`short`，40% 落在 `mid`，`body` 和 `trip` 各 5%。没有合格种类的类别退出第一段，其余类别分掉它的权重。

### 同一时间一件插曲、一个背景

- 有插曲在进行时，完全不抽，连背景也不抽。
- 有背景在进行时，不会抽第二个背景，插曲照抽；只是住院（`hospitalized`）和术后（`post_surgery`）期间，
  需要她出门活动的那几种不抽：`temp_work`、`police_encounter`、`animal_encounter`、`hit_someone`、
  `got_hit`、`working_out`、`grocery_shopping`、`in_class`、`on_the_way`。
- 一件插曲和一个背景可以同时进行。背景里抽到的插曲，是从背景里讲出来的。
- 这以角色实例为单位，跨它所有的 session：她只有一段人生。抽取会读该实例最近 90 天的事件，也就是最长的那个冷却。

写入时用同一条规则做原子检查：有插曲在进行时不写入另一件插曲；有任何事件在进行时不写入背景。

### 冷却与上限

- **按种类。**一个种类要等它最新一次事件*开始*后过了冷却才能再开始，不是结束后。冷却最短 8 小时
  （`weather_change`），重的种类很长（`hit_someone` 14 天、`breakup` 30 天、`hospitalized` 60 天、
  `post_surgery` 90 天）。四个 `trip` 种类自己没有冷却，由类别冷却管。
- **按类别。**`body` 每两周最多开始一次，`trip` 每周最多开始一次，不论具体是哪个种类。`short` 与 `mid` 没有。
- **按 key。**同一个 session 里同一个 `event_key` 返回同一个事件，不抽取，也不生成。
- **不设总量上限。**engine 不限制调用方多久来要一次。频率由冷却加上调用方自己的策略决定。要生成消息的抽取会占一个
  单用户并发名额，所有 LLM 入口共用这个上限（到上限返回 `429`）。

### 门槛

门槛决定一个种类能不能被抽到。偏好和需求系数只改权重，不会绕过门槛。

| 种类 | 门槛 |
|---|---|
| `ex_message`、`breakup` | 亲密度档 2 及以上 |
| `sudden_desire` | 亲密度档 3 |
| `period` | 角色的 `art_metadata.gender` 为 `"female"` |
| `away_from_phone` | ghost 保护允许（见[下文](#away_from_phone)） |

档位（1 到 3）读自该 session 的好感度，和聊天回合一样先做时间衰减；见 [affinity-model.zh.md](affinity-model.zh.md)。

### 角色设置

角色 genome 的 `art_metadata` 里有三个键，由 engine 读取，由写角色的人填写，和 `gender`、`birthday`、
`timezone` 放在一起：

| 键 | 取值 | 效果 |
|---|---|---|
| `prefer_events` | `mid` 种类的数组 | 列出的种类在 `mid` 内权重为 3 倍 |
| `health` | `"weak"`、`"normal"`（默认）、`"strong"` | `body` 类别权重分别为 2 倍、1 倍、一半 |
| `prefer_trip` | `true`、`false`（默认） | `trip` 类别权重为 3 倍 |

读取是宽松的。类型不对的值按没写处理；`prefer_events` 里不是已知 `mid` 种类的条目被丢掉，其余保留。
`health` 缩放整个 `body` 类别，`period` 也在内，`period` 自己 28 天的冷却让它不管怎样都大约每月一次。
`prefer_trip` 提高的是出行的概率，不是频率：每周一次的类别冷却对它同样适用。

调用方的 `kinds` 白名单是另一个手段：角色的世界观用它排除某些种类，engine 里并不丢掉这些种类。

### 发起一次抽取

什么时候要由调用方决定，发不发生、发生什么由 engine 决定。没有任何东西会自己发起抽取，事件结束时也不会发出任何东西。
每次抽取带一个 `event_key`（只在本会话内有效，所以重试是安全的）、可选的 `kinds` 白名单，以及聊天回合会带的
语言、tier 和 scope 字段，因为 engine 不会在两次调用之间保存它们。响应是事件加上讲述它的那条消息，或者两者都是 `null`。
群聊或语音 session 会被拒绝，返回 `409 wrong_channel`。

### 她怎么讲出来

除了 `away_from_phone`，每种事件都由一条主动消息讲出来。这条消息走和回复同一条 companion 模型路径
（角色设定、历史窗口、记忆召回、`[now]`），不经过判官，不流式，也没有用户消息行。engine 在请求末尾补一条舞台提示，
作为最后一条 `user` 消息。提示不落库。它只陈述事实，不下指令：

```
（你刚临时接到一份工作，得马上去处理。）
```

其余细节（什么工作、前任写了什么、哪种动物）由她根据自己的人设和世界观补全。提示有两种变体：

- 背景的提示带上抽到的时长（`period` 的不带）：`（你刚出发去外地出差，要待 24 小时。）`。
- 背景期间抽到的插曲，提示以背景的状态开头：`（你正在外地出差。你刚在路上遇到了一只小动物。）`。

然后：

- 输出正则规则照常生效。回复为空，或被规则剥到一无所剩，什么都不写，响应是 `null`。生成失败返回上游错误，什么都不写。
- 事件行和消息在同一个事务里写入。消息的 `sent_at` 就是事件的 `started_at`，是生成完成后取的同一个时刻；
  `ends_at` 是这个时刻加上抽到的时长。如果写入时输给了已经在进行的事件，消息随之作废。
- 这条消息是一行普通的 assistant 消息，`assistant_action_type = 'proactive'`、`metadata.proactive = 'event'`，
  在调用已读接口之前一直是未读。它不跑 post-process：不写记忆，不产生好感度事件，不动 ghost 连续次数。
  它会更新 session 的 `last_active_at`。
- **竞态时用户不优先。**事件生成期间落进会话的用户消息不会让事件作废：不论用户有没有开口，它都已经发生。

### 持续期间

某一时刻正在进行的事件，是满足 `started_at <= 该时刻 < ends_at` 的那个背景和那件插曲。它们到达三个读取方。

- **判官**得到一行 `[近期事件]`，排在 `[需求]` 之后、`[身体状态]` 之前。先列背景，再列插曲，用 `；` 连接，
  每项形如 `{label}（{span}前开始，约 {span}后结束）`：

  ```
  [近期事件] 长途旅游（2 天前开始，约 3 天后结束）；在看书（不到 1 小时前开始，约 2 小时后结束）
  ```

  不到一小时的跨度读作 `不到 1 小时`（结束那一半此时不带 `约`）；48 小时以内按整小时四舍五入；
  从 48 小时起按整天四舍五入。`{label}` 是每个种类固定的中文名（`reading` 是 `在看书`，`long_trip` 是
  `长途旅游`）。没有进行中的事件时，这一行不出现。
- **回复**在 `[character_state]` 里得到 `- 正在经历：`，排在 `此刻的需要` 之后，列出同样两件事，只写结束时间：

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  ```

  有没有 LLM 判官都一样。`/open` 开场白和事件消息带同一行。语音、群聊和改图请求不读事件。
- **调用方**从 `GET …/effects` 拿到：`event` 是进行中的插曲，`background_event` 是进行中的背景。
  进行中的 `away_from_phone` 和其他插曲一样作为 `event` 返回；客户端怎么展示由它自己决定。

文字回合按自己那行用户消息的发送时间来读事件，所以被重新驱动的回合看到的是同样的事件。

### `away_from_phone`

唯一一种本身就是沉默的事件。它开始时不生成任何东西。

**抽到。**不调模型。事件行写入时 `message_id` 为 NULL，`started_at` 是调用的时刻，持续 1 到 3 小时。响应带着事件，
`"message": null`。它只在角色可以沉默的地方才抽，所以抽取要过两道借自 ghost 保护的门槛。被挡住的抽取只是把
`away_from_phone` 从允许的种类里拿掉，其他种类照常抽：

- 部署的 ghosting 开关关闭时不抽（`[tasks.pde_decision]` 上 `ghosting = false`）；
- 用户消息不足 10 条的 session 里不抽，也就是 `ghost_permitted` 守的那个下限。

门槛只作用于抽取，看的是调用方发起抽取的那个 session。一旦这一段存在，角色在所有 session 里的轮次都会沉默。

**持续期间。**角色任何一个 session 里的每一轮 1:1 文字消息都会被 ghost，不经过判官，也不经过 ghost 打分：

- 用户那一行被标记为已 ghost。它没有已读回执，因为没有判官读过它，角色解释之后也依然没有。
- 不调用 `record_ghost`：`ghost_streak`、`total_ghosts`、`last_ghost_at` 都不动，判官自己的 ghost 否决也不受影响。
  这种沉默是处境使然，不是她对用户的判断。
- 这一轮只有三帧：`meta(ghost)`、`done`、`final`。判官没跑，所以没有 `reasoning` 帧，也没有 `body_state` 帧，
  也不评定需求。
- `/open` 返回 `greeting: null`，不生成。检查发生在生成之前；开场白正在生成时才开始的这一段不会拦住它。

有两种轮次是例外，走正常路径：

- **动作轮次**（give、打赏、亲吻这类）。动作在语义上是线下互动，用不着手机，所以会得到回复。它的 `正在经历` 一行
  仍然列出这段没看手机。
- **强制出图的轮次**（`image.force = true`），永远是出图。

ghosting 开关关闭时，已经在进行的一段不会让任何轮次沉默；轮次走正常路径，`正在经历` 一行仍然列出它。其余机制见
[ghost-mechanics.zh.md](ghost-mechanics.zh.md#engine-强制的-ghost)。

**事后。**解释是推算出来的，不存标记。在 session `S` 里，某一时刻若满足下面三条，没看手机事件 `E` 就是未解释的：

- `E` 是该实例最新的一次 `away_from_phone`，并且已经结束；
- `S` 里有一行用户消息，在 `E` 的时间窗内被 ghost；
- `S` 里在 `E` 结束之后没有写过 assistant 消息。

`E` 未解释期间，在 `S` 里生成的每一条角色消息都带下面这一行：一轮回复、一条 `/open` 开场白或一条事件消息。
判官的 `[近期事件]` 行末尾会多一项 `没看手机（2 小时前结束，期间对方的消息你没有回）`。`E` 结束后写下的第一行
assistant 消息终结它。窗内角色写过的回复（动作轮次的那种）不算。

```
- 刚才：有段时间没看手机，对方那时发来的消息你现在才看到
```

### 与 `/open` 的关系

事件消息算作会话里有人说过话，所以同一个当地日期里，生日和节日开场白都不会跟在它后面。事件消息落库时，
正在生成的开场白让路并返回 `null`；事件不会给开场白让路。开场白带 `正在经历` 和 `刚才` 两行，不带需求行；
`away_from_phone` 进行期间 `/open` 返回 `null`。

### 存储

- `engine.persona_events` 每个事件一行：`instance_id`（随实例级联删除）、`session_id`（在哪个 session 抽到并讲出来；
  session 没了就置为 NULL）、`event_key`（在 session 内唯一）、`kind`、`started_at`、`ends_at`，以及 `message_id`
  （讲述它的那条消息；`away_from_phone` 为 NULL，消息没了也置为 NULL）。类别和是否背景由种类推出，不存。
  种类是当前构建不认识的行，读取时跳过。
- 消息是 `engine.chat_messages` 的一行，带 `metadata.proactive = 'event'`；事件的种类和时间在 `persona_events` 里。
- 需求存在被评定的用户消息行上，键为 `metadata.needs`，配一个部分索引（`chat_messages_needs_idx`），让读取最新评定
  对每个 session 都是一次索引探测。

## 两者如何互相影响

### 1. 需求给种类加权

需求系数在抽取的第二段，对一个种类在它所属类别内的权重做缩放。它只决定类别里*哪一种*出现，不决定某个类别出不出现。
系数来自 `crates/eros-engine-core/src/events.rs` 里的 `needs_factor`：

| 需求水平 | 种类 | 系数 |
|---|---|---|
| `stamina` 为 `low` | `temp_work`、`working_out` | ×0.5 |
| `stamina` 为 `low` | `unwell`、`sick`、`bathing` | ×2 |
| `hunger` 为 `high` | `grocery_shopping` | ×2 |
| `emotional` 为 `high` | `ex_message` | ×3 |
| `emotional` 为 `high` | `animal_encounter`、`irritable` | ×2 |

`social` 不给任何种类加权。只有部署配置了判官、并且角色有一次评定时，系数才生效（评定推到调用时刻）；否则所有系数都是 1。
系数不是门槛：权重 ×3 的种类仍然要先有资格。

### 2. 事件从不改写已存的需求

没有任何代码路径会因为事件去写需求。判官在下一轮读到 `[近期事件]`，自己在把事件放在眼前的前提下重新评定需求；
`examples/model_config.toml` 里的参考判官 prompt 就是这样要求它的：以 `[需求]` 为起点，结合对话、`[近期事件]`、
`[身体状态]` 和经过的时间重判。判官弃权（`needs: null`）时，不管发生了什么，上一次的评定继续往前漂。
需求只有一个来源：判官。

### 3. 没看手机的那一段不写评定

被 `away_from_phone` 压成沉默的回合不跑判官，所以什么都不评。最后一次评定在这一段里一直往前漂，
这一段之后第一个被判官评过的回合看到的，是把整段时间都推过去的水平：一段很长的沉默之后，她读起来比之前更饿、更想找人说话。

合在一起，两者构成一个以判官为中心的回路：饿着的角色更可能正在外面买菜；这件事进入判官的上下文；判官再评一次饥饿。

## 没有 LLM 判官时

| | 没有 `[tasks.pde_decision].filter_prompt` |
|---|---|
| 需求 | 没有：没有评定，没有各处的行，`needs: null`，抽取里没有需求系数 |
| 抽取 | 照常，只是没有需求系数 |
| 事件消息 | 照常生成并讲出来 |
| `正在经历` 与 `刚才` | 照常进入回复、`/open` 开场白和事件消息 |
| `[近期事件]` | 不渲染：没有判官来读它 |
| `away_from_phone` | 照常让轮次沉默，因为这个检查在判官之前 |

## 代码指引

- `crates/eros-engine-core/src/needs.rs`：水平、漂移常量、`carried`。
- `crates/eros-engine-core/src/events.rs`：类别和种类、每个种类的规则、门槛、需求与角色系数、`draw`、`lookback`。
- `crates/eros-engine-server/src/routes/session_events.rs`：events 路由、抽取的输入、`away_from_phone` 的两道门槛。
- `crates/eros-engine-server/src/pipeline/proactive.rs`：事件消息、它的舞台提示和写入。
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`：需求的读取与写入、`[需求]` 与 `[近期事件]` 两行
  （`build_pde_ctx`）、判官之前的 away 分支、`body_state_frame`。
- `crates/eros-engine-server/src/prompt.rs`：`needs_line`、`events_judge_text`、`events_reply_text`、
  `persona_traits`，以及 `[character_state]` 里的各行。
- `crates/eros-engine-server/src/routes/session_effects.rs`：`GET …/effects`。
- `crates/eros-engine-server/src/pipeline/voice.rs`：bootstrap 里的需求。
- `crates/eros-engine-store/src/events.rs::EventRepo`：`insert`、`by_key`、`recent`、`current`、`unexplained_away`。
- `crates/eros-engine-store/src/chat.rs::ChatRepo`：`set_needs` 与 `latest_needs`。
- `crates/eros-engine-store/migrations/0070_chat_messages_needs_idx.sql` 与 `0072_persona_events.sql`：索引和表。
