# 生活事件与需求

[English](life-events-needs.md) · [中文](life-events-needs.zh.md)

角色有自己的一天，有两类状态跟着这一天走。**需求**是四项基本水平（社交欲、情感需求、外出、享受），
由 PDE 判官评定，由 engine 随时间往前推，被接受的 give 也会改动它。**生活事件**是发生在她身上的事，调用方来要的时候才抽：
一趟出差、一场病、路上遇到的一只流浪猫、一个小时没看手机。两者都以平实的当前事实交给判官和回复，
都不告诉她该怎么表现；例外是本身就是沉默的事件（离线类种类）。两者之上还有第三件事：**生活选择**。她可以向用户问两人到底是什么关系（告白、提出结成伙伴、求婚、分手），
也可以提出一起过夜、一起洗澡或一起去旅行（见[生活选择](#生活选择)）；用户把她的耐心磨光了，这一对就会陷入冷战（见[冷战](#冷战)）。

请求与响应的形状见 [api-reference.zh.md](api-reference.zh.md)，判官的上下文和 verdict 见
[model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)。
本页讲各部分怎么运转、怎么配合。

## 速览

| | 需求 | 生活事件 |
|---|---|---|
| 是什么 | 四项水平：`social`、`emotional`、`outing`、`indulgence`，各为 `low` / `mid` / `high` | 发生在她一天里的事，分四个类别 |
| 谁写入 | PDE 判官，在被判官评过的文字回合；被接受的 give 上 engine 也写（见[give 会改动需求](#give-会改动需求)） | 调用方的 `POST /v2/comp/session/{session_id}/events`，种类由 engine 选 |
| 存在哪 | 被评定那一行用户消息的 `metadata.needs` | `engine.persona_events`，外加讲述它的那条消息（她不讲的离线类种类没有） |
| 范围 | 每个角色实例一份状态，跨它所有的 session | 同左 |
| 时间 | 两次评定之间漂移，读取时现算 | `started_at` 与 `ends_at`；冷却从开始时刻算起 |
| 需要 LLM 判官 | 需要 | 不需要 |
| 判官看到 | `[需求]` 行 | `[近期事件]` 行 |
| 回复看到 | `- 此刻的需要：…` | `- 正在经历：…` 与 `- 刚才：…` |
| 画图合成器看到 | — | `[此刻状态]` 段 |
| 调用方拿到 | `body_state` 帧和 `GET …/effects` 上的 `needs` | events 路由的响应；`GET …/effects` 上的 `event` 与 `background_event` |

语音通话读需求、不读事件。群聊轮次的回复两者都不读；只有群聊图片的合成器会读发言人的事件。细节见下文。

## 需求

### 四项需求

| 需求 | `low` | `high` | 回复里的措辞，`low` / `high` |
|---|---|---|---|
| `social` | 不想有人陪 | 想找人说话 | 不想跟人打交道 / 想找人说说话 |
| `emotional` | 心里满足 | 很想被在乎 | 心里挺满足 / 很想被在乎 |
| `outing` | 只想待在家里 | 想出门 | 只想待在屋里 / 想出门走走 |
| `indulgence` | 没有这个念头 | 对成瘾物的渴求：抽根烟、喝一杯、来口大麻、喝杯咖啡、吃顿好的 | — / 很想抽根烟、很想喝一杯、很想抽口大麻、很想喝杯咖啡、很想吃顿好的或很想放纵一下（[见下](#需求出现在哪)） |

水平是有序的档位。判官从不读也不写数字，`mid` 就是不值一提的中间。

### 谁来判定

需求属于可选的 LLM 判官（配置了 `[tasks.pde_decision].filter_prompt`）。没有判官就没有需求：
没有 `[需求]` 行，回复里没有对应的行，帧和 effects 路由上的 `needs` 为 `null`，事件抽取里没有需求加权，
语音通话里也没有。

- **verdict。** 判官的 JSON 可以带 `needs`：四个键齐全的对象，或 `null`，意思是“不变，沿用上次的判定”。
  解析不了的值按 `null` 处理，永远不会让 verdict 失败。严格的 `response_format` 要求非空对象里四个键齐全。
  有 `social` 和 `emotional`、但缺 `outing` 或 `indulgence` 的对象（判官 prompt 没要这两项，或判官不走严格模式），缺的那一项沿用判官看到的水平（第一次评定之前为 `mid`）；
  缺 `social` 或 `emotional`，整个对象解析不了。engine 不评的键，比如旧版判官 prompt 还会给的 `hunger`、`stamina`，一律忽略。
- **评谁。** 只评角色本人，不评你们之间的关系；关系由好感度评估器评。
- **存在哪。** 存在判官评过的那个回合的用户消息行上，键为 `metadata.needs`，不管这一回合的结局是回复、
  图片、ghost 还是产品咨询。如果这一回合有被接受的 give，由 engine 写入存下的水平，判官弃权时也照写：give 改动的那几项取自基线，
  其余各项取自判官，判官弃权时取基线（见[下文](#give-会改动需求)）。写入会等它完成，回合才继续。
  写入失败只记日志，回合带着本来要写的水平继续往下走。
- **什么情况不写。** 规则引擎回合（判官关闭或失败）、`needs: null` 的 verdict、被离线类种类
  压成沉默的回合（它不跑判官），以及所有群聊和语音回合。例外是配置了判官的部署里、改动了某项需求的被接受的 give：
  只要此前已有一次评定，判官失败或给出 `needs: null` 的回合也照写。没有判官的部署不读基线，give 在那里什么都不写。
- **谁的状态。** 角色实例的。读取时跨该实例的所有 session 找最新一次评定，所以再开一个会话不会让她的需求清零。

判官给出的 `needs` 和它看到的水平，会随 verdict 一起进审计；见
[llm-audit.zh.md](llm-audit.zh.md#pde-判官的-payload)。

### give 会改动需求

配置了判官的部署里，1:1 文字回合的 `give` 一旦被接受，engine 把两项需求各挪动一档：

| `give` 的物品 | `social` | `indulgence` |
|---|---|---|
| `cigarette`、`alcohol`、`herb`、`cuisine` | +1 | +1 |
| `medicine`、`coffee` | −1 | −1 |
| 其他任何物品，包括 `food` | | |

−1 是朝 `low` 方向一档，+1 是朝 `high` 方向一档，到头就停。数量不计：一次 give 只挪一档。
被接受的 `smoke_together`、`smoke_weed_together` 或 `drink_together`，按一次给 1 份 `cigarette`、`herb` 或 `alcohol` 的 give 挪动（见[一起做的动作](#一起淋浴泡澡和蒸桑拿)）。

*被接受*指这一回合实际生效的回应是接受。判官拒绝，或习惯掷骰强制拒收
（见[习惯掷骰](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)），都不挪动任何需求。
失败放行的回合（判官失败）照“判官失败即接受”接受 give，所以挪动在那里同样生效。

写入的内容：

- **基线。** 判官看到的那组水平：这一行用户消息之前最新的一次评定，推到这一行的 `sent_at`。
- **被挪动的需求。** 物品挪动的每一项，存下的水平是基线再挪一档。判官这一回合对该项的评定丢弃：
  它在 `[用户动作]` 里看到了这次 give，常常自己就挪了这几项，再在它的评定上挪一次，这次 give 就算了两遍。
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
| `social` | `high` | 12 小时 |
| `emotional` | `mid` | 12 小时 |
| `outing` | `mid` | 8 小时 |
| `indulgence`，没有习惯为 `true` | `mid` | 8 小时 |
| `indulgence`，一项习惯为 `true` | `high` | 12 小时 |
| `indulgence`，两项习惯为 `true` | `high` | 8 小时 |
| `indulgence`，三项及以上习惯为 `true` | `high` | 4 小时 |

离开得越久，她越想找人说话；情感需求和想出门或想待着的念头则向中间回落。中午评成 `low` 的社交欲，午夜读作 `mid`，
第二天中午起读作 `high`。习惯指角色的 `smoking`、`alcoholic`、`drug`、`coffee`、`gluttony` 五个键（见[角色设置](#角色设置)）；`false` 和不写算作一样。
为 `true` 的越多，判官把这份渴求评低之后，它回来得越快。存下时没有 `outing` 和 `indulgence` 的评定，这两项都读作 `mid`；还带着 `hunger` 或 `stamina` 的评定，读的时候忽略这两个键。
BYOC 角色的这几个键在她的卡片上，engine 只在一轮对话里才打开卡片：在对话之外（`GET …/effects`，以及回放时退回到推算水平的那一步），
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
  [需求] 社交欲=低 情感需求=高 外出=中
  [需求] 社交欲=低 情感需求=高 外出=中 享受=高
  ```

  `外出` 每次都在；`享受=高` 只在 `indulgence` 为 `high` 时出现，没有它就读作“不高”（参考判官 prompt 里写明了这一点）。
  严格模式下的判官每个回合仍然要评 `indulgence`，却看不出它原来是 `low` 还是 `mid`，所以连续聊天时它每一轮都在重新猜这一项，
  按习惯数目的漂移只在两次评定之间起作用。第一次评定之前读作 `[需求] 尚未判定`。这一行永远在，因为缺这一行会被读成“全是低”。
- **回复**得到不是 `mid` 的那些水平，按上表的顺序，用 `；` 连起来，作为 `[character_state]` 里的一行：

  ```
  [character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
  - 此刻的需要：想找人说说话；很想被在乎
  ```

  `indulgence` 只在 `high` 时出现。它点名一样她用的东西，每一轮从值为 `true` 的习惯键里随机挑一个：`smoking` 是很想抽根烟，
  `alcoholic` 是很想喝一杯，`drug` 是很想抽口大麻，`coffee` 是很想喝杯咖啡，`gluttony` 是很想吃顿好的；没有键为 `true` 时读作很想放纵一下。各项都是 `mid`（`low` 的 `indulgence` 在这里也按 `mid` 算），
  或还没有评定，就什么都不加，prompt 保持不变。被判官评过的回合，回复按这一回合最终的水平来写：
  写在这一行上的水平（被接受的 give 挪动的那几项取自基线），否则用判官看到的那组推算后的水平。
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

engine 用到需求的地方只有一处：事件抽取的权重（见下）。没有任何 ghost 规则或图片规则读它们。不想跟人打交道的角色
不会因为规则更容易 ghost；判官看到这些水平，自己判断。

## 生活事件

### 类别

可抽取的事件共 70 种，每一种属于一个类别，类别决定它的形态：

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

另有二十一种永远不会被抽到：`taking_space`（说好暂时分开）、`fell_out`（为分开的事闹得不愉快）、`traveling_together`（和对方一起旅行）、`bathing_together`（和对方一起洗澡）、`on_a_date`（和对方约会）、`showering_together`（和对方一起淋浴）、`sauna_together`（和对方一起蒸桑拿），以及其余一起做的动作写入的十四种，从 `using_toilet_together` 到 `fighting_together`（见[一起做的动作](#一起淋浴泡澡和蒸桑拿)）。
前两种属于已经退役的提出分开：engine 不再写入它们，它们只留在存量数据里（见[选择存在哪](#选择存在哪)）。
它们按形态是 `mid` 插曲，其中 `taking_space` 超出了这个类别的 4 小时：它持续 12 到 24 小时。
`traveling_together` 是 `trip` 类的背景，用户接受她的提议时由 engine 写入（见[一起去旅行](#一起去旅行)）；
`showering_together` 和 `bathing_together` 是 30 分钟的 `short` 插曲，用户答应和她一起洗澡时由 engine 写入其中之一，两者机会均等（见[一起洗澡](#一起洗澡)），她答应用户的 `shower_together` 或 `bath_together` 时也会写入；
`sauna_together` 是 1 小时的 `short` 插曲，她答应用户的 `sauna_together` 时由 engine 写入（见[一起淋浴、泡澡和蒸桑拿](#一起淋浴泡澡和蒸桑拿)）；
其余十四种是她答应用户其余一起做的动作时由 engine 写入的插曲，除 `asleep_together` 和 `fighting_together` 是 `mid` 外都是 `short`（`having_sex_together` 和 `asleep_together` 在用户答应她的 `sleep_together` 或 `fall_asleep_together` 时也会写入，见[浪漫选择](#浪漫选择)）；
`on_a_date` 是 1 或 4 小时的 `short` 插曲，她答应用户的约会邀约时由 engine 写入（见 [activities.zh.md](activities.zh.md#事件)）。
这二十一种都没有抽取规则，调用方的 `kinds` 也拒绝它们。

### 抽取

抽取是输入加三个随机数（两段各一个，时长一个）的纯函数。

1. **资格。** 一个种类同时满足下面几条才有资格：
   - 调用方的 `kinds` 允许它（[见下](#调用方的-kinds)；不带表示全部 70 种）；
   - 它属于她的世界（见[下文](#她的世界)）；
   - 过了它的门槛（见下）；
   - 它自己的冷却已过；
   - 它所属类别的冷却已过；
   - 离线类种类还要过了共享的离线冷却（见下）；
   - 背景规则允许它（见下）；
   - 需要背景的种类（`got_lost`、`missed_ride`、两种赶路和交通工具），它的某个背景正在进行（见下）。
2. **类别。** 每个至少有一个合格种类的类别，权重是类别权重乘以角色系数。掷一次随机数，按累计权重选一个。
3. **种类。** 该类别里每个合格种类的权重是基础权重乘以它的各项[需求系数](#1-需求给种类加权)、
   `prefer_events` 系数、[关联系数](#关联系数)，少数种类再乘一个角色系数（[`is_virgin`](#角色设置)、
   [`homesick`](#角色设置)、[`sex_addict`](#角色设置)、[`hygienic`](#角色设置)、[`employed`](#角色设置)、[`outdoorsy`](#户外还是室内)）。再掷一次，选一个。
4. **时长。** 固定时长照用；区间则掷出来。

没有合格种类就没有事件，响应是 `{"event": null, "message": null}`。只要有合格种类，抽取就一定得到事件。
她的一天有多热闹，取决于调用方什么时候来要，以及冷却。

因为分两段，一个类别的占比不取决于它里面有多少种类。四个类别都合格、角色没有任何设置时，一次抽取有一半落在
`short`，40% 落在 `mid`，`body` 和 `trip` 各 5%。没有合格种类的类别退出第一段，其余类别分掉它的权重。

### 同一时间一件插曲、一个背景

- 有插曲在进行时，完全不抽，连背景也不抽。
- 有背景在进行时，不会抽第二个背景，插曲照抽；只是住院（`hospitalized`）和术后（`post_surgery`）期间，
  需要她出门活动的那几种不抽：`temp_work`、`police_encounter`、`animal_encounter`、`hit_someone`、
  `got_hit`、`working_out`、`on_the_way`、`in_battle`、`got_bumped`、`saw_bag`、`saw_clothes`、`out_shopping`、
  `bros_gathering`、`sis_gathering`、`admiring_scenery`、`admiring_art`、`watching_show`、`on_the_road`、`having_sex`、
  `bird_encounter`、`streaking`、`got_lost`、`drinking`、`smoking`、`smoking_weed`、`just_have_sex`。
  `period` 期间，
  不抽 `period_scare` 与 `pregnancy_scare`。`traveling_together` 期间，不抽任何可能让她离线的种类，也不抽 `just_have_sex`：她和用户在一起。
- 有十种是需要背景，而不是被背景排除。`got_lost` 和 `on_a_cruise` 只在 `short_trip`、`long_trip`、`hometown_visit` 或
  `traveling_together` 进行期间抽取，出差时和不在旅途中都不抽。`missed_ride`、`on_the_way`、`on_the_road`、`on_a_boat`、
  `on_a_bus`、`on_a_train`、`on_a_plane` 和 `driving` 在这几种或 `business_trip` 进行期间抽取。
- 她在等用户回答自己提出的某次生活选择时，同样完全不抽，角色的任何一个 session 都一样：这个请求本身就是前台戏，
  也免得一段离线时段在问话中间开始。带着已经抽过的 `event_key` 来重复调用，照样返回那个事件。
- 她有一场还没结算的约会时，不论正在进行还是已经结束，同样完全不抽，角色的任何一个 session 都一样：她正和用户在外面，
  或者已经回来、约会还没进她的记忆（见 [activities.zh.md](activities.zh.md#暂停的东西)）。重复的 `event_key` 照样返回它的事件。
- 一件插曲和一个背景可以同时进行。背景里抽到的插曲，是从背景里讲出来的。
- 这以角色实例为单位，跨它所有的 session：她只有一段人生。抽取会读该实例最近 90 天的事件，也就是最长的那个冷却。

写入时用同一条规则做原子检查：有插曲在进行时不写入另一件插曲；有任何事件在进行时不写入背景。

### 冷却与上限

- **按种类。** 一个种类要等它最新一次事件*开始*后过了冷却才能再开始，不是结束后。冷却最短 12 小时
  （`showering`、`reading` 等），重的种类很长（`hit_someone` 14 天、`breakup` 30 天、`hospitalized` 60 天、
  `post_surgery` 90 天）。四个 `trip` 种类自己没有冷却，由类别冷却管。
- **按类别。**`body` 每两周最多开始一次，`trip` 每周最多开始一次，不论具体是哪个种类。`short` 与 `mid` 没有。
- **离线类共享。** 任何一个离线类种类开始后的 12 小时里，所有离线类种类都没有资格，不论刚才是哪一个；
  各自的冷却照样适用。它限制她沉默的频率，就像类别冷却限制出行和身体状态的频率。
  `out_shopping` 只对被它带进离线的角色才算离线类种类。
- **按 key。** 同一个 session 里同一个 `event_key` 返回同一个事件，不抽取，也不生成。
- **不设总量上限。** engine 不限制调用方多久来要一次。频率由冷却加上调用方自己的策略决定。要生成消息的抽取会占一个
  单用户并发名额，所有 LLM 入口共用这个上限（到上限返回 `429`）。

### 门槛

门槛决定一个种类能不能被抽到。偏好和需求系数只改权重，不会绕过门槛。

| 种类 | 门槛 |
|---|---|
| `ex_message`、`breakup`、`ex_wants_back` | 亲密度档 2 及以上 |
| `sudden_desire`、`just_have_sex` | 亲密度档 3 |
| `masturbating` | 角色的 `art_metadata.sex_addict` 为 `true`，并且亲密度档 3 |
| `drinking`、`smoking`、`smoking_weed` | 角色的 `art_metadata.alcoholic`、`smoking`、`drug` 分别为 `true` |
| `got_lost`、`on_a_cruise` | 有 `short_trip`、`long_trip`、`hometown_visit` 或 `traveling_together` 正在进行（见[上文](#同一时间一件插曲一个背景)） |
| `missed_ride`、`on_the_way`、`on_the_road`、`on_a_boat`、`on_a_bus`、`on_a_train`、`on_a_plane`、`driving` | 有 `business_trip`、`short_trip`、`long_trip`、`hometown_visit` 或 `traveling_together` 正在进行 |
| `strip`、`streaking` | 亲密度档 3，且会话最新记录的 `affinity_scope` 性向相容（scope 里有暧昧感轴）；没有记录算不相容 |
| `saw_bag`、`saw_clothes` | 角色的 `art_metadata.gender` 为 `"female"` |
| `period` | 角色的 `art_metadata.gender` 为 `"female"`，并且她不是机器（`art_metadata.species` 为 `"robot"` 或 `"android"`） |
| `want_to_quit_job` | 角色的 `art_metadata.employed` 不为 `"free"` |
| `period_scare` | 角色的 `art_metadata.gender` 为 `"female"`，她不是机器，并且亲密度档 2 及以上 |
| `pregnancy_scare` | 角色的 `art_metadata.gender` 为 `"female"`，她不是机器，并且亲密度档 3 |
| `using_toilet` | 她不是机器 |
| `having_sex` | 角色的 `art_metadata.sex_addict` 为 `true` |
| `away_from_phone`、`busy`、`in_battle`、`asleep`、`bros_gathering`、`sis_gathering`、`having_sex`，以及女性角色的 `out_shopping` | ghost 保护允许（见[下文](#离线类)） |

`just_have_sex` 只看档位：它和 `strip` 不同，不查性向是否相容。

`out_shopping` 和两个聚会没有性别门槛：谁会因此离线、为什么两种性别都会抽到两个聚会，见
[逛街与聚会](#逛街与聚会)。

档位（1 到 3）读自该 session 的好感度，和聊天回合一样先做时间衰减；见 [affinity-model.zh.md](affinity-model.zh.md)。

### 角色设置

角色 genome 的 `art_metadata` 里有十六个键，由 engine 读取，由写角色的人填写，和 `gender`、`birthday`、
`timezone` 放在一起：

| 键 | 取值 | 效果 |
|---|---|---|
| `prefer_events` | `mid` 种类的数组 | 列出的种类在 `mid` 内权重为 3 倍 |
| `health` | `"weak"`、`"normal"`（默认）、`"strong"` | `body` 类别权重分别为 2 倍、1 倍、一半 |
| `prefer_trip` | `true`、`false`（默认） | `trip` 类别权重为 3 倍 |
| `homesick` | `true`、`false`、不写（默认） | `trip` 内部：`true` 时 `hometown_visit` 3 倍、`short_trip` / `long_trip` 三分之一；`false` 时 `hometown_visit` 三分之一；`business_trip` 是工作，两种情况都不动 |
| `is_virgin` | `true`、`false`（默认） | 她还是处女期间，`having_sex` 在 `mid` 内、`just_have_sex` 在 `short` 内权重为五分之一 |
| `sex_addict` | `true`、`false`、不写（默认） | `short` 内部：`true` 时 `sudden_desire`、`strip`、`streaking` 与 `just_have_sex` 3 倍，`false` 时三分之一；它们的门槛两种情况都照旧。只有 `true` 才会抽到 `having_sex` 和 `masturbating` |
| `hygienic` | `true`、`false`、不写（默认） | `short` 内部：`true` 时 `showering`、`bathing`、`want_hot_spring` 与 `want_sauna` 3 倍，`false` 时三分之一；它也决定[一起洗澡](#一起洗澡)要 chemistry 到第几档 |
| `employed` | `"free"`、`"part_time"`（默认）、`"full_time"` | 她的一天有多少是工作：`temp_work`、`scolded_by_superior`、`scolded_superior` 与 `business_trip` 在各自类别内分别为三分之一、1 倍、3 倍；`free` 的角色不会抽到 `want_to_quit_job`；其他值按 `part_time` 读 |
| `outdoorsy` | `"outdoor"`、`"neutral"`（默认）、`"indoor"` | 每个类别内部：`outdoor` 时户外种类 3 倍、室内种类三分之一，`indoor` 反过来；中性种类不动（见[下文](#户外还是室内)）；其他值按 `neutral` 读 |
| `setting` | `"modern"`（默认）、`"fantasy"` | 她生活的世界（见[下文](#她的世界)）；不是 `fantasy` 的角色拒绝用户的 `fight_together`（见[一起做的动作](#一起淋浴泡澡和蒸桑拿)）；其他值按 `modern` 读 |
| `species` | 任意词 | `"robot"` 和 `"android"` 是机器：她不会抽到 `period`、`period_scare`、`pregnancy_scare`、`using_toilet`，也拒绝用户的 `toilet_together`（见[一起做的动作](#一起淋浴泡澡和蒸桑拿)）；其他词不影响抽取。这个词对 prompt 的作用见 [eros-character-v1.1.md](eros-character-v1.1.md) |
| `smoking`、`alcoholic`、`drug`、`coffee`、`gluttony` | 各为 `true`、`false`、不写（默认） | 她抽不抽烟、喝不喝酒、用不用大麻、喝不喝咖啡、贪不贪吃（`false` 表示她忌口、节食）：决定 `indulgence` 回来得多快（见[漂移](#两次评定之间的漂移)）、`high` 的 `indulgence` 在回复里点名什么，以及她怎么对待递来的这样东西、怎么对待用户请她一起抽烟、抽大麻或喝酒（见下）；`smoking`、`alcoholic`、`drug` 为 `true` 时，她才会抽到 `smoking`、`drinking`、`smoking_weed` |

`prefer_trip` 管她动不动，`homesick` 管她动起来往哪去。两者都为 `true` 的空姐角色经常出行、照常出差，
放假回家而不是去别处。

抽取看到的「是否处女」是推导出来的：卡上写了 `is_virgin`，并且这个实例从来没有过 `having_sex`、`just_have_sex` 或 `having_sex_together` 事件。
其中任何一种的第一条行写下之后就不再是了，不需要任何写入：下一次抽取读到那一行就行。她在约会里答应的 `sex_together` 不写行，所以不改变这一点。这个键不进任何
prompt；她是什么样的人由写角色的人写进人设，和其他人设事实一样。`sex_addict` 同样不进任何 prompt。

`hygienic`、`employed` 和 `outdoorsy` 也不进任何 prompt，而且不算习惯：它们不影响 `indulgence`。`hygienic` 是单独的一个系数，
和同一种类上的需求系数相乘。`employed` 不动 `busy` 和 `in_battle`，也不改任何冷却或时长。

#### 户外还是室内

每种插曲要么在户外，要么在室内，要么中性；`outdoorsy` 只读这一点。户外指露天：商店、教室、办公室、美术馆都不算户外。

| 场所 | `short` | `mid` |
|---|---|---|
| 户外 | `police_encounter`、`animal_encounter`、`on_the_way`、`got_bumped`、`admiring_scenery`、`bird_encounter`、`want_to_see_sea`、`want_hot_spring`、`got_lost`、`missed_ride` | `on_the_road`、`on_a_boat`、`on_a_cruise`、`on_a_bus`、`on_a_train`、`on_a_plane`、`driving`、`want_to_go_out` |
| 室内 | `showering`、`bathing`、`scolded_by_superior`、`scolded_superior`、`admiring_art`、`want_sauna`、`using_toilet` | `reading`、`gaming`、`want_to_go_home` |
| 中性 | 其余 `short` 种类，性相关的种类与 `streaking` 都在其中 | 其余 `mid` 种类 |

可能让她离线的种类（`asleep`、`in_battle`、`busy`、`away_from_phone`、`out_shopping`、两个聚会、`having_sex`）一律中性，
所以 `outdoorsy` 不会改变她联系不上的频率。它和同一种类上的其他系数相乘，不改门槛、冷却、时长，也不改各类别出现的比例。

读取是宽松的。类型不对的值按没写处理；`prefer_events` 里不是已知 `mid` 种类的条目被丢掉，其余保留。
`health` 缩放整个 `body` 类别，`period` 也在内，`period` 自己 28 天的冷却让它不管怎样都大约每月一次。
`prefer_trip` 提高的是出行的概率，不是频率：每周一次的类别冷却对它同样适用。

五个习惯键通过 `indulgence` 的漂移影响抽取，其中三个（`smoking`、`alcoholic`、`drug`）还是习惯类事件的门槛（见[下文](#迷路习惯与性)）。判官的角色块（`[角色人格]`）不列它们，所以她不会每一轮都提烟、酒、大麻、咖啡或吃的；
希望这些习惯带进她说话的角色，要在人设里自己写明。engine 从不写这五个键。

**递来她不用的东西。** 一轮的动作是 `give`，递的是 `cigarette`、`alcohol`、`herb`、`coffee` 或 `cuisine`，而对应的键（`smoking`、`alcoholic`、`drug`、`coffee`、`gluttony`）
为 `false` 时，engine 在判官运行之前掷一次。三次里有一次，这一轮照常进行。其余情况她这一次不收：

- 判官的 `[用户动作]` 一行末尾加上 `，这次你不会收下`，判官的 `inner_state` 就按拒收来写；这一行只陈述结果，不给理由；
- 不管判官怎么答，生效的回应都是 `refuse`；没有 LLM 判官时，规则引擎的接受同样变成 `refuse`。

此后它就是一次普通的拒绝：`action_response` 帧和存下的 `response` 都是 `refuse`，这次 give 不留余效，bond 线降一档
（见[被拒绝的动作](affinity-model.zh.md#被拒绝的动作)），回复也按拒收来写。判官自己的回答留在它的审计 payload 里。
`true` 和不写从不掷，其他物品也不掷。被重新驱动的回合会重新掷一次，就像判官也会重新判一次。

`smoke_together`、`smoke_weed_together` 或 `drink_together` 也这样掷，按一次给 1 份 `cigarette`、`herb` 或 `alcohol` 的 give，
看 `smoking`、`drug` 或 `alcoholic` 键。被拒时 `[用户动作]` 一行末尾改为 `，这次你不会答应`，降一档的是 chemistry 线，不是 bond。
其他动作都不掷。

### 关联系数

她的一天是连着的：一件**在线**的事件会让生活里跟在它后面的种类更容易抽到。先行事件在进行中，或结束后
48 小时内，算在线。抽取第二段里，每有一个指向它的、不同种类的在线先行事件，该种类权重乘 3；两个就是 9 倍。
这个系数不解除门槛（在线的 `sick` 只对档 2 以上的女性角色提高 `period_scare`），不碰第一段，也不缩短冷却。

| 在线的先行事件 | 权重乘 3 的种类 |
|---|---|
| `ex_message`、`sudden_desire`、`strip` | `having_sex`、`just_have_sex` |
| `sudden_desire` | `strip`、`masturbating` |
| `sick` | `period_scare` |
| `having_sex`、`just_have_sex`、`having_sex_together` | `pregnancy_scare` |
| `business_trip` | `on_the_way`、`busy`、`temp_work` |
| `short_trip`、`long_trip`、`hometown_visit`、`traveling_together` | `on_the_road`、`admiring_scenery`、`taking_selfie` |
| `hometown_visit` | `bros_gathering`、`sis_gathering` |
| `hospitalized`、`post_surgery` | `reading`、`gaming` |
| `period` | `irritable`、`unwell` |
| `breakup`、`quarrel`、`scolded_by_superior`、`in_battle` | `irritable` |
| `got_hit`、`got_bumped` | `unwell` |
| `working_out` | `showering`、`bathing` |
| `ex_message`、`breakup` | `ex_wants_back` |
| `scolded_by_superior`、`temp_work` | `want_to_quit_job` |
| `breakup`、`quarrel`、`want_to_quit_job` | `pondering_life` |
| `pondering_life` | `want_to_see_sea` |

有一种是压低而不是排除：实例从来没有过 `having_sex`、`just_have_sex` 或 `having_sex_together` 事件时，`pregnancy_scare` 权重为五分之一。没有硬性的
「只能在之后」门槛，也没有状态机：三种都从未发生的部署里，这个虚惊只是降到五分之一，不会消失。
表和常量（`FOLLOW_FACTOR`、`FOLLOW_WINDOW_HOURS`、`UNLIKELY_FACTOR`）在 `crates/eros-engine-core/src/events.rs`。

### 调用方的 `kinds`

调用方的 `kinds` 是另一个手段：角色的世界观或部署方的策略用它排除某些种类，engine 里并不丢掉这些种类。

| `kinds` | 可以抽的种类 |
|---|---|
| 不带或 `null` | 全部 70 种 |
| `[]` | 一种都不抽：响应是 `{"event": null, "message": null}` |
| `["reading", "quarrel"]` | 只抽列出的 |
| `{"allow": […], "disallow": […]}` | `allow` 允许的，去掉 `disallow` 列出的 |

对象里，`allow` 不带或为 `null` 表示全部允许，`[]` 表示一种都不允许；`disallow` 不带、为 `null` 或 `[]` 都不去掉任何种类；
两边都列了的种类不抽。只写 `{"disallow": […]}` 就能排除种类而不冻结其余种类，engine 以后新增的种类也会照常抽取。
对象里出现未知的键，或任何地方出现未知种类，请求都会被拒绝，所以拼错的 `disallow` 不会悄悄放出本想排除的种类；
代价是调用方要等 engine 认识某个新种类之后才能列它。`taking_space`、`fell_out`、`traveling_together`、`bathing_together`、`on_a_date`、`showering_together`、`sauna_together` 以及其余一起做的动作写入的十四种永远不会被抽到，
所以在 `kinds` 的任何位置写它们，都按同样的方式被拒绝。

有七种带成人题材：`sudden_desire`、`strip`、`streaking`、`having_sex`、`just_have_sex`、`masturbating`，以及隐含性行为的 `pregnancy_scare`。engine 不内建这类分组；只对部分用户提供这类内容的调用方，
把它们列进 `disallow`。`having_sex_together` 和 `streaking_together` 也带成人题材，但不会被抽到：只有被接受的
`sex_together` 或 `streak_together` 会写入它们，所以这样的调用方不发这两种动作。`scolded_by_superior` 与 `scolded_superior` 假定她有上级，在她的世界里读作老板、上司、领导、主人或君主；
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

**情绪化语气。** 部署打开 [`tones.emotional`](model-config.zh.md#tones--聊天语气仅限-chat-任务)（大于 0）时，
十六种事件用 `emotional` 语气讲出来：`quarrel`、`scolded_by_superior`、`scolded_superior`、`got_hit`、`hit_someone`、`police_encounter`、`ex_wants_back`、`irritable`、`want_to_quit_job`、`unwell`、`sick`、`period`、`sudden_desire`、`drinking`、`smoking_weed`、`just_have_sex`。这时请求只有情绪化的 system prompt 和提示：没有历史窗口、
没有记忆召回、没有用户画像，也没有亲密度结论；`[shared_memories]` 放这段关系最近的三个 episode，`[now]`
保留正在进行的事件。这条消息的行带 `metadata.tone = "emotional"`。其余事件，以及语气关闭时的所有事件，
照上面的方式讲。

然后：

- 输出正则规则照常生效。回复为空，或被规则剥到一无所剩，什么都不写，响应的 `event` 和 `message` 都是 `null`。生成失败返回上游错误，什么都不写。
- 事件行和消息在同一个事务里写入。消息的 `sent_at` 就是事件的 `started_at`，是生成完成后取的同一个时刻；
  `ends_at` 是这个时刻加上抽到的时长。如果写入时输给了已经在进行的事件，消息随之作废。
- 这条消息是一行普通的 assistant 消息，`assistant_action_type = 'proactive'`、`metadata.proactive = 'event'`，
  在调用已读接口之前一直是未读。它不跑 post-process：不写记忆，不产生好感度事件，不动 ghost 连续次数。
  它会更新 session 的 `last_active_at`。
- **竞态时用户不优先。** 事件生成期间落进会话的用户消息不会让事件作废：不论用户有没有开口，它都已经发生。
- **可以要流式响应。** 这个路由可以改用 server-sent event 流回答：先给抽到的事件，再边生成边给她的消息。
  事件仍然和消息一起写入，所以客户端要等到这条消息的 `done` 到了才保留它。见
  [API 参考](api-reference.zh.md#openeventslife-choices-的流式响应)。

### 持续期间

某一时刻正在进行的事件，是满足 `started_at <= 该时刻 < ends_at` 的那个背景和那件插曲。在聊天里它们到达四个读取方；
开启世界系统时，故事导演和世界导演也会读到（见[聊天之外](#聊天之外)）。

- **判官**得到一行 `[近期事件]`，排在 `[需求]` 之后、`[身体状态]` 之前。先列背景，再列插曲，用 `；` 连接，
  每项形如 `{label}（{span}前开始，约 {span}后结束）`：

  ```
  [近期事件] 长途旅游（2 天前开始，约 3 天后结束）；在看书（不到 1 小时前开始，约 2 小时后结束）
  ```

  不到一小时的跨度读作 `不到 1 小时`（结束那一半此时不带 `约`）；不满 48 小时按整小时四舍五入；
  从 48 小时起按整天四舍五入。`{label}` 是每个种类固定的中文名（`reading` 是 `在看书`，`long_trip` 是
  `长途旅游`）。没有进行中的事件、没有未解释的离线时段、没有等着标记归来的存量分开、也没有未交代的虚惊（见下）时，这一行不出现。
- **回复**在 `[now]` 块里得到 `- 正在经历：`，排在时间、节日、生日和纪念日几行之后，列出同样两件事，只写结束时间；
  下文的 `- 刚才：` 行紧跟其后。两行都不在 `[character_state]` 里：那个块的表头让位于它上面的块，
  放在那里，事件开始之前写下的故事摘要就会盖过事件：

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  ```

  有没有 LLM 判官都一样。`/open` 开场白和事件消息带同一行。语音通话、群聊轮次和改图的回复 prompt 不读事件。
- **画图合成器**得到一段 `[此刻状态]`，排在 `[身体状态]` 之后，先列背景、再列插曲的标签，用 `；` 连接：

  ```
  [此刻状态]
  短途旅游；在坐火车
  ```

  聊天流水线画的每一张图都带它：聊天回合的图、事件消息自己的图，以及群聊回合的图（写发言人自己的状态）。没有进行中的事件时
  载荷里没有这一段。正在开始的事件即使还没落库也算进行中：事件消息的图画的是它讲述的事件，被接受的
  [一起做的动作](#一起淋浴泡澡和蒸桑拿)所出的图画的是它写入的事件。独立的 compose 端点和改图不发这一段。见
  [合成器](model-config.zh.md#taskschat_image_prompt_compose--出图-prompt-合成器图片轮次必需)。
- **调用方**从 `GET …/effects` 拿到：`event` 是进行中的插曲，`background_event` 是进行中的背景。
  进行中的离线类事件和其他插曲一样作为 `event` 返回，带 `"offline": true`；客户端怎么展示由它自己决定。

文字回合按自己那行用户消息的发送时间来读事件，所以被重新驱动的回合看到的是同样的事件。

### 离线类

八个可抽取的种类会让角色联系不上，直到它们结束：`away_from_phone`、`busy`、`in_battle`、`asleep`、`bros_gathering`、
`sis_gathering`、`having_sex`，以及只对女性角色生效的 `out_shopping`。离线是事件持续期间她的行为方式，
不是类别：`busy` 按时长属于 `mid`，按行为属于离线。它们的门槛、沉默、例外和事后的解释都相同；
区别只在措辞，以及她讲不讲出来。`in_battle` 的措辞让她的世界观决定它是什么：现代角色把它读成一场艰难的谈判、
一顿商务宴请或一件难办的事，历史或奇幻角色把它读成一场真正的战斗。存量的 `taking_space` 对所有角色都是离线，
走的是同一套机制（见[提出分开的存量数据](#选择存在哪)）。各种类及其时长见
[events 路由](api-reference.zh.md#post-v2compsessionsession_idevents)。

**讲出来，还是沉默。** 哪些种类算离线、开始时讲不讲：

| 种类 | 谁会离线 | 讲不讲 |
|---|---|---|
| `away_from_phone`、`asleep` | 所有角色 | 从不讲 |
| `busy`、`in_battle` | 所有角色 | 亲密度档 2 起讲；以下沉默 |
| `out_shopping` | 女性角色；其他角色照常回复 | 女性角色档 2 起讲、以下沉默；其他角色每一档都讲，是一件普通的插曲 |
| `bros_gathering`、`sis_gathering` | 所有角色 | 从不讲；事后由她解释 |
| `having_sex` | 性瘾角色（只对她抽取） | 从不讲；事后直说（`在做爱，对方那时发来的消息你现在才看到`），跟谁由她自己说 |
| `taking_space`（只有存量行） | 所有角色 | 从不讲 |

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
`在逛街，对方那时发来的消息你现在才看到`。存量的分开自有两种说法（见 [model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)）。

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

五种会讲出来的插曲，给她一个用户听得到的下午：`admiring_scenery`、`admiring_art`（`short`，1 小时）、`taking_selfie`
（`short`，30 分钟）、`on_the_road`（`mid`，2 到 4 小时：一段长路；只在旅行中，也只在 `fantasy` 的世界里，现代角色改抽
[交通工具](#交通工具)；`on_the_way` 仍是那 30 分钟的赶路，同样只在旅行中）与 `watching_show`（`mid`，2 到 3 小时）。期间她照常回复。提示只陈述事实，细节由她填：看的什么风景、
哪件作品、什么表演、坐的什么。请求带 `image`、部署配了 composer 时，`taking_selfie` 的消息带上这张自拍：先写她的话，
composer 照着她的话画（见 [api-reference](api-reference.zh.md#post-v2compsessionsession_idevents)）；否则只有文字。`taking_selfie`
住院也抽；其余四种要她出门。

### 脱给你看

`strip`（`short`，30 分钟，权重 1，冷却 48 小时）是她脱衣服给对方看。它要亲密度档 3，并且这一对性向相容
（见门槛表）。突然很想要会让它更容易出现，它又会让做爱更容易出现，`sex_addict` 对它的缩放与 `sudden_desire` 相同。
请求带 `image`、部署配了 composer 时，消息会像自拍一样带上她的图；提示句说她刚脱了衣服拍了一张发过去，否则说她想让对方看看。
消息里的穿着写成刚把衣服脱了，请求带 `outfit` 时写成刚把「{outfit}」脱了，所以图里不会是穿着衣服的她。它和 `sudden_desire`
一样带成人题材，只对部分用户提供这类内容的调用方把它排除。

### 一起淋浴、泡澡和蒸桑拿

用户可以请她一起淋浴、泡澡或蒸桑拿：`shower_together`、`bath_together`、`sauna_together` 三种动作，都不带字段
（见[发送路由](api-reference.zh.md#post-compchatsession_idmessagestream)）。另有十四种一起做的动作，同样不带字段，请她和用户一起做别的事（见下表）。
和任何动作一样，由判官决定。在判官之前，
性向不相容的一对发来的淋浴或泡澡，和 `strip` 一样被判为拒绝：不管判官怎么答，也不管有没有 LLM 判官，她都拒绝。蒸桑拿没有这条规则。
这样的一对发来的 `sex_together` 或 `streak_together` 同样如此，机器角色（见[角色设置](#角色设置)）收到的 `toilet_together` 也一样，
世界不是 `fantasy` 的角色（见[她的世界](#她的世界)）收到的 `fight_together` 也一样；
其余一起做的动作没有这类规则，`smoke_together`、`smoke_weed_together` 和 `drink_together` 要过习惯掷骰（见下）。
被拒时 chemistry 线降一档，什么都不写；她的世界排除掉的 `fight_together` 被拒时哪条线都不降；她自己提出的 `bathe_together`（[一起洗澡](#一起洗澡)）及其冷却，不读这些动作，这些动作也不读它们。

被接受的动作让她进入一个事件，与存下的回答在同一个事务里写入：

| 动作 | 事件 | 标签 | 持续 |
|---|---|---|---|
| `shower_together` | `showering_together` | 和对方一起淋浴 | 30 分钟 |
| `bath_together` | `bathing_together` | 和对方一起洗澡 | 30 分钟 |
| `sauna_together` | `sauna_together` | 和对方一起蒸桑拿 | 1 小时 |
| `toilet_together` | `using_toilet_together` | 和对方一起上厕所 | 30 分钟 |
| `smoke_together` | `smoking_together` | 和对方一起抽烟 | 30 分钟 |
| `fall_asleep_together` | `asleep_together` | 和对方睡在一起 | 1–4 小时 |
| `sex_together` | `having_sex_together` | 和对方上床 | 1 小时 |
| `walk_together` | `walking_together` | 和对方一起散步 | 1 小时 |
| `smoke_weed_together` | `smoking_weed_together` | 和对方一起抽大麻 | 30 分钟 |
| `jog_together` | `jogging_together` | 和对方一起慢跑 | 1 小时 |
| `yoga_together` | `yoga_together` | 和对方一起做瑜伽 | 1 小时 |
| `stargaze_together` | `stargazing_together` | 和对方一起看星星 | 1 小时 |
| `ride_horse_together` | `horse_riding_together` | 和对方一起骑马 | 1 小时 |
| `streak_together` | `streaking_together` | 和对方一起裸奔 | 30 分钟 |
| `drink_together` | `drinking_together` | 和对方一起喝酒 | 1 小时 |
| `hot_spring_together` | `hot_spring_together` | 和对方一起泡温泉 | 1 小时 |
| `fight_together` | `fighting_together` | 和对方并肩作战 | 1–2 小时 |

除 `asleep_together` 和 `fighting_together` 是 `mid` 插曲、时长按整小时抽定外，都是 `short` 插曲。
开始时结束正在进行的插曲，在途的背景照常继续，和她提出的洗澡一样。事件以用户那条消息为键，所以被重新驱动的回合只写一次。
她的约会进行期间什么都不写：约会的 `on_a_date` 保持原样，约会继续。她不把这个事件当作事件来讲，因为她对这个动作的回复已经说了；
它不离线，`asleep_together` 期间她也照常回复；十七种都不会被抽到。期间它就是正在进行的插曲，所以 `正在经历`、判官和画图合成器像读其他插曲一样读它。
`showering_together`、`bathing_together`、`having_sex_together`、`streaking_together`、`using_toilet_together` 和 `asleep_together`
不进故事导演读的事件，和她同睡请求的结果一样；其余十一种会进。

`smoke_together`、`smoke_weed_together` 和 `drink_together` 只在三件事上算作给了她 1 份
`cigarette`、`herb` 或 `alcohol` 的 give：习惯掷骰（见[角色设置](#角色设置)）、被接受时挪动的需求（见 [give 会改动需求](#give-会改动需求)），
以及留下的余效（见[发送路由](api-reference.zh.md#post-compchatsession_idmessagestream)）。被拒时降的是 chemistry，不是 bond，
也不往[纪事](memory-layers.zh.md#纪事)里写任何时刻。

`sex_together` 是她自己提出的 `sleep_together` 由用户反过来提。被接受后，这个会话里她的这项请求就算有了结果（见[浪漫选择](#浪漫选择)）；
用户接受她的 `sleep_together` 时也写入同一个事件。这个事件算作性：结束她的处女身份（见[角色设置](#角色设置)），也让验孕虚惊更容易出现（见[关联系数](#关联系数)）。
被接受之后用户还可以再提，每次都由判官决定。`fall_asleep_together` 同样是她自己提出的 `fall_asleep_together` 由用户反过来提：
被接受后，这个会话里她的这项请求就算有了结果，用户接受她的 `fall_asleep_together` 时也写入同一个 `asleep_together`。

被接受的一起做的动作，在请求带 `image`、部署配了 composer 时是一个出图回合，和被接受的 `photo`、`strip`、`ask_dancing` 一样：先是她的话，
再是一条 `image_request`。合成器会被告知这个事件是她此刻的状态，所以图里画的就是它；在约会里什么都不写时，这个动作的种类只作为这一回合她的状态。
被拒的只回文字。部署的 [`tones.emotional`](model-config.zh.md#tones--聊天语气仅限-chat-任务) 大于 0 时，对淋浴、泡澡、`sex_together` 和 `streak_together` 的回复用 `emotional` 语气，接受、拒绝都一样；
其余一起做的动作不用。

### 日常里的小事

八种会讲出来的插曲，填进她的一天。它们都是 `short`，都不让她离线，也都不带图，两种世界都适用（见[下文](#她的世界)）：

| 种类 | 标签 | 类别 | 权重 | 冷却 | 持续 | 门槛 | 提示 |
|---|---|---|---|---|---|---|---|
| `bird_encounter` | 遇到一只鸟 | `short` | 2 | 12 小时 | 30 分钟 | 无 | `（你刚看到一只鸟停在不远处。）` |
| `ex_wants_back` | 前任来求复合 | `short` | 0.5 | 30 天 | 1 小时 | 亲密度档 2 | `（你的前任刚来找你，说想复合。）` |
| `want_to_quit_job` | 想辞职 | `short` | 1 | 7 天 | 1 小时 | `employed` 不为 `"free"` | `（你刚冒出想辞职的念头。）` |
| `pondering_life` | 在思考人生 | `short` | 2 | 48 小时 | 1 小时 | 无 | `（你刚一个人发呆，在想人生的事。）` |
| `streaking` | 刚裸奔了一圈 | `short` | 0.5 | 30 天 | 30 分钟 | 亲密度档 3，且性向相容 | `（你刚一时兴起，脱光了跑了一圈。）` |
| `want_to_see_sea` | 想去看海 | `short` | 2 | 72 小时 | 1 小时 | 无 | `（你现在很想去看海。）` |
| `want_hot_spring` | 想去泡温泉 | `short` | 1 | 72 小时 | 1 小时 | 无 | `（你现在很想去泡温泉。）` |
| `want_sauna` | 想去洗桑拿 | `short` | 1 | 72 小时 | 1 小时 | 无 | `（你现在很想去洗桑拿。）` |

提示只陈述事实，细节由她填：什么鸟、前任说了什么、工作的事她打算怎么办。`high` 的 `outing` 让 `want_to_see_sea` 权重翻倍（见[下文](#1-需求给种类加权)）；
`hygienic` 把 `want_hot_spring` 和 `want_sauna` 和洗澡、泡澡一起缩放（见[角色设置](#角色设置)）。一件事会牵出另一件：前任发来消息或刚分手，前任更可能来求复合；
被上级骂或临时接到工作，她更可能想辞职；分手、吵架或想辞职，会让她想起人生；想人生又会让她想去看海（见[关联系数](#关联系数)）。
住院和术后期间，她不会遇到鸟，也不会裸奔；那几种「想去」和两种心事照样会来。

`streaking` 和 `strip` 一样带成人题材：`sex_addict` 对它的缩放与 `sudden_desire` 相同，只对部分用户提供这类内容的调用方把它排除。

### 迷路、习惯与性

另外六种会讲出来的插曲。它们都是 `short`，都不让她离线，也都不带图，两种世界都适用（见[下文](#她的世界)）：

| 种类 | 标签 | 类别 | 权重 | 冷却 | 持续 | 门槛 | 提示 |
|---|---|---|---|---|---|---|---|
| `got_lost` | 迷路了 | `short` | 3 | 48 小时 | 1 小时 | 有 `short_trip`、`long_trip`、`hometown_visit` 或 `traveling_together` 正在进行 | `（你刚迷了路，找不到要去的地方。）`；`traveling_together` 期间是 `（你们一起出来玩，刚迷了路。）` |
| `drinking` | 在喝酒 | `short` | 2 | 12 小时 | 30 分钟 | `alcoholic` 为 `true` | `（你正在喝酒。）` |
| `smoking` | 在抽烟 | `short` | 2 | 12 小时 | 30 分钟 | `smoking` 为 `true` | `（你正在抽烟。）` |
| `smoking_weed` | 在抽大麻 | `short` | 2 | 12 小时 | 30 分钟 | `drug` 为 `true` | `（你正在抽大麻。）` |
| `just_have_sex` | 刚做完爱 | `short` | 1 | 48 小时 | 30 分钟 | 亲密度档 3 | `（你刚和人做完爱。）` |
| `masturbating` | 自慰中 | `short` | 1 | 24 小时 | 30 分钟 | `sex_addict` 为 `true`，并且亲密度档 3 | `（你正在自慰。）` |

`got_lost` 需要背景（见[上文](#同一时间一件插曲一个背景)）：
在她自己的旅行里，提示像背景里抽到的任何插曲一样，先说旅行的状态（`（你正在短途旅行。你刚迷了路，找不到要去的地方。）`）；
和用户一起旅行时，迷路的是他们两个人。

习惯类事件跟着习惯键走（见[角色设置](#角色设置)）：卡上写了她喝酒、抽烟或抽大麻，她才会抽到对应的那一种；`high` 的
`indulgence` 让这三种各自权重乘 3（见[下文](#1-需求给种类加权)）。咖啡和美食没有对应的事件。

`just_have_sex` 是她生活里的性，事后讲出来：跟谁由她自己说。它只看档位，不查性向是否相容。`having_sex` 是她事后才解释的那段离线，
只对 `sex_addict` 为 `true` 的角色抽取；其他角色只有 `just_have_sex`，从档 3 起。`sex_addict` 让 `just_have_sex` 权重为 3 倍或三分之一，
和对 `sudden_desire` 一样；她还是处女期间，它的权重为五分之一，和 `having_sex` 一样，两种里任何一种的第一条行（或 `having_sex_together` 的第一条行）都会结束她的处女身份
（见[角色设置](#角色设置)）。前任发来消息、突然很想要、脱给对方看，都会让它更容易出现；它又和 `having_sex` 一样让验孕虚惊更容易出现；
突然很想要也会让 `masturbating` 更容易出现（见[关联系数](#关联系数)）。

住院和术后期间，她不喝酒、不抽烟、不抽大麻、不做爱，也不在旅途中，所以不会迷路；自慰照样会有。
和用户一起旅行期间不抽 `just_have_sex`：她和用户在一起。

`just_have_sex` 和 `masturbating` 带成人题材，只对部分用户提供这类内容的调用方把它们排除。

### 没赶上车

| 种类 | 标签 | 类别 | 权重 | 冷却 | 时长 | 门槛 | 提示 |
|---|---|---|---|---|---|---|---|
| `missed_ride` | 没赶上车 | `short` | 3 | 48 小时 | 1 小时 | 有 `business_trip`、`short_trip`、`long_trip`、`hometown_visit` 或 `traveling_together` 正在进行 | `（你刚没赶上车。）`；`traveling_together` 期间为 `（你们一起出来玩，刚没赶上车。）` |

会讲出来，不离线，不带图，两种世界都有（马车也是车）。和 `got_lost` 不同，出差也算。

### 上厕所

| 种类 | 标签 | 类别 | 权重 | 冷却 | 时长 | 门槛 | 提示 |
|---|---|---|---|---|---|---|---|
| `using_toilet` | 上厕所中 | `short` | 1 | 12 小时 | 30 分钟 | 她不是机器（见[角色设置](#角色设置)） | `（你正在上厕所。）` |

会讲出来，不离线，不带图，两种世界都有，没有亲密度门槛。她照常回复。它是室内种类，所以 `outdoorsy` 会缩放它（见[下文](#户外还是室内)）；
没有任何背景会排除它，没有事件让它更容易出现，也没有需求系数作用于它。

### 交通工具

六种 `mid` 种类说她正坐什么走，都只在旅行进行期间抽取：

| 种类 | 标签 | 权重 | 冷却 | 时长 | 世界 | 提示 | `traveling_together` 期间 |
|---|---|---|---|---|---|---|---|
| `on_a_boat` | 在船上 | 1 | 48 小时 | 1–2 小时 | 两者都可 | `（你正在坐船。）` | `（你们一起出来玩，正在坐船。）` |
| `on_a_cruise` | 在游轮上 | 1 | 48 小时 | 2–4 小时 | `modern` | `（你正在游轮上。）` | `（你们一起出来玩，正在游轮上。）` |
| `on_a_bus` | 在坐大巴 | 1 | 48 小时 | 2–4 小时 | `modern` | `（你正在坐大巴。）` | `（你们一起出来玩，正在坐大巴。）` |
| `on_a_train` | 在坐火车 | 1 | 48 小时 | 2–4 小时 | `modern` | `（你正在坐火车。）` | `（你们一起出来玩，正在坐火车。）` |
| `on_a_plane` | 在飞机上 | 1 | 48 小时 | 2–4 小时 | `modern` | `（你正在飞机上。）` | `（你们一起出来玩，正在飞机上。）` |
| `driving` | 在开车 | 1 | 48 小时 | 1–3 小时 | `modern` | `（你正在开车。）` | `（你们一起出来玩，正开车在路上。）` |

任何旅行都算，出差也在内；只有 `on_a_cruise` 例外，它只在 `short_trip`、`long_trip`、`hometown_visit` 或 `traveling_together`
期间出现。她自己的旅行中，提示先说旅行：`（你正在短途旅行。你正在坐火车。）`。六种都会讲出来，不离线，不带图，算户外；
除了要在旅行中，没有别的门槛，各自有自己的冷却，所以同一趟旅行里可以先坐飞机再坐火车。它们是 `mid`，都可以写进 `prefer_events`。

对 `modern` 的角色，它们取代了 `on_the_road`：六种加起来的权重等于 `on_the_road` 在休闲旅行里的权重，出差时是它的六分之五。
`fantasy` 的角色抽 `on_the_road` 和 `on_a_boat`。

### 她的世界

`art_metadata.setting` 说她生活在哪个世界：`modern`（默认，其他值也按它读）或 `fantasy`。不属于她那个世界的种类不会对她抽出，
不管请求的 `kinds` 或她的 `prefer_events` 怎么写：

| 适用 | 种类 |
|---|---|
| 只限 `modern` | `police_encounter`、`showering`、`taking_selfie`、`gaming`、`watching_show`、`business_trip`、`hospitalized`、`post_surgery`、`temp_work`、`scolded_superior`、`on_a_cruise`、`on_a_bus`、`on_a_train`、`on_a_plane`、`driving` |
| 只限 `fantasy` | `on_the_road`、`fighting_together` |
| 两者都可 | 其余所有种类 |

`on_the_road` 是唯一会被抽到的、只限 `fantasy` 的种类：现代角色改坐[交通工具](#交通工具)。现代角色的 `prefer_events` 里写了它，这一项不起作用。
`fighting_together` 从不被抽到；写入它的 `fight_together` 只有 `fantasy` 的角色会答应（见[一起做的动作](#一起淋浴泡澡和蒸桑拿)）。

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
离线类种类或约会进行期间 `/open` 返回 `null`。

### 存储

- `engine.persona_events` 每个事件一行：`instance_id`（随实例级联删除）、`session_id`（在哪个 session 抽到并讲出来；
  session 没了就置为 NULL）、`event_key`（在 session 内唯一）、`kind`、`started_at`、`ends_at`，以及 `message_id`
  （讲述它的那条消息；她不讲的离线类种类为 NULL，消息没了也置为 NULL）。类别和是否背景由种类推出，不存。
  种类是当前构建不认识的行，读取时跳过。
- 消息是 `engine.chat_messages` 的一行，带 `metadata.proactive = 'event'`；事件的种类和时间在 `persona_events` 里。
- 需求存在被评定的用户消息行上，键为 `metadata.needs`，配一个部分索引（`chat_messages_needs_idx`），让读取最新评定
  对每个 session 都是一次索引探测。
- 存量的 `taking_space` 和 `fell_out` 和其他事件一样是 `persona_events` 的一行，`message_id` 为 NULL；写下它们的那次选择在
  `engine.life_choices` 里（见[选择存在哪](#选择存在哪)）。

### 聊天之外

开启 [World Stories](world-system.zh.md#world-stories人生故事) 时，故事导演直接读该实例的事件，每件带标签、相对时间和她讲述时的那条
消息，另外还读在途背景、这一对的关系状态和冷战，以及这对关系里的 `confess`、`partner_up`、`propose`、`break_up` 选择（还有存量的 `ask_for_space`）和结果，
并把它们当作既成事实。一起洗澡、一起淋浴、约会，以及 `having_sex_together`、`streaking_together`、`using_toilet_together` 和 `asleep_together` 不在其中：
和她同睡、洗澡、旅行请求的结果一样，它们只留在对话里。她主动发的消息不进它的聊天证据，所以一件事
不会被读成她对一句不相干的话的回答。World Memories 导演对名单里的每个角色读在途背景，不论开没开 stories，住院中的角色
就不会被写进聚会。两位导演都不写事件，也不改上面的角色设置。需求不进其中任何一个。

## 两者如何互相影响

下面的需求系数都是第二段里的系数；其余几个（[关联系数](#关联系数)、`prefer_events`、`is_virgin`、`homesick`、`sex_addict`、`hygienic`、`employed`）
都不读需求。

### 1. 需求给种类加权

需求系数在抽取的第二段，对一个种类在它所属类别内的权重做缩放。它只决定类别里*哪一种*出现，不决定某个类别出不出现。
系数来自 `crates/eros-engine-core/src/events.rs` 里的 `needs_factor` 和 `indulgence_factor`：

| 需求水平 | 种类 | 系数 |
|---|---|---|
| `emotional` 为 `high` | `ex_message` | ×3 |
| `emotional` 为 `high` | `animal_encounter`、`irritable`、`quarrel` | ×2 |
| `outing` 为 `high` | `want_to_go_out` | ×3 |
| `outing` 为 `high` | `want_to_see_sea` | ×2 |
| `outing` 为 `low` | `want_to_go_home` | ×3 |
| `indulgence` 为 `high` | `want_to_go_out`、`irritable` | ×2 |
| `indulgence` 为 `high` | `drinking`、`smoking`、`smoking_weed` | ×3 |

`indulgence` 那两行是单独的一个系数，和上面各行相乘：`outing` 与 `indulgence` 都为 `high` 时 `want_to_go_out` 为 ×6，
`indulgence` 与 `emotional` 都为 `high` 时 `irritable` 为 ×4。
`social` 不给任何种类加权。只有部署配置了判官、并且角色有一次评定时，系数才生效（评定推到调用时刻）；否则所有系数都是 1。
系数不是门槛：权重 ×3 的种类仍然要先有资格。

### 2. 事件从不改写已存的需求

没有任何代码路径会因为事件去写需求。判官读到 `[近期事件]`，以及 give 带来的 `[用户动作]` 和之后的 `[身体状态]`，
自己在把它们放在眼前的前提下重新评定需求；`examples/model_config.toml` 里的参考判官 prompt 就是这样要求它的：
以 `[需求]` 为起点，结合对话、`[近期事件]`、`[身体状态]` 和经过的时间重判。判官弃权（`needs: null`）时，不管发生了什么，上一次的评定继续往前漂。
唯一不是判官写的，是被接受的 give 对两项需求的挪动（见[give 会改动需求](#give-会改动需求)），它只替换判官对这几项的评定。

### 3. 离线的那一段不写评定

被离线类种类压成沉默的回合不跑判官，所以什么都不评。最后一次评定在这一段里一直往前漂，
这一段之后第一个被判官评过的回合看到的，是把整段时间都推过去的水平：一段很长的沉默之后，她读起来比之前更想找人说话。

合在一起，两者构成一个以判官为中心的回路：想出门的角色更可能抽到 `want_to_go_out`；这件事进入判官的上下文；判官再评一次 `outing`。

## 生活选择

**生活选择**是她向用户提出的一个真实请求，有些回答有真实的代价。它由上面那套机制搭起来：她的请求是一条主动消息，被接受的请求可能写入一个事件，
用户的回答走一个端点。一共八种：

| 种类 | 她问用户 | 规则 |
|---|---|---|
| `confess` | 愿不愿意和她在一起 | [浪漫选择](#浪漫选择) |
| `partner_up` | 愿不愿意和她结成伙伴 | [浪漫选择](#浪漫选择) |
| `propose` | 愿不愿意嫁给她、娶她 | [浪漫选择](#浪漫选择) |
| `break_up` | 分手 | [浪漫选择](#浪漫选择) |
| `sleep_together` | 愿不愿意和她上床 | [浪漫选择](#浪漫选择) |
| `fall_asleep_together` | 愿不愿意躺在一起睡觉，仅此而已 | [浪漫选择](#浪漫选择) |
| `travel_together` | 一起去旅行 | [一起去旅行](#一起去旅行) |
| `bathe_together` | 一起洗澡 | [一起洗澡](#一起洗澡) |

挪动这一对[关系状态](affinity-model.zh.md#关系状态)的，只有前四种。不论处在什么状态，这一对都可能陷入[冷战](#冷战)，冷战不是谁提出来的。

### 她什么时候提出

engine 在每一轮 affinity 评估已经落库的 1:1 文字回合结束时检查（即*被判官评过的回合*：affinity 评估器跑过、verdict 落了库，
不管部署有没有配置 PDE 判官），时间在落库之后。它先让[冷战](#冷战)开始或结束，再按这个顺序检查各种类：`break_up`、`confess`、`partner_up`，
然后是 `propose` 或她的同睡请求，然后 `bathe_together`，最后 `travel_together`。每种都有自己的触发条件，读的是已落库、做过时间衰减的状态；
每种还都要满足：

- **新的连续。** 只有该 session *连续起算点*之后的、被判官评过的回合才计数。起算点取它最新一次选择的 `asked_at` 和 `resolved_at`
  （已存的、被接受的 `ask_for_space` 取它那段分开的结束时刻）以及它最新一行 `relationship` 事件中最晚的那个；状态或冷战每变一次都会写这样一行。
  她等到了回答的那段等待期间发生的、被判官评过的动作回合属于那段经过，不计数；过期的请求以 `asked_at` 为锚，所以它等待期间的动作回合照样计数。
  转换了不理会的请求的那一轮，是新连续的第一轮。提出一次就用掉这段连续：下一次要新的被判官评过的回合。
- **跑道清空。** 角色的所有 session 都没有待回答的选择，角色也没有正在进行的[约会](activities.zh.md)和存量的分开（`taking_space`）。她只有一段人生，所以同一时间最多只有一个请求待回答：
  一种请求提出之后，同一轮后面的检查都会发现跑道已经占住。约会自己的回合从来不是被判官评过的回合，所以从不触发选择。
- **合适的 session。** 角色是 genome 角色的 1:1 文字 session。语音 session 不会触发、送达或结算选择，群聊根本不写好感度，
  BYOC session 从不触发，因为结算端点不服务 BYOC。

触发是 engine 对已落库状态做的数学，没有判官参与。

### 她的请求与送达

触发之后，依次发生：

1. **她的话。** companion 对话模型走和事件消息相同的主动消息路径生成这个请求（角色设定、历史窗口、记忆召回、`[now]`；
   没有判官，不流式，没有用户消息行），用 engine 默认的 scope。舞台提示是最后一条 `user` 消息，不落库；每种的舞台提示见
   [model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)。这条消息是一行普通的 assistant 消息，
   `assistant_action_type = 'proactive'`、`metadata.proactive = 'life_choice'`，在调用已读接口之前一直是未读，不跑 post-process。
2. **选择这一行。** `engine.life_choices` 里的一行，和她的消息在同一个事务里写入：种类、状态 `pending`，`asked_at` 就是这条消息的 `sent_at`。
   待回答的窗口是 24 小时；过期时刻是 `asked_at` 加 24 小时，推算出来，不存。角色已经有一个待回答的选择，或者连续起算点在触发读取之后挪动了，
   这次写入就让步。

回复为空、被规则剥到一无所剩，或生成失败，什么都不写：没有消息，没有选择行，之后符合条件的回合还可以再问。这个检查排在这一轮
post-process 的最后，在好感度落库和 feeling 摘要之后，所以它的生成不会拖慢这两件事，这一轮自己的好感度写入（状态加事件行，一个事务）
也不受影响。这一对记录下来的 scope 只决定有没有资格，别的什么都不决定；她在结算时的回复，用结算请求自己带的 scope。

选择待回答且未过期期间，events 路由对这个角色什么都不抽。她提出时已经在进行的插曲照样跑到回答出来为止；写入事件的接受，以及被接受的
`confess`、`partner_up` 或 `propose`，会把它截断。

**送达。** 通知搭在逐轮的好感度 delta 查询上，即
[`GET /bff/v1/comp/affinity/{session_id}/event`](api-reference.zh.md#get-bffv1compaffinitysession_idevent)。
它的 `event` 对象多一个可选字段 `life_choice`（`id`、`kind`、`options`、`asked_at`、`expires_at`、`message_id`），
在响应时按该 session 待回答、未过期的选择现场拼出，没有时不带。请求落在触发那一轮的事件落库之后几秒，所以这一轮自己放行的那次查询
通常赶不上它：适配过的客户端在历史里某一行带 `occasion: "life_choice"` 时，应当重新拉一次 delta。两个历史路由
（[canonical](api-reference.zh.md#get-compchatsession_idhistorylimit20offset0) 与
[BFF](api-reference.zh.md#get-bffv1compchatsession_idhistorylimit50offset0)）都把主动 assistant 行的 `metadata.proactive`
作为可选的 `occasion` 给出，其他行省略。她那条请求所在的行，在两个历史路由和 BFF start 打包里还带一个 `life_choice` 回执
（`id`、`kind`、`status`、`resolved_at`），重新加载也能看出这次选择怎么收场。
回答带来的降档和状态挪动都不会从这条路由返回，客户端不会遇到不认识的 `event_type`；新的状态在结算响应里带回，别的设备从绝对值路由读。

**降级本身就是设计。** 没有适配的下游会忽略这个不认识的字段，把请求当成一条普通的主动消息渲染，用户用文字回复。
这就是下面的"不理会"那条路。什么都不会坏；选择的界面完全是增量。

### 用户的回答

回答是 `POST /v2/comp/session/{session_id}/life-choices/{choice_id}`，带 `{"response": "accept" | "decline"}`
（[接口参考](api-reference.zh.md#post-v2compsessionsession_idlife-choiceschoice_id)），也可以是用户自己的话，或者沉默：

| 路径 | 怎么发生 | 选择的状态 | 代价 |
|---|---|---|---|
| 接受 | 端点，`accept` | `accepted` | 无；开始什么、挪动什么，看种类 |
| 拒绝 | 端点，`decline` | `declined` | 有些种类降档 |
| 不理会 | 待回答期间用户的第一条普通消息 | `ignored` | 无，也没有事件 |
| 过期 | 24 小时内既没回答也没发消息 | 仍为 `pending`，读作已过期 | 无 |

**接受与拒绝。** 一个事务回答这次选择并付清代价：接受写入的事件、挪动的状态、拒绝降的线。各种类做什么，见各自的小节。
然后她用一条主动消息回应，`metadata.proactive = 'life_choice'`，用请求自己的 scope 生成并返回；回复生成失败或为空，回答和它做的一切照样保留。
降档产生的好感度事件和它的 context 键见 [affinity-model.zh.md](affinity-model.zh.md#生活选择的降档与冷战减半)。

**状态变了。** 动作轮次不会结算她待回答的请求，所以请求等着的时候，关系状态可能已经变了：她的 `propose` 还在等，用户自己的 `break_up` 被接受了。
`confess`、`partner_up`、`propose` 或 `break_up` 的回答到达时，如果状态已经不是这种请求会从中提出的状态，就返回 `409`，什么都不写；
请求自己过期。只看状态：冷战里提出的 `break_up`，冷战结束之后照样可以回答。

**不理会。** 选择待回答期间到达的第一条普通用户文字消息就是回答：状态变为 `ignored`，不论哪种，都没有代价，也没有事件。
不改任何档位；判官像读任何一轮那样读用户写的话，自己给这一轮打分。时钟看的是用户那一行的 `sent_at`：
只有在她提出之后发出的消息才会触发（提出时已经在路上的那条不算回答），排队或被重新驱动的回合读到的结果相同。
**动作轮次**（give、打赏、亲吻）不算回答：它按自己的规则回复，请求仍然待回答，因为她等着的时候收到礼物，是在示好，不是在回答。
`/open` 开场白和 reaction 都不会让它结算。

**过期。** 24 小时内既没有回答、也没有消息，选择就此失效。它在表里的状态仍是 `pending`，但处处读作已过期：delta 上的字段不再附带，
端点返回 `409`，之后的消息什么都不转换。没有任何代价；之后再提出，需要重新满足它完整的触发条件。

结算和一条进来的用户消息按事务竞争。先写入的赢，输的一方什么都不写（端点返回 `409`）。

端点可以改用 server-sent event 流回答：先发一帧 `resolved`，带已提交的回答和新的好感度，再边生成边给她的回话。见
[API 参考](api-reference.zh.md#openeventslife-choices-的流式响应)。

### 冷战

用户把她的耐心磨光了，这一对就陷入**冷战**。冷战是 session 好感度行上的一个标记，叠在这一对的任何状态之上；冷战结束，底下的状态原样露出来
（见 [affinity-model.zh.md](affinity-model.zh.md#冷战)）。冷战不是谁提出来的。

**什么时候开始。** 每个被判官评过的 1:1 文字回合之后、检查她的请求之前，engine 读起算点之后各个被判官评过的回合里判官给的耐心档
（`patience_grade`，1 到 3，读自每个回合的 `state_after`），从新到旧。最新三个全是最低档 1，或最新七个里至少五个是，冷战就开始。
分手被拒绝时也开始，不论是她提的还是用户提的，已经在冷战时除外。不读档位差距、相容与否或关系状态。

读的是判官自己的档，不是推算出的 `patience`：后者是档位乘上 bond 线的加成，bond 低时就算最高档也到不了 0.5，拿它设门槛，可能把一段新关系永远困在冷战里。
`state_after` 里没有这个档的回合，读作中间档 2。

**什么时候结束。** 起算点之后最新三个被判官评过的回合，耐心档全都高于最低档，或者分手被接受。冷战的每一次开始和结束都写一行 `relationship` 事件，
它会挪动连续起算点：让冷战开始的那几轮不算进让它结束的连续里，冷战结束也让每一种请求重新开始计连续。

**她会说出来。** 由连续触发的冷战开始时，她通过和请求相同的路径发一条主动消息，不带选择行：`metadata.proactive = 'cold_war'`
（历史路由里的 `occasion`），舞台提示是

```
（你对对方的耐心被磨光了。你决定不再给对方好脸色，告诉对方你现在不想好好说话。）
```

标记先落库：消息生成失败或为空，冷战照样开着，她什么都不说。分手被拒绝而开始的冷战没有单独的消息，她的拒绝就是那一轮的回复；冷战结束也不写消息。

**冷战期间。**

- 她照常回复，只是冷淡：回复的 `[relationship]` 段落最后一行是 `你们正在冷战`，判官读到 `冷战中=是`。
- 四个线轴上每一个为正的已提交增量一律减半，这个 session 每个被判官评过的回合和约会结算都是如此
  （见 [affinity-model.zh.md](affinity-model.zh.md#生活选择的降档与冷战减半)）。冷战开始时什么都不降。
- 她不提任何暖向的请求：没有 `confess`、`partner_up`、`propose`、`sleep_together`、`fall_asleep_together`、`bathe_together` 或 `travel_together`。
- 处在可分手的状态时，她可以提出分手（见[下文](#浪漫选择)）。

待回答的选择或进行中的约会，既挡不住冷战开始，也挡不住它结束。只有 genome 角色的 1:1 文字 session 才有冷战；BYOC session 始终是 `none`，没有冷战。

### 浪漫选择

有六种选择跟着这段关系本身走。四种挪动关系状态：`confess`，她告白，问用户愿不愿意和她在一起；`partner_up`，她提出和用户结成伙伴；`propose`；以及 `break_up`。
另外两种是她想和用户过夜的请求：`sleep_together`，指上床；`fall_asleep_together`，只是躺在一起睡觉。她问哪一种，由这一对的关系状态决定。

**谁有资格。** `confess`、`propose`、`sleep_together` 和 `fall_asleep_together` 只问调用方通过它发来的 `affinity_scope` 标明为浪漫上相容的一对
（判据见[接口参考](api-reference.zh.md#post-v2compsessionsession_idlife-choiceschoice_id)）。不满足这条判据、或没有记录到 scope 的一对，永远不会被问到这几种。
`partner_up` 和 `break_up` 不读 scope：chemistry 领先的不相容一对什么都不会被问到，等 bond 追上来，她会提出结成伙伴。已经到达的状态，不会因为 scope 变了而撤回。
engine 不接收性别或性取向，也不判断谁会被谁吸引。

**从哪些状态提出。**

| 种类 | 从这些状态提出 |
|---|---|
| `confess` | `none`、`ex`、`partner`、`peer` |
| `partner_up` | `none`、`ex`、`peer` |
| `propose` | `dating`、`fwb`、`situationship` |
| `break_up` | 冷战期间的可分手状态（`dating`、`partner`、`fwb`、`situationship`、`engaged`） |
| `sleep_together` | `dating`、`engaged`、`fwb`、`situationship`，以及 chemistry 在第 4 档或以上的 `partner` |
| `fall_asleep_together` | `none`、`peer`、`ex`，以及 chemistry 在第 3 档的 `partner` |

所以 `dating`、`engaged`、`fwb` 和 `situationship` 即使之后 bond 超过了 chemistry，也不会被提出结成伙伴；`engaged` 既不会被告白，也不会被求婚。

**她什么时候提出。** 检查和每种选择在同一个位置，读同样已落库、做过时间衰减的状态，跑道和 session 的条件也相同；冷战期间暖向的几种都不检查。
*领先*的线是档位较高的那条；档位相同时看分数，分数完全相同算 bond 领先。一个被判官评过的回合，在推算出的 `warmth` 和 `patience` 都高于 0.7 时算*温暖*，
值读自该回合的 `state_after`；0.7 是 engine 自己的规则阈值，不是判官的档。

| 种类 | 档位 | 被判官评过的回合 | 等待 |
|---|---|---|---|
| `break_up` | 无 | 最新 3 个全在判官的最低耐心档 | 除新的连续外没有 |
| `confess` | chemistry 领先，在第 3 档或以上 | 最新 3 个的 `warmth` 都高于 0.7 | 从该 session 最近一次状态变化起 14 天，以及从 `confess` 上一次任何形式的回答起 14 天 |
| `partner_up` | bond 领先，在第 3 档或以上 | 最新 3 个的 `patience` 都高于 0.7 | 从该 session 最近一次状态变化起 14 天，以及从 `partner_up` 上一次任何形式的回答起 14 天 |
| `propose` | chemistry 和 bond 都在第 4 档或以上 | 最新 8 个里至少 5 个温暖 | 被接受或不理会的 `propose` 之后 14 天；被拒绝或过期之后，等新的连续 |
| `sleep_together`、`fall_asleep_together` | chemistry 在第 3 档或以上 | 最新 3 个全都温暖 | 有了结果就不再提（见下） |

`confess` 只读 warmth，`partner_up` 只读 patience：warmth 受 chemistry 加成，patience 受 bond 加成，另一条线低于约 0.415 时温暖的回合根本出不来，
明显一头热的一对就永远不会被问到。*状态变化*指状态真的挪动了的 `relationship` 事件；冷战开始或结束不算。任何形式的回答指接受、拒绝或不理会；过期不算。
让冷战开始的那一轮会挪动连续起算点，所以 `break_up` 要在它之后再有三个最低档的回合。`propose` 与她的同睡请求同一轮都满足时，触发 `propose`。

四种挪动状态的请求，只要状态、等待和连续允许，就可以再提，所以分手之后这一对可以再走一遍。她的同睡请求每种各自只提一次：
在一个 session 里被用户接受或不理会过的那一种，在那里不会再触发；她在这个 session 或它的约会里答应过用户反过来提的同一件事之后，也不会再触发：
`sex_together` 对应 `sleep_together`（见[一起做的动作](#一起淋浴泡澡和蒸桑拿)），`fall_asleep_together` 对应 `fall_asleep_together`。
被拒的不算；她已经提出、还在等回答的请求保持原样，照常被回答或过期。问过一起睡的一对，等状态够了，照样可能被问上床。

**用户的 `propose` 和 `break_up`。** 两者都不带字段，也不是一起做的动作：不写事件，本身也不是出图回合。在判官之前，状态不是 `dating`、`fwb`
或 `situationship` 时用户的 `propose` 被拒，状态不可分手时用户的 `break_up` 被拒；这样的拒绝什么都不发生，判官会被告知结果。
其余由判官决定：`propose` 被接受，这一对成为 `engaged`；`break_up` 被接受，这一对成为 `ex`、冷战结束，被拒绝则状态不变、没有冷战时开始冷战。
被拒都不降档。状态在保存回答的那个事务里挪动，落在这一轮的 session 上，约会里则落在发出邀约的那个 session 上。BYOC 实例上什么状态都不挪动：
用户的 `propose` 在任何状态下都交给判官，什么都不挪动；`break_up` 一律被拒。用户可以再提，每次都由判官决定。
部署的 [`tones.emotional`](model-config.zh.md#tones--聊天语气仅限-chat-任务) 大于 0 时，她对这两者的回复都用 `emotional` 语气，接受、拒绝都一样。

**她的请求。** 送达方式和每种请求一样（见[上文](#她的请求与送达)），没有档位差距。舞台提示见
[model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器)。

**回答。** 同一个端点，同样的 `409` 规则，状态变了时同样返回 `409`（见[上文](#用户的回答)），同样的响应形状。

| 种类 | 接受 | 拒绝 |
|---|---|---|
| `confess` | `none` 或 `ex` 变为 `dating`，`partner` 变为 `fwb`，`peer` 变为 `situationship`；正在进行的插曲结束 | 状态不变；chemistry 降一档 |
| `partner_up` | 这一对变为 `partner`；正在进行的插曲结束 | 这一对变为 `peer`；什么都不降 |
| `propose` | 这一对变为 `engaged`；正在进行的插曲结束 | 状态不变；bond 和 chemistry 各降一档 |
| `break_up` | 这一对变为 `ex`，冷战结束 | 状态不变，冷战继续（已经结束的话重新开始）；什么都不降 |
| `sleep_together` | 从此刻开始一小时的 `having_sex_together`，作为 `event` 返回，和用户的 `sex_together` 被接受时一样；正在进行的插曲结束 | chemistry 降一档 |
| `fall_asleep_together` | 从此刻开始一到四小时的 `asleep_together`，作为 `event` 返回，和用户的 `fall_asleep_together` 被接受时一样；她照常回复，正在进行的插曲结束 | chemistry 降一档 |

接受没有代价，背景照旧；只有 `sleep_together` 和 `fall_asleep_together` 返回 `event`。不理会和过期不论哪种都没有代价。
状态挪动和一行 `relationship` 事件在同一个事务里写入（见 [affinity-model.zh.md](affinity-model.zh.md#关系状态)）。
拒绝的降档，是被拒动作的落点规则作用在被点名的那几条线上，与结算在同一个事务里，这几条线的 `pending_deltas` 也清零。动作轮次不会结算它。

故事导演把这四种挪动状态的选择连同结果、以及这一对的关系状态和冷战，当作引擎记录的事实来读（见[聊天之外](#聊天之外)）；她同睡请求的结果只留在对话里。

### 一起去旅行

`travel_together`：她提议两个人一起去旅行。它和浪漫选择一样是暖向的，但不读这一对的 scope，所以浪漫选择碰不到的一对也可能被问到。

**她什么时候提出。** 在每个被判官评过的 1:1 文字回合之后、所有检查的最后进行，排在浪漫选择和一起洗澡之后，读的是同样已落库、做过时间衰减的状态，
跑道和 session 条件也一样，冷战期间不提。它要 bond 在第 3 档或以上、最新 3 个被判官评过的回合全都温暖，并且没有背景事件在进行。
被接受或不理会过的 `travel_together`，从结算时起 14 天内不再提出；被拒绝或过期后，等这个 session 有了新的连续就可以再提。
同一轮前面的种类也满足时，它们先走，待回答的请求会占住跑道。

**她的请求。** 交付方式和每种请求一样：先是她的话，再在旁边写入选择这一行，没有档位差距，窗口同样是 24 小时，affinity delta 上同样带 `life_choice` 字段。

**回答。** 同一个端点，同样的 `409` 规则，同样的响应形状。

| 路径 | 选择状态 | 代价 | 发生什么 | 她说的话 |
|---|---|---|---|---|
| 接受 | `accepted` | 无 | `traveling_together` 事件从此刻开始，作为 `event` 返回 | 生成的回应，知道这趟要玩多久 |
| 拒绝 | `declined` | 无 | 无 | 生成的、有点失落的回应 |
| 不理会 | `ignored` | 无 | 无 | 没有单独的话：这一轮照常运行 |
| 过期 | 仍为 `pending`，读作已过期 | 无 | 无 | 无 |

**这趟旅行。** `traveling_together`（和对方一起旅行）是 `trip` 类、不可抽取的背景，持续 48 到 72 小时，在用户接受时按整小时均匀抽取。
开始时结束正在进行的插曲。它不作为事件讲述，因为她对“好”的回应已经说了；全程她照常回复，不算离线。`正在经历` 和判官像读其他背景一样读它。
期间不抽第二个背景，也不抽任何可能让她离线的种类；`on_the_road`、`admiring_scenery` 和 `taking_selfie` 像跟随她自己的旅行一样跟随它，
[交通工具](#交通工具)出现时提示说她和对方一起在路上。和所有事件一样，它以角色实例为单位。

**旅行中的其他请求。** 被接受的旅行以结算时刻为起算点，所以旅途中她也可能提出别的请求。

### 一起洗澡

`bathe_together`：她问用户愿不愿意和她一起洗澡。和浪漫选择一样，她只问浪漫上相容的一对，用文字打出的回答不带代价地结算它，只有拒绝才有代价。

**她什么时候提出。** 在每个被判官评过的 1:1 文字回合之后、浪漫选择的检查之后、旅行的检查之前进行，读的是同样已落库、做过时间衰减的状态，跑道和 session 条件也一样，冷战期间不提。下面几条都要满足：

- 这一对在浪漫上相容，判法与浪漫选择相同；
- `hygienic` 为 `true` 的角色 chemistry 在第 2 档或以上，不写这个键的角色在第 3 档或以上；`hygienic` 为 `false` 的角色从不提出；
- 最新的 7 个被判官评过的回合里至少 3 个 `warmth` 达到 0.5，值读自该回合的 `state_after`。不读 patience；0.5 是 engine 自己的规则阈值，不是判官的档。和所有选择一样，只有该 session 连续起算点之后的回合才计数。

被接受或不理会过的 `bathe_together`，从结算时起 7 天内不再提出；被拒绝或过期后，下一个满足条件的回合就会再提。

它有自己的触发条件，不是通往上床的一步：有的人很容易一起上床，却不愿意一起洗澡；一对恋人也可以各洗各的，再睡在一起。
两种请求谁也不等谁结算，进行中的旅行也不挡它。她同一时间只有一个请求待回答，所以同一轮浪漫选择也满足时，浪漫选择先走，它待回答的请求会占住跑道。

**她的请求。** 交付方式和每种请求一样：先是她的话，再在旁边写入选择这一行，没有档位差距，窗口同样是 24 小时，affinity delta 上同样带 `life_choice` 字段。
请求用 engine 的默认 scope 生成，提示是
`（你们最近越来越亲密，你很想和对方一起洗个澡。你决定主动开口，问对方愿不愿意和你一起洗，希望对方给你一个答复。）`；
结算时她的回应用结算请求自己的 scope。

**回答。** 同一个端点，同样的 `409` 规则，同样的响应形状。

| 路径 | 选择状态 | 代价 | 发生什么 | 她说的话 |
|---|---|---|---|---|
| 接受 | `accepted` | 无 | `showering_together` 或 `bathing_together` 事件从此刻开始，两者机会均等，作为 `event` 返回 | 淋浴按 `（对方答应了和你一起洗澡，你们现在一起站在花洒下。）`、泡澡按 `（对方答应了和你一起洗澡，你们现在一起泡在浴缸里。）` 生成的回应 |
| 拒绝 | `declined` | chemistry 降一档，和 `sleep_together` 一样 | 无 | 按 `（对方没有答应和你一起洗澡，你有点失落。）` 生成的回应 |
| 不理会 | `ignored` | 无 | 无 | 没有单独的话：这一轮照常运行 |
| 过期 | 仍为 `pending`，读作已过期 | 无 | 无 | 无 |

**这次洗澡。** 接受后是淋浴还是泡澡，机会均等：`showering_together`（和对方一起淋浴）或 `bathing_together`（和对方一起洗澡），也就是用户的 `shower_together` 和 `bath_together` 写入的那两种。两者都是 `short` 类、不可抽取的插曲，从用户接受时起持续 30 分钟。
开始时结束正在进行的插曲，在途的背景照常继续。它不作为事件讲述，因为她对“好”的回应已经说了；她和用户在一起，不算离线。
期间它就是正在进行的插曲，所以不抽别的插曲；`正在经历` 和判官像读其他插曲一样读它。和所有事件一样，它以角色实例为单位。接受时不带图。

一起洗澡不进她的世界故事：故事导演读的事件里没有任何 `showering_together` 或 `bathing_together`，所以和她的同睡请求一样，它的结果只留在对话里。

### 选择存在哪

`engine.life_choices` 每次选择一行：`session_id`（非空，随 session 级联删除）、`kind`（`confess`、`partner_up`、`propose`、`break_up`、`sleep_together`、
`fall_asleep_together`、`travel_together` 或 `bathe_together`，存量行里还有 `ask_for_space`）、
`status`（`pending`、`accepted`、`declined`、`ignored`）、`tier_gap`（只有存量的 `ask_for_space` 行有值）、
`asked_at`、`resolved_at`、`message_id`（那条请求；消息没了就置为 NULL）和
`event_id`（接受时写下的 `traveling_together`、`showering_together`、`bathing_together`、`having_sex_together` 或 `asleep_together` 行，其他路径为 NULL，
挪动状态的种类被接受时也是；事件没了就置为 NULL）。过期时刻是推算出来的，不存。关系状态和冷战不存在这里：它们在 session 的好感度行上
（见 [affinity-model.zh.md](affinity-model.zh.md#关系状态的两列)）。

**提出分开的存量数据。** 她提出分开的请求（`ask_for_space`）已经退役：engine 不再写这种选择，也不再写 `taking_space` 或 `fell_out` 事件，但之前的行照样可读。
存量的 `ask_for_space` 带着结果出现在历史回执和故事导演读的选择里；还在待回答的会自己过期，或被一条打字的回复不带代价地转成不理会，在端点上回答它返回 `409`。
还在进行的 `taking_space`（说好暂时分开）和任何离线类一样让她离线到结束，之后在那次选择所在的 session 里，按
[model-config.zh.md](model-config.zh.md#taskspde_decision--opt-in-llm-pde-判断器) 里的两种说法标出她的归来；`fell_out`（为分开的事闹得不愉快）读作一件已结束的插曲。
这两种都不能被抽到，也不能写进 `kinds`。

## 没有 LLM 判官时

| | 没有 `[tasks.pde_decision].filter_prompt` |
|---|---|
| 需求 | 没有：没有评定，没有各处的行，`needs: null`，抽取里没有需求系数 |
| 抽取 | 照常，只是没有需求系数 |
| 事件消息 | 照常生成并讲出来 |
| `正在经历` 与 `刚才` | 照常进入回复、`/open` 开场白和事件消息 |
| `[近期事件]` | 不渲染：没有判官来读它 |
| 离线类种类 | 和有判官时一样让轮次沉默，因为这个检查在判官之前：ghosting 开关以及动作、强制出图两种例外照样适用 |
| 生活选择与冷战 | 不变：触发读的是 affinity 评估器评过的回合和其中的耐心档，不是 PDE 判官。用户的 `propose` 和 `break_up` 照样过状态门槛，过了之后规则引擎按接受处理 |
| `smoking`、`alcoholic`、`drug`、`coffee`、`gluttony` | 为 `false` 的键在 give 时照样掷（算作 give 的一起做的动作也一样），可能把它变成拒收；`smoking`、`alcoholic`、`drug` 照样是习惯类事件的门槛；没有 `indulgence` 可漂移、可点名、可加权 |

## 代码指引

- `crates/eros-engine-core/src/needs.rs`：水平、漂移常量、`Habits`、`carried`、`after_give`（give 的挪动）。
- `crates/eros-engine-core/src/pde.rs`：`refuses_give` 与 `UNUSED_SUBSTANCE_ACCEPT`，即 give 时掷的那一次。
- `crates/eros-engine-core/src/events.rs`：类别和种类、每个种类的规则、门槛、需求与角色系数、`is_offline` 与 `is_told`、`draw`、`lookback`。
- `crates/eros-engine-server/src/routes/session_events.rs`：events 路由、抽取的输入、离线类的两道门槛。
- `crates/eros-engine-server/src/routes/life_choices.rs`：结算端点。
- `crates/eros-engine-server/src/routes/bff/affinity.rs`：delta 上的 `life_choice` 字段。
- `crates/eros-engine-server/src/pipeline/proactive.rs`：事件消息和选择相关的消息、它们的舞台提示和写入（`kind_cue`、`find_cue`）。
- `crates/eros-engine-server/src/pipeline/life_choice.rs`：冷战、各种触发与她的请求、不理会的转换、冷战的增长缩放。
- `crates/eros-engine-core/src/relationship.rs`：关系状态、`settle`、`leading_line`、冷战的连续规则与 `sleep_ask_is_sex`。
- `crates/eros-engine-core/src/scope.rs`：`romance_compatible`，浪漫选择读的那条判据。
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`：需求的读取与写入（被接受的 give 的挪动走 `after_give`）、`[需求]` 与 `[近期事件]` 两行
  （`build_pde_ctx`）、判官之前的 offline 分支、掷出拒收后的改判（`apply_give_refusal`）、`body_state_frame`。
- `crates/eros-engine-server/src/prompt.rs`：`needs_line`、`events_judge_text`、`events_reply_text`、
  `just_now_text`、`persona_traits`、`persona_habits`、`[character_state]` 里的各行，以及 `[now]` 里的事件行。
- `crates/eros-engine-server/src/routes/session_effects.rs`：`GET …/effects`。
- `crates/eros-engine-server/src/pipeline/voice.rs`：bootstrap 里的需求。
- `crates/eros-engine-store/src/events.rs::EventRepo`：`insert`、`insert_preempting_in_tx`、`by_key`、`recent`、`current`、`backgrounds_at`、`unexplained_offline`、`unreturned_separation`、`unresolved_scare`。
- `crates/eros-engine-store/src/story.rs::StoryRepo`：`life_events`、`life_choices`，以及不含 proactive 行的 `chat_evidence`。
- `crates/eros-engine-server/src/pipeline/story.rs`：`load_evidence` 与故事 payload 的 `life_events`、`current_background`、`life_choices`；`pipeline/world.rs::direct_world`：世界导演的 `current_background`；`prompt.rs::event_when_text`：两者共用的相对时间。
- `crates/eros-engine-store/src/life_choices.rs::LifeChoiceRepo`：选择行、`pending`、`resolve_in_tx`、`settled`、`settled_since`、`answered_since`、`streak_anchor`。
- `crates/eros-engine-store/src/affinity.rs::AffinityRepo`：`persist_life_choice_drop_in_tx`、`last_judged_endpoints`、`latest_status_change`、`write_relationship_in_tx`、`set_cold_war`。
- `crates/eros-engine-store/src/chat.rs::ChatRepo`：`set_needs` 与 `latest_needs`；`latest_scopes`，浪漫选择据以判断一对关系的 scope。
- `crates/eros-engine-store/migrations/0000_baseline.sql`：事件表与选择表、needs 索引，以及放宽后的 affinity `event_type` 检查（由 baseline 之前的迁移链折叠而来）。
