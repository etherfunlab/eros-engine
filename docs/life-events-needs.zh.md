# 生活事件与需求

[English](life-events-needs.md) · [中文](life-events-needs.zh.md)

角色有自己的一天，有两类状态跟着这一天走。**需求**是六项基本水平（饥饿、社交欲、体力、情感需求、外出、享受），
由 PDE 判官评定，由 engine 随时间往前推，被接受的 give 也会改动它。**生活事件**是发生在她身上的事，调用方来要的时候才抽：
一趟出差、一场病、路上遇到的一只流浪猫、一个小时没看手机。两者都以平实的当前事实交给判官和回复，
都不告诉她该怎么表现；例外是本身就是沉默的事件（离线类种类）。两者之上还有第三件事：关系磨薄了，她可以提出先分开一阵，
这是第一个**生活选择**（见[提出分开的选择](#提出分开的选择)）。

请求与响应的形状见 [api-reference.zh.md](api-reference.zh.md)，判官的上下文和 verdict 见
[model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)。
本页讲各部分怎么运转、怎么配合。

## 速览

| | 需求 | 生活事件 |
|---|---|---|
| 是什么 | 六项水平：`hunger`、`social`、`stamina`、`emotional`、`outing`、`indulgence`，各为 `low` / `mid` / `high` | 发生在她一天里的事，分四个类别 |
| 谁写入 | PDE 判官，在被判官评过的文字回合；被接受的 give 上 engine 也写（见[give 会改动需求](#give-会改动需求)） | 调用方的 `POST /v2/comp/session/{session_id}/events`，种类由 engine 选 |
| 存在哪 | 被评定那一行用户消息的 `metadata.needs` | `engine.persona_events`，外加讲述它的那条消息（她不讲的离线类种类没有） |
| 范围 | 每个角色实例一份状态，跨它所有的 session | 同左 |
| 时间 | 两次评定之间漂移，读取时现算 | `started_at` 与 `ends_at`；冷却从开始时刻算起 |
| 需要 LLM 判官 | 需要 | 不需要 |
| 判官看到 | `[需求]` 行 | `[近期事件]` 行 |
| 回复看到 | `- 此刻的需要：…` | `- 正在经历：…` 与 `- 刚才：…` |
| 调用方拿到 | `body_state` 帧和 `GET …/effects` 上的 `needs` | events 路由的响应；`GET …/effects` 上的 `event` 与 `background_event` |

语音通话读需求、不读事件。群聊轮次两者都不读。细节见下文。

## 需求

### 六项需求

| 需求 | `low` | `high` | 回复里的措辞，`low` / `high` |
|---|---|---|---|
| `hunger` | 不饿 | 很饿 | 不饿 / 饿了 |
| `social` | 不想有人陪 | 想找人说话 | 不想跟人打交道 / 想找人说说话 |
| `stamina` | 很累 | 精力充沛 | 很累 / 精神很好 |
| `emotional` | 心里满足 | 很想被在乎 | 心里挺满足 / 很想被在乎 |
| `outing` | 只想待在家里 | 想出门 | 只想待在屋里 / 想出门走走 |
| `indulgence` | 没有这个念头 | 对成瘾物的渴求：抽根烟、喝一杯、来口大麻、喝杯咖啡 | — / 很想抽根烟、很想喝一杯、很想抽口大麻、很想喝杯咖啡或很想放纵一下（[见下](#需求出现在哪)） |

水平是有序的档位。判官从不读也不写数字，`mid` 就是不值一提的中间。

### 谁来判定

需求属于可选的 LLM 判官（配置了 `[tasks.pde_decision].filter_prompt`）。没有判官就没有需求：
没有 `[需求]` 行，回复里没有对应的行，帧和 effects 路由上的 `needs` 为 `null`，事件抽取里没有需求加权，
语音通话里也没有。

- **verdict。** 判官的 JSON 可以带 `needs`：六个键齐全的对象，或 `null`，意思是“不变，沿用上次的判定”。
  解析不了的值按 `null` 处理，永远不会让 verdict 失败。严格的 `response_format` 要求非空对象里六个键齐全。
  前四个键齐全、但缺 `outing` 或 `indulgence` 的对象（判官 prompt 没要这两项，或判官不走严格模式），缺的那一项沿用判官看到的水平（第一次评定之前为 `mid`）；
  前四个键缺任何一个，整个对象解析不了。
- **评谁。** 只评角色本人，不评你们之间的关系；关系由好感度评估器评。
- **存在哪。** 存在判官评过的那个回合的用户消息行上，键为 `metadata.needs`，不管这一回合的结局是回复、
  图片、ghost 还是产品咨询。如果这一回合有被接受的 give，由 engine 写入存下的水平：give 改动的那几项取自基线，
  其余各项取自判官。判官弃权时同样如此（见[下文](#give-会改动需求)）。写入会等它完成，回合才继续。
  写入失败只记日志，回合带着本来要写的水平继续往下走。
- **什么情况不写。** 规则引擎回合（判官关闭或失败）、`needs: null` 的 verdict、被离线类种类
  压成沉默的回合（它不跑判官），以及所有群聊和语音回合。例外是改动了某项需求的被接受的 give：
  只要此前已有一次评定，规则引擎回合和 `needs: null` 的 verdict 也照写。
- **谁的状态。** 角色实例的。读取时跨该实例的所有 session 找最新一次评定，所以再开一个会话不会让她的饥饿清零。

判官给出的 `needs` 和它看到的水平，会随 verdict 一起进审计；见
[llm-audit.zh.md](llm-audit.zh.md#pde-判官的-payload)。

### give 会改动需求

1:1 文字回合里，`give` 一旦被接受，engine 把三项需求各挪动一档：

| `give` 的物品 | `hunger` | `social` | `indulgence` |
|---|---|---|---|
| `food` | −1 | | |
| `cigarette`、`alcohol`、`herb` | −1 | +1 | +1 |
| `medicine`、`coffee` | −1 | −1 | −1 |
| 其他任何物品 | | | |

−1 是朝 `low` 方向一档，+1 是朝 `high` 方向一档，到头就停。数量不计：一次 give 只挪一档。

*被接受*指这一回合实际生效的回应是接受。判官拒绝，或习惯掷骰强制拒收
（见[习惯掷骰](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)），都不挪动任何需求。
失败放行的回合（判官失败）按现有规则接受 give，所以挪动在那里同样生效。

写入的内容：

- **基线。** 判官看到的那组水平：这一行用户消息之前最新的一次评定，推到这一行的 `sent_at`。
- **被挪动的需求。** 物品挪动的每一项，存下的水平是基线再挪一档。判官这一回合对该项的评定丢弃：
  它在 `[用户动作]` 里看到了这次 give，常常自己就把饥饿评低了，再在它的评定上挪一次，这顿饭就算了两遍。
- **其余需求。** 取判官的评定，和任何回合一样相对基线解析。判官弃权或失败时，保留基线。
- **没有基线。** 还没有任何评定时，判官的评定原样写入；判官也没有评定，就什么都不写。
  engine 从不凭空编造水平。
- **一次写入。** 结果写到这一行用户消息上，只写一次，用的就是上面那次写入。give 的回合里，
  判官弃权时也会写，因为 engine 自己有评定的理由。
- **读取方。** 回复、`body_state` 帧和 `GET …/effects` 和读任何评定一样读这组写下的水平。
  判官的审计里保留的仍是判官说的。

表里没有的物品什么都不挪，这一回合判官的评定照常写入。语音和群聊回合不论有没有 give 都不写需求。
生活事件只通过判官影响需求（见[下文](#2-事件从不改写已存的需求)）。

### 两次评定之间的漂移

两次评定之间什么都不存。当前状态就是最新一次评定加上此后经过的时间。自评定起每满一个周期，
每项需求朝它的静息水平挪一档，到了就停：

| 需求 | 静息水平 | 每隔多久一档 |
|---|---|---|
| `hunger` | `high` | 4 小时 |
| `social` | `high` | 12 小时 |
| `stamina` | `mid` | 8 小时 |
| `emotional` | `mid` | 12 小时 |
| `outing` | `mid` | 8 小时 |
| `indulgence`，没有习惯为 `true` | `mid` | 8 小时 |
| `indulgence`，一项习惯为 `true` | `high` | 12 小时 |
| `indulgence`，两项习惯为 `true` | `high` | 8 小时 |
| `indulgence`，三项或四项习惯为 `true` | `high` | 4 小时 |

离开得越久，她越饿、越想找人说话；疲惫、情感需求和想出门或想待着的念头则向中间回落。中午评成 `low` 的饥饿，16:00 读作 `mid`，
20:00 起读作 `high`。习惯指角色的 `smoking`、`alcoholic`、`drug`、`coffee` 四个键（见[角色设置](#角色设置)）；`false` 和不写算作一样。
为 `true` 的越多，判官把这份渴求评低之后，它回来得越快。存下时没有 `outing` 和 `indulgence` 的评定，这两项都读作 `mid`。
BYOC 角色的这四个键在她的卡片上，engine 只在一轮对话里才打开卡片：在对话之外（`GET …/effects`，以及回放时退回到推算水平的那一步），
她的 `indulgence` 按没有习惯的角色漂移。常量在 `crates/eros-engine-core/src/needs.rs`。

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
  [需求] 饥饿=高 社交欲=低 体力=中 情感需求=中 外出=中
  [需求] 饥饿=高 社交欲=低 体力=中 情感需求=中 外出=中 享受=高
  ```

  `外出` 每次都在；`享受=高` 只在 `indulgence` 为 `high` 时出现，没有它就读作“不高”（参考判官 prompt 里写明了这一点）。
  严格模式下的判官每个回合仍然要评 `indulgence`，却看不出它原来是 `low` 还是 `mid`，所以连续聊天时它每一轮都在重新猜这一项，
  按习惯数目的漂移只在两次评定之间起作用。第一次评定之前读作 `[需求] 尚未判定`。这一行永远在，因为缺这一行会被读成“全是低”。
- **回复**得到不是 `mid` 的那些水平，按上表的顺序，用 `；` 连起来，作为 `[character_state]` 里的一行：

  ```
  [character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
  - 此刻的需要：饿了；很累
  ```

  `indulgence` 只在 `high` 时出现。它点名一样她用的东西，每一轮从值为 `true` 的习惯键里随机挑一个：`smoking` 是很想抽根烟，
  `alcoholic` 是很想喝一杯，`drug` 是很想抽口大麻，`coffee` 是很想喝杯咖啡；没有键为 `true` 时读作很想放纵一下。各项都是 `mid`（`low` 的 `indulgence` 在这里也按 `mid` 算），
  或还没有评定，就什么都不加，prompt 保持不变。被判官评过的回合，回复按这一回合最终的水平来写：
  写在这一行上的水平（verdict 里的，被接受的 give 挪动过的那几项除外），否则用判官看到的那组推算后的水平。
  配置了判官的部署里，规则引擎回合用推算后的水平，再加上被接受的 give 的挪动。
- **调用方**在 `body_state` 帧上拿到同样的水平（`needs`，对象或 `null`）。有 give 余效在起作用，或需求已知时发送，
  幂等重放时重发。`GET …/effects` 返回推到调用时刻的水平，没有评定或没有判官时为 `null`；
  它的 `changes_at` 给出这份响应下一次自己变化的时刻：某项需求的下一步，或进行中事件的结束；`as_of` 是计算它的时刻，
  客户端据此把等待算成一段时长。见
  [消息流](api-reference.zh.md#post-compchatsession_idmessagestream)和
  [effects 路由](api-reference.zh.md#get-v2compsessionsession_ideffects)。
- **语音通话**把推到通话第一轮的水平冻结进 bootstrap 快照。它们渲染成 `[身体状态]` 块里的
  `此刻的需要：…`，每一轮的 `body_state` 帧带的都是冻结的水平。通话不评定也不推进它们；`high` 的 `indulgence`
  在整通电话里点名同一样东西：挑哪一样由快照的创建时刻推出。见
  [语音回合](api-reference.zh.md#post-compvoicesession_idturnstream)。
- **看不到需求的地方：**`/open` 开场白、事件消息、改图和群聊轮次。

engine 用到需求的地方只有一处：事件抽取的权重（见下）。没有任何 ghost 规则或图片规则读它们。累了的角色
不会因为规则更容易 ghost；判官看到这些水平，自己判断。

## 生活事件

### 类别

可抽取的事件共 52 种，每一种属于一个类别，类别决定它的形态：

| `category` | 形态 | 持续 | 类别权重 | 类别冷却 |
|---|---|---|---|---|
| `short` | 插曲 | 至多 1 小时 | 5 | 无 |
| `mid` | 插曲 | 至多 4 小时 | 4 | 无 |
| `body` | 背景 | 1 到 7 天 | 0.5 | 14 天 |
| `trip` | 背景 | 1 到 7 天 | 0.5 | 7 天 |

**插曲**是前台事件：她被叫去加班、前任发来消息、她在看书、在洗澡、没看手机。**背景**是持续数天的状态
（出差、生病、经期），期间插曲照样会发生。各种类、时长和含义见
[events 路由](api-reference.zh.md#post-v2compsessionsession_idevents)。每个种类的权重、冷却和时长区间在
`crates/eros-engine-core/src/events.rs` 的 `rule` 里，类别的数值在 `category_rule` 里。

时长写成区间的，在事件开始时按整小时均匀抽定。

另有两种永远不会被抽到：`taking_space`（说好暂时分开）和 `fell_out`（为分开的事闹得不愉快）。
她提出先分开一阵、用户作答或不作答时，由 engine 自己写入（见[提出分开的选择](#提出分开的选择)）。
它们按形态是 `mid` 插曲，其中 `taking_space` 超出了这个类别的 4 小时：它持续 12 到 24 小时。
它们没有抽取规则，调用方的 `kinds` 也拒绝它们。

### 抽取

抽取是输入加三个随机数（两段各一个，时长一个）的纯函数。

1. **资格。** 一个种类同时满足下面几条才有资格：
   - 调用方的 `kinds` 允许它（[见下](#调用方的-kinds)；不带表示全部 52 种）；
   - 过了它的门槛（见下）；
   - 它自己的冷却已过；
   - 它所属类别的冷却已过；
   - 离线类种类还要过了共享的离线冷却（见下）；
   - 背景规则允许它（见下）。
2. **类别。** 每个至少有一个合格种类的类别，权重是类别权重乘以角色系数。掷一次随机数，按累计权重选一个。
3. **种类。** 该类别里每个合格种类的权重是基础权重乘以它的各项[需求系数](#1-需求给种类加权)、
   `prefer_events` 系数、[关联系数](#关联系数)，少数种类再乘一个角色系数（[`is_virgin`](#角色设置)、
   [`homesick`](#角色设置)、[`sex_addict`](#角色设置)）。再掷一次，选一个。
4. **时长。** 固定时长照用；区间则掷出来。

没有合格种类就没有事件，响应是 `{"event": null, "message": null}`。只要有合格种类，抽取就一定得到事件。
她的一天有多热闹，取决于调用方什么时候来要，以及冷却。

因为分两段，一个类别的占比不取决于它里面有多少种类。四个类别都合格、角色没有任何设置时，一次抽取有一半落在
`short`，40% 落在 `mid`，`body` 和 `trip` 各 5%。没有合格种类的类别退出第一段，其余类别分掉它的权重。

### 同一时间一件插曲、一个背景

- 有插曲在进行时，完全不抽，连背景也不抽。
- 有背景在进行时，不会抽第二个背景，插曲照抽；只是住院（`hospitalized`）和术后（`post_surgery`）期间，
  需要她出门活动的那几种不抽：`temp_work`、`police_encounter`、`animal_encounter`、`hit_someone`、
  `got_hit`、`working_out`、`grocery_shopping`、`in_class`、`on_the_way`、`in_battle`、`got_bumped`、
  `bumped_someone`、`saw_bag`、`saw_clothes`、`out_shopping`、`bros_gathering`、`sis_gathering`、
  `admiring_scenery`、`admiring_art`、`watching_show`、`on_the_road`、`having_sex`。
  `period` 期间，
  不抽 `period_scare` 与 `pregnancy_scare`。
- 她在等用户回答自己提出的分开请求时，同样完全不抽，角色的任何一个 session 都一样：这个请求本身就是前台戏，
  也免得一段离线时段在问话中间开始。带着已经抽过的 `event_key` 来重复调用，照样返回那个事件。
- 一件插曲和一个背景可以同时进行。背景里抽到的插曲，是从背景里讲出来的。
- 这以角色实例为单位，跨它所有的 session：她只有一段人生。抽取会读该实例最近 90 天的事件，也就是最长的那个冷却。

写入时用同一条规则做原子检查：有插曲在进行时不写入另一件插曲；有任何事件在进行时不写入背景。

### 冷却与上限

- **按种类。** 一个种类要等它最新一次事件*开始*后过了冷却才能再开始，不是结束后。冷却最短 8 小时
  （`weather_change`），重的种类很长（`hit_someone` 14 天、`breakup` 30 天、`hospitalized` 60 天、
  `post_surgery` 90 天）。四个 `trip` 种类自己没有冷却，由类别冷却管。
- **按类别。**`body` 每两周最多开始一次，`trip` 每周最多开始一次，不论具体是哪个种类。`short` 与 `mid` 没有。
- **离线类共享。** 任何一个离线类种类开始后的 12 小时里，所有离线类种类都没有资格，不论刚才是哪一个；
  各自的冷却照样适用。它限制她沉默的频率，就像类别冷却限制出行和身体状态的频率。一次被接受的分开同样会启动它；
  `out_shopping` 只对被它带进离线的角色才算离线类种类。
- **按 key。** 同一个 session 里同一个 `event_key` 返回同一个事件，不抽取，也不生成。
- **不设总量上限。** engine 不限制调用方多久来要一次。频率由冷却加上调用方自己的策略决定。要生成消息的抽取会占一个
  单用户并发名额，所有 LLM 入口共用这个上限（到上限返回 `429`）。

### 门槛

门槛决定一个种类能不能被抽到。偏好和需求系数只改权重，不会绕过门槛。

| 种类 | 门槛 |
|---|---|
| `ex_message`、`breakup` | 亲密度档 2 及以上 |
| `sudden_desire` | 亲密度档 3 |
| `period`、`saw_bag`、`saw_clothes` | 角色的 `art_metadata.gender` 为 `"female"` |
| `period_scare` | 角色的 `art_metadata.gender` 为 `"female"`，并且亲密度档 2 及以上 |
| `pregnancy_scare` | 角色的 `art_metadata.gender` 为 `"female"`，并且亲密度档 3 |
| `away_from_phone`、`busy`、`in_battle`、`asleep`、`bros_gathering`、`sis_gathering`、`having_sex`，以及女性角色的 `out_shopping` | ghost 保护允许（见[下文](#离线类)） |

`out_shopping` 和两个聚会没有性别门槛：谁会因此离线、为什么两种性别都会抽到两个聚会，见
[逛街与聚会](#逛街与聚会)。

档位（1 到 3）读自该 session 的好感度，和聊天回合一样先做时间衰减；见 [affinity-model.zh.md](affinity-model.zh.md)。

### 角色设置

角色 genome 的 `art_metadata` 里有十个键，由 engine 读取，由写角色的人填写，和 `gender`、`birthday`、
`timezone` 放在一起：

| 键 | 取值 | 效果 |
|---|---|---|
| `prefer_events` | `mid` 种类的数组 | 列出的种类在 `mid` 内权重为 3 倍 |
| `health` | `"weak"`、`"normal"`（默认）、`"strong"` | `body` 类别权重分别为 2 倍、1 倍、一半 |
| `prefer_trip` | `true`、`false`（默认） | `trip` 类别权重为 3 倍 |
| `homesick` | `true`、`false`、不写（默认） | `trip` 内部：`true` 时 `hometown_visit` 3 倍、`short_trip` / `long_trip` 三分之一；`false` 时 `hometown_visit` 三分之一；`business_trip` 是工作，两种情况都不动 |
| `is_virgin` | `true`、`false`（默认） | 她还是处女期间，`having_sex` 在 `mid` 内权重为五分之一 |
| `sex_addict` | `true`、`false`、不写（默认） | `short` 内部：`true` 时 `sudden_desire` 3 倍，`false` 时三分之一；它的亲密度档门槛两种情况都照旧 |
| `smoking`、`alcoholic`、`drug`、`coffee` | 各为 `true`、`false`、不写（默认） | 她抽不抽烟、喝不喝酒、用不用大麻、喝不喝咖啡：决定 `indulgence` 回来得多快（见[漂移](#两次评定之间的漂移)）、`high` 的 `indulgence` 在回复里点名什么，以及她怎么对待递来的这样东西（见下） |

`prefer_trip` 管她动不动，`homesick` 管她动起来往哪去。两者都为 `true` 的空姐角色经常出行、照常出差，
放假回家而不是去别处。

抽取看到的「是否处女」是推导出来的：卡上写了 `is_virgin`，并且这个实例从来没有过 `having_sex` 事件。
第一条 `having_sex` 行写下之后就不再是了，不需要任何写入：下一次抽取读到那一行就行。这个键不进任何
prompt；她是什么样的人由写角色的人写进人设，和其他人设事实一样。`sex_addict` 同样不进任何 prompt。

读取是宽松的。类型不对的值按没写处理；`prefer_events` 里不是已知 `mid` 种类的条目被丢掉，其余保留。
`health` 缩放整个 `body` 类别，`period` 也在内，`period` 自己 28 天的冷却让它不管怎样都大约每月一次。
`prefer_trip` 提高的是出行的概率，不是频率：每周一次的类别冷却对它同样适用。

四个习惯键只通过 `indulgence` 的漂移影响抽取。判官的角色块（`[角色人格]`）不列它们，所以她不会每一轮都提烟、酒、大麻或咖啡；
希望这些习惯带进她说话的角色，要在人设里自己写明。engine 从不写这四个键。

**递来她不用的东西。** 一轮的动作是 `give`，递的是 `cigarette`、`alcohol`、`herb` 或 `coffee`，而对应的键（`smoking`、`alcoholic`、`drug`、`coffee`）
为 `false` 时，engine 在判官运行之前掷一次。三次里有一次，这一轮照常进行。其余情况她这一次不收：

- 判官的 `[用户动作]` 一行末尾加上 `，这次你不会收下`，判官的 `inner_state` 就按拒收来写；这一行只陈述结果，不给理由；
- 不管判官怎么答，生效的回应都是 `refuse`；没有 LLM 判官时，规则引擎的接受同样变成 `refuse`。

此后它就是一次普通的拒绝：`action_response` 帧和存下的 `response` 都是 `refuse`，这次 give 不留余效，bond 线降一档
（见[被拒绝的动作](affinity-model.zh.md#被拒绝的动作)），回复也按拒收来写。判官自己的回答留在它的审计 payload 里。
`true` 和不写从不掷，其他物品和其他动作也不掷。被重新驱动的回合会重新掷一次，就像判官也会重新判一次。

### 关联系数

她的一天是连着的：一件**在线**的事件会让生活里跟在它后面的种类更容易抽到。先行事件在进行中，或结束后
48 小时内，算在线。抽取第二段里，每有一个指向它的、不同种类的在线先行事件，该种类权重乘 3；两个就是 9 倍。
这个系数不解除门槛（在线的 `sick` 只对档 2 以上的女性角色提高 `period_scare`），不碰第一段，也不缩短冷却。

| 在线的先行事件 | 权重乘 3 的种类 |
|---|---|
| `ex_message`、`sudden_desire` | `having_sex` |
| `sick` | `period_scare` |
| `having_sex` | `pregnancy_scare` |
| `business_trip` | `on_the_road`、`on_the_way`、`busy`、`temp_work` |
| `short_trip`、`long_trip`、`hometown_visit` | `on_the_road`、`admiring_scenery`、`taking_selfie` |
| `hometown_visit` | `bros_gathering`、`sis_gathering` |
| `hospitalized`、`post_surgery` | `reading`、`gaming` |
| `period` | `irritable`、`unwell` |
| `breakup`、`quarrel`、`scolded_by_superior`、`in_battle` | `irritable` |
| `got_hit`、`got_bumped`、`weather_change` | `unwell` |
| `working_out` | `showering`、`bathing` |

有一种是压低而不是排除：实例从来没有过 `having_sex` 事件时，`pregnancy_scare` 权重为五分之一。没有硬性的
「只能在之后」门槛，也没有状态机：从不抽 `having_sex` 的部署里，这个虚惊只是降到五分之一，不会消失。
表和常量（`FOLLOW_FACTOR`、`FOLLOW_WINDOW_HOURS`、`UNLIKELY_FACTOR`）在 `crates/eros-engine-core/src/events.rs`。

### 调用方的 `kinds`

调用方的 `kinds` 是另一个手段：角色的世界观或部署方的策略用它排除某些种类，engine 里并不丢掉这些种类。

| `kinds` | 可以抽的种类 |
|---|---|
| 不带或 `null` | 全部 52 种 |
| `[]` | 一种都不抽：响应是 `{"event": null, "message": null}` |
| `["reading", "quarrel"]` | 只抽列出的 |
| `{"allow": […], "disallow": […]}` | `allow` 允许的，去掉 `disallow` 列出的 |

对象里，`allow` 不带或为 `null` 表示全部允许，`[]` 表示一种都不允许；`disallow` 不带、为 `null` 或 `[]` 都不去掉任何种类；
两边都列了的种类不抽。只写 `{"disallow": […]}` 就能排除种类而不冻结其余种类，engine 以后新增的种类也会照常抽取。
对象里出现未知的键，或任何地方出现未知种类，请求都会被拒绝，所以拼错的 `disallow` 不会悄悄放出本想排除的种类；
代价是调用方要等 engine 认识某个新种类之后才能列它。`taking_space` 和 `fell_out` 永远不会被抽到，
所以在 `kinds` 的任何位置写它们，都按同样的方式被拒绝。

有三种带成人题材：`sudden_desire`、`having_sex`，以及隐含性行为的 `pregnancy_scare`。engine 不内建这类分组；只对部分用户提供这类内容的调用方，
把它们列进 `disallow`。`scolded_by_superior` 与 `scolded_superior` 假定她有上级，在她的世界里读作老板、上司、领导、主人或君主；
没有上级的角色同样交给 `disallow` 排除。

### 发起一次抽取

什么时候要由调用方决定，发不发生、发生什么由 engine 决定。没有任何东西会自己发起抽取，事件结束时也不会发出任何东西。
每次抽取带一个 `event_key`（只在本会话内有效，所以重试是安全的）、可选的 `kinds` 过滤，以及聊天回合会带的
语言、tier 和 scope 字段，因为 engine 不会在两次调用之间保存它们。响应是事件加上讲述它的那条消息，或者两者都是 `null`。
群聊或语音 session 会被拒绝，返回 `409 wrong_channel`。

### 她怎么讲出来

每种事件都由一条主动消息讲出来，除了 `away_from_phone`、`asleep`、`having_sex`、两个聚会，以及亲密度档 2 以下的 `busy`、
`in_battle` 和女性角色的 `out_shopping`（见[下文](#离线类)）。这条消息走和回复同一条 companion 模型路径
（角色设定、历史窗口、记忆召回、`[now]`），不经过判官，不流式，也没有用户消息行。engine 在请求末尾补一条舞台提示，
作为最后一条 `user` 消息。提示不落库。它只陈述事实，不下指令：

```
（你刚临时接到一份工作，得马上去处理。）
```

其余细节（什么工作、前任写了什么、哪种动物）由她根据自己的人设和世界观补全。提示有两种变体：

- 背景的提示带上抽到的时长（`period` 的不带）：`（你刚出发去外地出差，要待 24 小时。）`。
- 背景期间抽到的插曲，提示以背景的状态开头：`（你正在外地出差。你刚在路上遇到了一只小动物。）`。

然后：

- 输出正则规则照常生效。回复为空，或被规则剥到一无所剩，什么都不写，响应的 `event` 和 `message` 都是 `null`。生成失败返回上游错误，什么都不写。
- 事件行和消息在同一个事务里写入。消息的 `sent_at` 就是事件的 `started_at`，是生成完成后取的同一个时刻；
  `ends_at` 是这个时刻加上抽到的时长。如果写入时输给了已经在进行的事件，消息随之作废。
- 这条消息是一行普通的 assistant 消息，`assistant_action_type = 'proactive'`、`metadata.proactive = 'event'`，
  在调用已读接口之前一直是未读。它不跑 post-process：不写记忆，不产生好感度事件，不动 ghost 连续次数。
  它会更新 session 的 `last_active_at`。
- **竞态时用户不优先。** 事件生成期间落进会话的用户消息不会让事件作废：不论用户有没有开口，它都已经发生。

### 持续期间

某一时刻正在进行的事件，是满足 `started_at <= 该时刻 < ends_at` 的那个背景和那件插曲。在聊天里它们到达三个读取方；
开启世界系统时，故事导演和世界导演也会读到（见[聊天之外](#聊天之外)）。

- **判官**得到一行 `[近期事件]`，排在 `[需求]` 之后、`[身体状态]` 之前。先列背景，再列插曲，用 `；` 连接，
  每项形如 `{label}（{span}前开始，约 {span}后结束）`：

  ```
  [近期事件] 长途旅游（2 天前开始，约 3 天后结束）；在看书（不到 1 小时前开始，约 2 小时后结束）
  ```

  不到一小时的跨度读作 `不到 1 小时`（结束那一半此时不带 `约`）；不满 48 小时按整小时四舍五入；
  从 48 小时起按整天四舍五入。`{label}` 是每个种类固定的中文名（`reading` 是 `在看书`，`long_trip` 是
  `长途旅游`）。没有进行中的事件、没有未解释的离线时段、没有等着标记归来的分开、也没有未交代的虚惊（见下）时，这一行不出现。
- **回复**在 `[now]` 块里得到 `- 正在经历：`，排在时间、节日、生日和纪念日几行之后，列出同样两件事，只写结束时间；
  下文的 `- 刚才：` 行紧跟其后。两行都不在 `[character_state]` 里：那个块的表头让位于它上面的块，
  放在那里，事件开始之前写下的故事摘要就会盖过事件：

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  ```

  有没有 LLM 判官都一样。`/open` 开场白和事件消息带同一行。语音、群聊和改图请求不读事件。
- **调用方**从 `GET …/effects` 拿到：`event` 是进行中的插曲，`background_event` 是进行中的背景。
  进行中的离线类事件和其他插曲一样作为 `event` 返回，带 `"offline": true`；客户端怎么展示由它自己决定。

文字回合按自己那行用户消息的发送时间来读事件，所以被重新驱动的回合看到的是同样的事件。

### 离线类

八个可抽取的种类会让角色联系不上，直到它们结束：`away_from_phone`、`busy`、`in_battle`、`asleep`、`bros_gathering`、
`sis_gathering`、`having_sex`，以及只对女性角色生效的 `out_shopping`。离线是事件持续期间她的行为方式，
不是类别：`busy` 按时长属于 `mid`，按行为属于离线。它们的门槛、沉默、例外和事后的解释都相同；
区别只在措辞，以及她讲不讲出来。`in_battle` 的措辞让她的世界观决定它是什么：现代角色把它读成一场艰难的谈判、
一顿商务宴请或一件难办的事，历史或奇幻角色把它读成一场真正的战斗。engine 自己写入的 `taking_space` 对所有角色都是离线，
走的是同一套机制（见[提出分开的选择](#提出分开的选择)）。各种类及其时长见
[events 路由](api-reference.zh.md#post-v2compsessionsession_idevents)。

**讲出来，还是沉默。** 哪些种类算离线、开始时讲不讲：

| 种类 | 谁会离线 | 讲不讲 |
|---|---|---|
| `away_from_phone`、`asleep` | 所有角色 | 从不讲 |
| `busy`、`in_battle` | 所有角色 | 亲密度档 2 起讲；以下沉默 |
| `out_shopping` | 女性角色；其他角色照常回复 | 女性角色档 2 起讲、以下沉默；其他角色每一档都讲，是一件普通的插曲 |
| `bros_gathering`、`sis_gathering` | 所有角色 | 从不讲；事后由她解释 |
| `having_sex` | 所有角色 | 从不讲；事后直说（`在做爱，对方那时发来的消息你现在才看到`），跟谁由她自己说 |
| `taking_space` | 所有角色 | 不由抽取产生；分开的流程里由她道别 |

亲密度档 2 及以上，`busy`、`in_battle` 和女性角色的 `out_shopping` 像其他种类一样用一条主动消息讲出来，
舞台提示只陈述事实：

```
（你接下来要忙一阵，这段时间顾不上看手机。）
（你接下来要去打一场硬仗，一段时间顾不上手机。）
（你接下来要出门逛街，这段时间顾不上看手机。）
```

档 2 以下，她一声不吭地沉默下去。讲述的那条消息为空或生成失败，
什么都不写，和任何要讲述的种类一样。

**抽到。** 她不讲的种类不调模型。事件行写入时 `message_id` 为 NULL，`started_at` 是调用的时刻，响应带着事件，
`"message": null`。`out_shopping` 算不算离线取决于角色，凡是由离线与否推出的一切都跟着这个答案走：
对不会被它带进离线的角色，它没有门槛、没有沉默，事后也没有解释。离线类种类只在角色可以沉默的地方才抽，
所以抽取要过两道借自 ghost 保护的门槛。被挡住的抽取只是把
离线类种类从允许的种类里拿掉，其他种类照常抽：

- 部署的 ghosting 开关关闭时不抽（`[tasks.pde_decision]` 上 `ghosting = false`）；
- 用户消息不足 10 条的 session 里不抽，也就是 `ghost_permitted` 守的那个下限。

门槛只作用于抽取，看的是调用方发起抽取的那个 session。一旦这一段存在，角色在所有 session 里的轮次都会沉默。
离线类种类还共享一个冷却（见[冷却与上限](#冷却与上限)）。

**持续期间。** 角色任何一个 session 里的每一轮 1:1 文字消息都会被 ghost，不经过判官，也不经过 ghost 打分。
已经讲出来的 `busy`、`in_battle` 或 `out_shopping` 同样让轮次沉默。

- 用户那一行被标记为已 ghost。它没有已读回执，因为没有判官读过它，角色解释之后也依然没有。
- 不调用 `record_ghost`：`ghost_streak`、`total_ghosts`、`last_ghost_at` 都不动，判官自己的 ghost 否决也不受影响。
  这种沉默是处境使然，不是她对用户的判断。
- 这一轮只有三帧：`meta(ghost)`、`done`、`final`。判官没跑，所以没有 `reasoning` 帧，也没有 `body_state` 帧，
  也不评定需求。
- `/open` 返回 `greeting: null`，不生成。检查发生在生成之前；开场白正在生成时才开始的这一段不会拦住它。

有两种轮次是例外，走正常路径：

- **动作轮次**（give、打赏、亲吻这类）。动作在语义上是线下互动，用不着手机，所以会得到回复。它的 `正在经历` 一行
  仍然列出这段离线。
- **强制出图的轮次**（`image.force = true`），永远是出图。

ghosting 开关关闭时，已经在进行的一段不会让任何轮次沉默；轮次走正常路径，`正在经历` 一行仍然列出它。`/open` 在它持续期间仍然不返回问候。其余机制见
[ghost-mechanics.zh.md](ghost-mechanics.zh.md#engine-强制的-ghost)。

**事后。** 解释是推算出来的，不存标记。在 session `S` 里，某一时刻若满足下面三条，离线事件 `E` 就是未解释的：

- `E` 是该实例最近 90 天内已经结束的事件里最新的一次，种类是任何会把这个角色带进离线的种类；
- `S` 里有一行用户消息，在 `E` 的时间窗内被 ghost；
- `S` 里在 `E` 结束之后没有写过带文字的 assistant 消息（只有图的回复不算）。

`E` 未解释期间，在 `S` 里生成的每一条角色消息都带下面这一行：一轮回复、一条 `/open` 开场白或一条事件消息。
文字是这个种类自己的：是什么让她联系不上，以及对方那时发来的消息她现在才看到。
判官的 `[近期事件]` 行末尾会多一项 `{label}（{span}前结束，期间对方的消息你没有回）`，比如
`睡着了（2 小时前结束，期间对方的消息你没有回）`。`E` 结束后写下的第一行带文字的 assistant 消息终结它，只有图的回复不算。窗内角色写过的回复
（动作轮次的那种）不算，`busy`、`in_battle` 或 `out_shopping` 开始时讲的那条消息也不算：它写在事件开始的时候，早于结束。
每个种类的文字见 [model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)。

```
- 刚才：睡着了，对方那时发来的消息你现在才看到
```

两个聚会从不事先讲出来，所以由这里来交代：`去了兄弟聚会，对方那时发来的消息你现在才看到` 与
`去了姐妹聚会，对方那时发来的消息你现在才看到`，判官那一行的后缀是
`兄弟聚会（{span}前结束，期间对方的消息你没有回）` 及其对应的姐妹版。女性角色的 `out_shopping` 读作
`在逛街，对方那时发来的消息你现在才看到`。分开自有两种说法（见[归来](#归来)）。

### 虚惊

`period_scare` 和 `pregnancy_scare` 是两个 `short` 种类，各持续一小时，最后都证明是一场空。它们只对女性角色抽取
（`period_scare` 从亲密度档 2 起，`pregnancy_scare` 在档 3），`period` 期间都不抽。她把它当作惊吓本身，用一条主动消息讲出来：

```
（你发现月经推迟了好几天还没来，心里有点慌。）
（你刚验了孕，验出两条杠，整个人都懵了。）
```

**持续期间。**`正在经历` 和 `[近期事件]` 只带标签（`月经没来`、`验出怀孕`）。任何 prompt 里都没有一处说它是假的：
她还不知道。她没有离线，照常回复。

**事后。** 交代是推算出来的，不存标记。在 session `S` 里，某一时刻若满足下面两条，虚惊 `F` 就是未交代的：

- `F` 是最近 90 天内在 `S` 里讲出、且已经结束的 `period_scare` 或 `pregnancy_scare` 里最新的一次；
- `S` 里在 `F` 结束之后没有写过带文字的 assistant 消息（只有图的回复不算）。

`F` 未交代期间，在 `S` 里生成的每一条角色消息都带下面这一行，判官的 `[近期事件]` 行末尾会多一项
`月经没来（{span}前结束，结果是虚惊一场）`（`验出怀孕` 同理）。`F` 结束后写下的第一行带文字的 assistant 消息终结它，只有图的回复不算。
她的其他 session 从不带它：她是在其中一个里讲的。

```
- 刚才：以为月经没来，后来来了，是虚惊一场
```

同一个 session 里既有未解释的离线时段、又有未交代的虚惊时，两段文字共用这一行，离线的在前，用 `；` 连接，
判官按同样的顺序得到两项：

```
- 刚才：睡着了，对方那时发来的消息你现在才看到；以为月经没来，后来来了，是虚惊一场
```

### 逛街与聚会

五个种类给她一段属于自己的时间，也给亲近的关系添一样可以想要的东西：

| 种类 | 标签 | 类别 | 权重 | 冷却 | 持续 | 门槛 |
|---|---|---|---|---|---|---|
| `saw_bag` | 看中了一个包 | `short` | 2 | 48 小时 | 1 小时 | 女性 |
| `saw_clothes` | 看中了一件衣服 | `short` | 2 | 48 小时 | 1 小时 | 女性 |
| `out_shopping` | 在逛街 | `mid` | 2 | 24 小时 | 2 到 4 小时 | 无 |
| `bros_gathering` | 兄弟聚会 | `mid` | 1 | 72 小时 | 2 到 4 小时 | 无 |
| `sis_gathering` | 姐妹聚会 | `mid` | 1 | 72 小时 | 2 到 4 小时 | 无 |

女性门槛读 `art_metadata.gender`，也就是 `period` 已经在用的那个键。没有任何需求系数作用在它们身上，三个 `mid` 种类
和其他 `mid` 种类一样可以写进 `prefer_events`。五个都在住院（`hospitalized`）和术后（`post_surgery`）期间排除：她那时不在外面走动。

**两个小发现。** `saw_bag` 和 `saw_clothes` 是普通的、会讲出来的插曲。亲密度档 1 时，这只是日常的一点质感，仅此而已。
从档 2 起，她可以直说自己想要；有 `tip_personality` 的角色用那份人格的口吻说，也就是打赏动作收尾时用的同一种口吻。
想要之后会不会有下文，是下游的事，engine 只负责把场景摆出来。舞台提示陈述事实，`{p}` 是角色的 `tip_personality`（去掉首尾空白）。
`saw_bag` 的提示：

| 亲密度档 | 提示 |
|---|---|
| 1 | `（你在逛街，随手看到一个挺好看的包。）` |
| 2 及以上，没有 `tip_personality` | `（你在逛街，看到一个特别喜欢的包，很想要。）` |
| 2 及以上，有 `tip_personality` | `（你在逛街，看到一个特别喜欢的包，很想要。代入你「{p}」的打赏人格，把这份心动告诉对方。）` |

`saw_clothes` 同理，把「一个」「包」换成「一件」「衣服」。

**`out_shopping`。** 只对女性角色离线；其他角色逛街时手机还在手里，照常回复。女性角色从档 2 起讲出来，
档 2 以下一声不吭地沉默（不调模型，`message_id` 为 NULL，`"message": null`）。不会被它带进离线的角色在每一档都讲它：

```
（你正在外面逛街。）
```

**两个聚会。** 两种性别都会抽到 `bros_gathering` 和 `sis_gathering`，权重相同，抽取里任何地方都没有性别系数。
这是有意为之：它读起来是平等，而撞上的那些场面本身就是内容，男性角色去了姐妹聚会，正是用户会追问的事。
两个聚会对所有角色都是离线，也从不事先讲出来：她和自己的朋友在一起，没有看手机，事后通过上文那条未解释的离线行来交代。
不想要这些的部署，像对其他种类一样把它们列进 `disallow`。

**为什么有离线事件。** 会消失的角色有自己的生活，不会围着用户转。角色正在聚会的用户，有理由去找另一个角色说话。

### 风景、演出与自拍

六种会讲出来的插曲，给她一个用户听得到的下午：`admiring_scenery`、`admiring_art`（`short`，1 小时）、`taking_selfie`
（`short`，30 分钟）、`on_the_road`（`mid`，2 到 4 小时：长途通勤、坐车、坐火车、坐飞机；`on_the_way` 仍是那 30 分钟的赶路）、
`watching_match` 与 `watching_show`（`mid`，2 到 3 小时）。期间她照常回复。提示只陈述事实，细节由她填：看的什么风景、
哪件作品、什么比赛、坐的什么。请求带 `image`、部署配了 composer 时，`taking_selfie` 的消息带上这张自拍：先写她的话，
composer 照着她的话画（见 [api-reference](api-reference.zh.md#post-v2compsessionsession_idevents)）；否则只有文字。`watching_match` 和 `taking_selfie`
住院也抽；其余四种要她出门。

### 想出门与想回家

两种会讲出来的插曲，把她的 `outing` 需求带进这一天：

| 种类 | 标签 | 类别 | 权重 | 冷却 | 持续 | 提示 |
|---|---|---|---|---|---|---|
| `want_to_go_out` | 想出门 | `mid` | 1 | 24 小时 | 1 到 3 小时 | `（你现在很想出门走走。）` |
| `want_to_go_home` | 想回家 | `mid` | 1 | 24 小时 | 1 到 3 小时 | `（你现在很想回家。）` |

她不离线，照常回复。没有门槛，在任何背景里都抽，住院也在内；和其他插曲一样，提示以背景的状态开头：
`（你正在住院。你现在很想出门走走。）`。两者都和其他 `mid` 种类一样可以写进 `prefer_events`。`high` 的 `outing` 让
`want_to_go_out` 权重乘 3，`low` 的让 `want_to_go_home` 乘 3；`high` 的 `indulgence` 让 `want_to_go_out` 乘 2
（见[下文](#1-需求给种类加权)）。

### 与 `/open` 的关系

事件消息算作会话里有人说过话，所以同一个当地日期里，生日和节日开场白都不会跟在它后面。事件消息落库时，
正在生成的开场白让路并返回 `null`；事件不会给开场白让路。开场白带 `正在经历` 和 `刚才` 两行，不带需求行；
离线类种类进行期间 `/open` 返回 `null`。

### 存储

- `engine.persona_events` 每个事件一行：`instance_id`（随实例级联删除）、`session_id`（在哪个 session 抽到并讲出来；
  session 没了就置为 NULL）、`event_key`（在 session 内唯一）、`kind`、`started_at`、`ends_at`，以及 `message_id`
  （讲述它的那条消息；她不讲的离线类种类为 NULL，消息没了也置为 NULL）。类别和是否背景由种类推出，不存。
  种类是当前构建不认识的行，读取时跳过。
- 消息是 `engine.chat_messages` 的一行，带 `metadata.proactive = 'event'`；事件的种类和时间在 `persona_events` 里。
- 需求存在被评定的用户消息行上，键为 `metadata.needs`，配一个部分索引（`chat_messages_needs_idx`），让读取最新评定
  对每个 session 都是一次索引探测。
- `taking_space` 和 `fell_out` 和其他事件一样是 `persona_events` 的一行，`message_id` 为 NULL；写下它们的那次选择在
  `engine.life_choices` 里（见[选择存在哪](#选择存在哪)）。

### 聊天之外

开启 [World Stories](world-system.zh.md#world-stories人生故事) 时，故事导演直接读该实例的事件，每件带标签、相对时间和她讲述时的那条
消息，另外还读在途背景，以及这对关系里的分开请求和结果，并把它们当作既成事实。她主动发的消息不进它的聊天证据，所以一件事
不会被读成她对一句不相干的话的回答。World Memories 导演对名单里的每个角色读在途背景，不论开没开 stories，住院中的角色
就不会被写进聚会。两位导演都不写事件，也不改上面的角色设置。需求不进其中任何一个。

## 两者如何互相影响

下面的需求系数都是第二段里的系数；其余几个（[关联系数](#关联系数)、`prefer_events`、`is_virgin`、`homesick`、`sex_addict`）
都不读需求。

### 1. 需求给种类加权

需求系数在抽取的第二段，对一个种类在它所属类别内的权重做缩放。它只决定类别里*哪一种*出现，不决定某个类别出不出现。
系数来自 `crates/eros-engine-core/src/events.rs` 里的 `needs_factor` 和 `indulgence_factor`：

| 需求水平 | 种类 | 系数 |
|---|---|---|
| `stamina` 为 `low` | `temp_work`、`working_out` | ×0.5 |
| `stamina` 为 `low` | `unwell`、`sick`、`bathing`、`asleep` | ×2 |
| `hunger` 为 `high` | `grocery_shopping` | ×2 |
| `emotional` 为 `high` | `ex_message` | ×3 |
| `emotional` 为 `high` | `animal_encounter`、`irritable`、`quarrel` | ×2 |
| `outing` 为 `high` | `want_to_go_out` | ×3 |
| `outing` 为 `low` | `want_to_go_home` | ×3 |
| `indulgence` 为 `high` | `want_to_go_out`、`irritable` | ×2 |

`indulgence` 那一行是单独的一个系数，和上面各行相乘：`outing` 与 `indulgence` 都为 `high` 时 `want_to_go_out` 为 ×6，
`indulgence` 与 `emotional` 都为 `high` 时 `irritable` 为 ×4。
`social` 不给任何种类加权。只有部署配置了判官、并且角色有一次评定时，系数才生效（评定推到调用时刻）；否则所有系数都是 1。
系数不是门槛：权重 ×3 的种类仍然要先有资格。

### 2. 事件从不改写已存的需求

没有任何代码路径会因为事件去写需求。判官读到 `[近期事件]`，以及 give 带来的 `[用户动作]` 和之后的 `[身体状态]`，
自己在把它们放在眼前的前提下重新评定需求；`examples/model_config.toml` 里的参考判官 prompt 就是这样要求它的：
以 `[需求]` 为起点，结合对话、`[近期事件]`、`[身体状态]` 和经过的时间重判。判官弃权（`needs: null`）时，不管发生了什么，上一次的评定继续往前漂。
唯一不是判官写的，是被接受的 give 对最多三项需求的挪动（见[give 会改动需求](#give-会改动需求)），它只替换判官对这几项的评定。

### 3. 离线的那一段不写评定

被离线类种类压成沉默的回合不跑判官，所以什么都不评。最后一次评定在这一段里一直往前漂，
这一段之后第一个被判官评过的回合看到的，是把整段时间都推过去的水平：一段很长的沉默之后，她读起来比之前更饿、更想找人说话。

合在一起，两者构成一个以判官为中心的回路：饿着的角色更可能正在外面买菜；这件事进入判官的上下文；判官再评一次饥饿。

## 提出分开的选择

chemistry 比 bond 高出两档、她的耐心又连续三个被判官评过的回合被磨低，这段关系就失衡了：心动跑在交情前面，而她累了。
她提出想先分开一阵。这是第一个**生活选择**：一个真实的请求，每一条路都有真实的代价，由上面那套机制搭起来：
她的请求是一条主动消息，分开是一段离线事件，用户的回答走一个端点。更完整的 `life_choices` 系统
（更多种类的选择、可配置的后果）不在这个流程之内。

### 她什么时候提出

engine 在每一轮 affinity 评估已经落库的 1:1 文字回合结束时检查（即*被判官评过的回合*：affinity 评估器跑过、verdict 落了库，
不管部署有没有配置 PDE 判官），时间在落库之后。下面几条同时满足时她提出，读的是已落库、做过时间衰减的状态：

- **差距。** chemistry 的档位减去 bond 的档位至少为 2，按两条五档的阶梯算。有符号：只有 chemistry 高于 bond 才算。
- **耐心。** 推算出的 `patience` 在这一轮和之前两个被判官评过的回合都低于 0.5。评估被跳过或失败的回合既不计数也不打断。
  各轮的值读自该 session 的好感度回合事件（`state_after.patience`），不另存；0.5 是 engine 自己的规则阈值，不是判官的档。
- **新的连续。** 三个回合都在该 session 最新一段经过之后：她的请求、它的结算，以及它开启的分开
  （取 `asked_at`、`resolved_at`，接受的选择再加上分开的 `ends_at`，三者最晚的那个）。她等待期间或分开期间发生的、
  被判官评过的动作回合属于这段经过，不计数；转换了不理会的请求的那一轮，是新连续的第一轮。提出一次就用掉这段连续：
  下一次要三个新的被判官评过的回合。
- **跑道清空。** 角色的所有 session 都没有待回答的选择，角色也没有正在进行的分开（`taking_space`）。
  她只有一段人生，所以同一时间最多只有一个请求待回答。
- **合适的 session。** 角色是 genome 角色的 1:1 文字 session。语音 session 不会触发、送达或结算选择，群聊根本不写好感度，
  BYOC session 从不触发，因为结算端点不服务 BYOC。

触发是 engine 对已落库状态做的数学，没有判官参与。除了这些机制之外没有别的冷却。用户一直重新造出这个条件，她就一直问：
下面的降档同时压低两条线，所以只有 bond 到了第 1 档（第 1 档的线不能再降，而 chemistry 还能降），或者用户把 bond 养上来，
差距才会合上。重复的提问，是关系一直失衡的自然后果。

### 她的请求与送达

触发之后，依次发生：

1. **她的话。** companion 对话模型走和事件消息相同的主动消息路径生成这个请求（角色设定、历史窗口、记忆召回、`[now]`；
   没有判官，不流式，没有用户消息行）。舞台提示是最后一条 `user` 消息，不落库：

   ```
   （这段关系最近让你很累：心动还在，但交情和耐心都被磨薄了。你决定郑重地告诉对方：你想先分开一阵子，希望对方给你一个答复。）
   ```

   这条消息是一行普通的 assistant 消息，`assistant_action_type = 'proactive'`、`metadata.proactive = 'life_choice'`，
   在调用已读接口之前一直是未读，不跑 post-process。
2. **选择这一行。** `engine.life_choices` 里的一行，和她的消息在同一个事务里写入：种类 `ask_for_space`，状态 `pending`，
   此刻的档位差距（2 到 4），`asked_at` 就是这条消息的 `sent_at`。待回答的窗口是 24 小时；过期时刻是
   `asked_at` 加 24 小时，推算出来，不存。

回复为空、被规则剥到一无所剩，或生成失败，什么都不写：没有消息，没有选择行，之后符合条件的回合还可以再问。这个检查排在这一轮
post-process 的最后，在好感度落库和 feeling 摘要之后，所以它的生成不会拖慢这两件事，这一轮自己的好感度写入（状态加事件行，一个事务）
也不受影响。

选择待回答且未过期期间，events 路由对这个角色什么都不抽。她提出时已经在进行的插曲照样跑到回答出来为止；接受或不理会会把它截断。

**送达。** 通知搭在逐轮的好感度 delta 查询上，即
[`GET /bff/v1/comp/affinity/{session_id}/event`](api-reference.zh.md#get-bffv1compaffinitysession_idevent)。
它的 `event` 对象多一个可选字段 `life_choice`（`id`、`kind`、`options`、`asked_at`、`expires_at`、`message_id`），
在响应时按该 session 待回答、未过期的选择现场拼出，没有时不带。请求落在触发那一轮的事件落库之后几秒，所以这一轮自己放行的那次查询
通常赶不上它：适配过的客户端在历史里某一行带 `occasion: "life_choice"` 时，应当重新拉一次 delta。两个历史路由
（[canonical](api-reference.zh.md#get-compchatsession_idhistorylimit20offset0) 与
[BFF](api-reference.zh.md#get-bffv1compchatsession_idhistorylimit50offset0)）都把主动 assistant 行的 `metadata.proactive`
作为可选的 `occasion` 给出，其他行省略。
降档产生的好感度事件不会从这条路由返回，客户端不会遇到不认识的 `event_type`；新的状态在结算响应里带回，
别的设备从绝对值路由读。

**降级本身就是设计。** 没有适配的下游会忽略这个不认识的字段，把请求当成一条普通的主动消息渲染，用户用文字回复。
这就是下面的"不理会"那条路。什么都不会坏；选择的界面完全是增量。

### 用户的回答

回答是 `POST /v2/comp/session/{session_id}/life-choices/{choice_id}`，带 `{"response": "accept" | "decline"}`
（[接口参考](api-reference.zh.md#post-v2compsessionsession_idlife-choiceschoice_id)），也可以是用户自己的话，或者沉默。

| 路径 | 怎么发生 | 选择的状态 | 降档 | 分开 | 她说的话 |
|---|---|---|---|---|---|
| 接受 | 端点，`accept` | `accepted` | 有 | `taking_space`，缺口每一档 6 小时 | 生成的道别 |
| 拒绝 | 端点，`decline` | `declined` | 有 | 无 | 生成的受伤回应 |
| 不理会 | 待回答期间用户的第一条普通消息 | `ignored` | 无 | 无；一小时的 `fell_out` 插曲 | 没有专门的一条：这一轮带着这场不愉快回复 |
| 过期 | 24 小时内既没回答也没发消息 | 仍为 `pending`，读作已过期 | 无 | 无 | 无 |

**接受。** 同一个事务里：选择变为 `accepted`；`taking_space` 事件（说好暂时分开）从此刻开始，持续时长是提出时存下的档位差距每一档
6 小时（差距 2、3、4 对应 12、18、24 小时）；执行降档。正在进行的插曲在这一刻结束，她放下手头的事；背景（出行、生病）
在底下照常进行。然后生成她的道别并返回，是一行普通的主动消息，`metadata.proactive = 'life_choice'`：

```
（对方同意了你提出的分开请求，接下来 {n} 小时你们不联系。你跟对方道别。）
```

`{n}` 是这段分开的小时数。道别生成失败或为空，结算、事件和降档照样保留：选择已经做出。

**拒绝。** 同一个事务里执行同样的降档，没有事件，也没有放缓。生成她受伤的回应并返回：

```
（对方不同意你提出的分开请求，你心里不好受。）
```

**不理会。** 选择待回答期间到达的第一条普通用户文字消息就是回答：状态变为 `ignored`，并且在同一个事务里、在这一轮运行之前，
写入一条 `fell_out` 事件（为分开的事闹得不愉快，形态像 `quarrel` 的插曲：一小时，不离线，没有自己的消息）。所以正是这一条不理会她的消息，
它的回复已经能在 `[近期事件]` 和 `正在经历` 里读到这场不愉快。正在进行的插曲在此被截断，和接受时一样：这场不愉快取代它，
一次一件插曲的规则不破。不改任何档位；判官看到这个上下文，自己给这一轮打分。时钟看的是用户那一行的 `sent_at`：
只有在她提出之后发出的消息才会触发（提出时已经在路上的那条不算回答），排队或被重新驱动的回合读到的结果相同。
**动作轮次**（give、打赏、亲吻）不算回答：它按自己的规则回复，请求仍然待回答，因为她等着的时候收到礼物，是在示好，不是在回答。
`/open` 开场白和 reaction 都不会让它结算。

**过期。** 24 小时内既没有回答、也没有消息，选择就此失效。它在表里的状态仍是 `pending`，但处处读作已过期：delta 上的字段不再附带，
端点返回 `409`，之后的消息也不会转成 `fell_out`。没有任何代价；之后再提出，需要重新满足完整的触发条件。

结算和一条进来的用户消息按事务竞争。先写入的赢，输的一方什么都不写（端点返回 `409`）。

### 代价

**降档（接受和拒绝）。** bond 和 chemistry 各降一档，按被拒动作的落点规则分别作用在每条线上：这条线落在下一档内的相同相对位置，
两个轴按同一比例缩放，第 1 档的线直接归 0。两条线的 `pending_deltas` 都清零。降档在结算的那一刻、与结算同一个事务里应用：
接受之后，降档就是她归来时的状态，归来时不再有别的事发生。好感度事件和它的 context 键见
[affinity-model.zh.md](affinity-model.zh.md#生活选择的降档与放缓)。

**放缓（只在接受时）。** 分开结束之后的 72 小时内，四个线轴上每一个为正的已提交增量在应用前减半，被削掉的部分直接丢弃，不会存进 `pending_deltas`。负增量照常全额应用，
推算出来的 `warmth` 和 `patience` 不缩放。它只作用于这次选择所在的 session：代价是这段关系的，和降档一样，
同一角色的其他 session 什么都不付。窗口是通过选择行推算出来的（`life_choices.event_id` 指向的 `taking_space` 行，
从它的 `ends_at` 到 `ends_at` 加 72 小时）：不新增状态，按墙钟而不是回合数算，所以用户晚些才回来，它照样在跑。
返回的 `effective_deltas` 如实显示减半后的值。

### 分开期间

`taking_space` 是一种不可抽取的离线插曲，整套照搬[离线类](#离线类)的机制：

- 角色任何一个 session 里的每一轮 1:1 文字消息都被 ghost，不经过判官：`meta(ghost)`、`done`、`final`；不评定需求，不写 ghost 连续次数。
- 动作轮次和强制出图的轮次是例外，和任何离线时段一样。
- `/open` 返回 `null`。
- 它进行期间不抽任何事件，并且启动共享的 12 小时离线冷却。
- 它和每一行 `persona_events` 一样按实例算：她只有一段人生，说好分开，就是她的手机在哪里都放下了。

ghosting 开关关闭时它不让任何轮次沉默，和任何离线时段一样，但 `/open` 仍然返回 `null`；降档、放缓和归来那一行都不依赖这个开关。分开期间的消息一律只是 ghost，不会转化任何挂起的请求。
分开进行期间，`正在经历` 和例外的动作轮次读到它的标签是说好暂时分开。

### 归来

不论哪种情况，归来都会被交代。标准的“未解释”规则需要窗口内有一行被 ghost 的用户消息，但守约的用户什么也不会发，
而她仍然得把归来标出来，否则这条弧线就断了。所以在这次选择所在的 session 里，`taking_space` 从它的 `ends_at` 起算未解释，
直到其后的第一行带文字的 assistant 消息，不论有没有被 ghost 的行；措辞随用户在窗口内有没有发过消息而定。她的道别写在分开开始的时候，
所以不会终结归来。该角色的其他 session 用标准的被 ghost 行规则，用第一种措辞。

| 用户 | `- 刚才：` | 判官一行的后缀 |
|---|---|---|
| 窗口内发过消息 | `你们之前说好分开一阵，现在时间到了；对方那时发来的消息你现在才看到` | `说好暂时分开（{span}前结束，期间对方的消息你没有回）` |
| 守约 | `你们之前说好分开一阵，现在时间到了，你们重新开始联系` | `说好暂时分开（{span}前结束，对方等到了约定之后）` |

### 选择存在哪

`engine.life_choices` 每次选择一行：`session_id`（非空，随 session 级联删除）、`kind`、`status`（`pending`、`accepted`、
`declined`、`ignored`）、`tier_gap`、`asked_at`、`resolved_at`、`message_id`（那条请求；消息没了就置为 NULL）和
`event_id`（接受时写下的 `taking_space` 行，其他路径为 NULL；事件没了就置为 NULL）。过期和放缓的窗口都是推算出来的，不存。
分开和不愉快那两行和其他事件一样在 `engine.persona_events` 里。

## 没有 LLM 判官时

| | 没有 `[tasks.pde_decision].filter_prompt` |
|---|---|
| 需求 | 没有：没有评定，没有各处的行，`needs: null`，抽取里没有需求系数 |
| 抽取 | 照常，只是没有需求系数 |
| 事件消息 | 照常生成并讲出来 |
| `正在经历` 与 `刚才` | 照常进入回复、`/open` 开场白和事件消息 |
| `[近期事件]` | 不渲染：没有判官来读它 |
| 离线类种类 | 和有判官时一样让轮次沉默，因为这个检查在判官之前：ghosting 开关以及动作、强制出图两种例外照样适用 |
| 提出分开 | 不变：触发读的是 affinity 评估器评过的回合，不是 PDE 判官 |
| `smoking`、`alcoholic`、`drug`、`coffee` | 为 `false` 的键在 give 时照样掷，可能把它变成拒收；没有 `indulgence` 可漂移、可点名 |

## 代码指引

- `crates/eros-engine-core/src/needs.rs`：水平、漂移常量、`Habits`、`carried`、`after_give`（give 的挪动）。
- `crates/eros-engine-core/src/pde.rs`：`refuses_give` 与 `UNUSED_SUBSTANCE_ACCEPT`，即 give 时掷的那一次。
- `crates/eros-engine-core/src/events.rs`：类别和种类、每个种类的规则、门槛、需求与角色系数、`is_offline` 与 `is_told`、`draw`、`lookback`。
- `crates/eros-engine-server/src/routes/session_events.rs`：events 路由、抽取的输入、离线类的两道门槛。
- `crates/eros-engine-server/src/routes/life_choices.rs`：结算端点。
- `crates/eros-engine-server/src/routes/bff/affinity.rs`：delta 上的 `life_choice` 字段。
- `crates/eros-engine-server/src/pipeline/proactive.rs`：事件消息和选择相关的消息、它们的舞台提示和写入（`kind_cue`、`find_cue`）。
- `crates/eros-engine-server/src/pipeline/life_choice.rs`：触发与她的请求、不理会的转换、放缓的读取。
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`：需求的读取与写入（被接受的 give 的挪动走 `after_give`）、`[需求]` 与 `[近期事件]` 两行
  （`build_pde_ctx`）、判官之前的 offline 分支、掷出拒收后的改判（`apply_give_refusal`）、`body_state_frame`。
- `crates/eros-engine-server/src/prompt.rs`：`needs_line`、`events_judge_text`、`events_reply_text`、
  `just_now_text`、`persona_traits`、`persona_habits`、`[character_state]` 里的各行，以及 `[now]` 里的事件行。
- `crates/eros-engine-server/src/routes/session_effects.rs`：`GET …/effects`。
- `crates/eros-engine-server/src/pipeline/voice.rs`：bootstrap 里的需求。
- `crates/eros-engine-store/src/events.rs::EventRepo`：`insert`、`insert_preempting_in_tx`、`by_key`、`recent`、`current`、`backgrounds_at`、`unexplained_offline`、`unreturned_separation`、`unresolved_scare`。
- `crates/eros-engine-store/src/story.rs::StoryRepo`：`life_events`、`life_choices`，以及不含 proactive 行的 `chat_evidence`。
- `crates/eros-engine-server/src/pipeline/story.rs`：`load_evidence` 与故事 payload 的 `life_events`、`current_background`、`life_choices`；`pipeline/world.rs::direct_world`：世界导演的 `current_background`；`prompt.rs::event_when_text`：两者共用的相对时间。
- `crates/eros-engine-store/src/life_choices.rs::LifeChoiceRepo`：选择行、`pending`、`resolve_in_tx`、`growth_slowed`。
- `crates/eros-engine-store/src/affinity.rs::AffinityRepo`：`persist_life_choice_drop_in_tx`、`last_judged_endpoints`。
- `crates/eros-engine-store/src/chat.rs::ChatRepo`：`set_needs` 与 `latest_needs`。
- `crates/eros-engine-store/migrations/0000_baseline.sql`：事件表与选择表、needs 索引，以及放宽后的 affinity `event_type` 检查（由 baseline 之前的迁移链折叠而来）。
