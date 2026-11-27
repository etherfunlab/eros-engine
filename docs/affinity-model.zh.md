# 好感度模型

[English](affinity-model.md) · [中文](affinity-model.zh.md) · [日本語](affinity-model.ja.md)

## 概述

好感度是每个关系（即每个 session）一份的六轴向量，每轴限定在 `[0, 1]`，每次更新都
clamp。它只在文本通道、非 `product_qa` 的聊天回合上移动。语音回合与 `product_qa`
回合从不写好感度事件，群聊回合也不写（群只读取各成员的好感度，从不写入）。不属于回合的变动有两种：
被回答的生活选择带来的降档（见[生活选择的降档与冷战减半](#生活选择的降档与冷战减半)），以及一场约会结算时的那一次结论（见[约会](#约会)）。判官（LLM 评估器）在任何地方都只输出 ordinal 的档位或等级；
引擎拥有每一个数字、每一个标签、每一次档位迁移，是唯一权威来源。

六轴之上，每个 session 还存一个[关系状态](#关系状态)：这一对说定了是什么关系，只由生活选择和用户的两种动作挪动，上面还可以叠一层冷战。

六轴分三组，全文都围绕这三组组织：**基础对**（`warmth`、`patience`）、**Bond**
友情线（`trust`、`intrigue`）、**Chemistry** 浪漫线（`intimacy`、`tension`）。

## 三组轴

### 基础对：warmth 与 patience

这两个轴不是累积状态。每个判定回合，评估器对每个端点报一个绝对**档**：`1` 冷淡/
不耐烦，`2` 常态（压倒性常见的裁定），`3` 明显热络/上心。引擎据此派生连续值：

```
base(level)  = (level − 1) / 3                          ∈ {0, 1/3, 2/3}
B(x)         = 1 + λ·(x − 0.35)                         λ = (1.5−1)/(1−0.35) = 10/13
decay(Δt)    = max(FLOOR, 1 − RATE·days)                Δt since updated_at

warmth   = clamp01( max(base(w_level)·B(chemistry), φ·chemistry) × decay )
patience = clamp01( max(base(p_level)·B(bond),      φ·bond)      × decay )
```

**耦合的方向是加成**。对线越深，同一个判官档折出的连续值越高：Chemistry 越深，
表达越温暖；Bond 越深，耐心越足。低 Bond × 高 Chemistry 天然产出傲娇形态（没耐心
但热络）；高 Bond × 低 Chemistry 产出老友形态（有耐心但冷静）。不需要提示词特判，
两种形态都是同一条公式的自然结果。

新会话默认值：两个端点都 ≈ `0.244`（档 2 的受抑基础值）。陌生人一开局耐心有限，
这是刻意设计。

基础对是用户逐回合能直接感受到的东西，直接喂给：

- `[relationship]` 提示词段落，在这一对没有关系状态、没有组合关系、也不在冷战时（见 [`[relationship]`](#relationship)）：
  `warmth` × `patience` 四象限，各以 `0.5` 为界。四格里的名词是真实的提示词字符串，逐字引用：

  | | patience ≥ 0.5 | patience < 0.5 |
  |---|---|---|
  | **warmth ≥ 0.5** | 好朋友 | 快被磨光耐心的朋友 |
  | **warmth < 0.5** | 没什么交情的人 | 死对头 |

  独立于 Bond/Chemistry 的档位；任一轴超出本次请求的 scope，或还没有好感度行
  时，这一段省略。
- `[mood]` 的冷淡门槛（`warmth ≤ 0.2` 触发冷淡语气指令，`patience < 0.35` 触发
  不耐烦指令）与逐回合骰子否决（同一套门槛）。
- PDE 判官看到的 patience 分带：低 `[0, 0.35)`、中 `[0.35, 0.65)`、高
  `[0.65, 1]`。

代码里这两个轴叫**派生端点**；存储的 `warmth`/`patience` 列是派生结果的物化缓存，
在时间衰减运行的地方同步刷新。每行的权威事实是四条线轴 + 两个判官**档**
（`warmth_grade` / `patience_grade`，`1..=3`）+ `updated_at`。

### Bond 友情线：trust 与 intrigue

`trust`：话题深度、自我袒露意愿。`intrigue`：好奇心、追问、反 ghost 驱动。

```
bond = (trust + intrigue) / 2    ∈ [0, 1]
```

友情就是信任加持续的兴趣。五档，标签（序列化 snake_case）：`acquaintance`、
`friend`、`close_friend`、`confidant`、`soulmate`。

### Chemistry 浪漫线：intimacy 与 tension

`intimacy`：内梗、昵称、呼应早前细节。`tension`：推拉、俏皮摩擦、傲娇余地。

```
chemistry = (intimacy + tension) / 2    ∈ [0, 1]
```

浪漫就是亲近加张力。标签：`spark`、`flirtation`、`crush`、`lover`、`beloved`。

两条线都不含基础对。构造上基础对就被排除在两线之外，线只作基础对的输入，方向
从不倒转。4.0 起两线不共享任何一个轴。

新会话四条线轴全部为 `0` → `bond = chemistry = 0`，两线都在档 1。没有单独的
「陌生人」状态：档 1 就读作 `acquaintance` + `spark`。

## 数值如何变化

### 判官协议：完全 ordinal

**判官在任何地方都不输出连续数值。** 四条线轴（`trust` / `intrigue` /
`intimacy` / `tension`）各报一个整数**档位** `0`–`4` 加**方向**；两个端点各报
一个绝对**档** `1`–`3`：

```json
{
  "warmth":   2,
  "trust":    {"grade": 1, "direction": "up"},
  "intrigue": {"grade": 0, "direction": "up"},
  "intimacy": {"grade": 0, "direction": "up"},
  "tension":  {"grade": 2, "direction": "down"},
  "patience": 2,
  "reason": "…"
}
```

线轴档位口径：`0` = 无事发生（压倒性常见的裁定）；`1` = 微小但真实的波动；
`2` = 明确的推进或伤害；`3` = 罕见的重要时刻；`4` = 里程碑（极罕见）。负面时刻
被提示要果断给出。端点档是**本轮的状态判读**，与增量无关。

模型做 ordinal 评级可靠，做校准算术不可靠，所以判官只选桶，数字全归引擎。
用户看到的连续 `warmth`/`patience` 分布由引擎的派生数学从离散档折出来（4.0 移除
了判官最后一处连续输出，即旧的 0.1 步进 patience 读数，它在生产环境挤天花板）。

**畸形裁定整份拒收。** 无法解析的 JSON、任一畸形轴（非整数或越界档位、未知
方向）、任一畸形端点档，都会让 `parse_affinity_eval` 拒掉整份裁定：档位全零、
无端点读数、reason 置空；本轮的规则增量照常持久化，判官失败不会丢事件。缺省或
`null` 轴读作档 `0`；缺省或 `null` 端点档读作「保持存储档位」；带引号的整数
（如 `"grade": "2"`）可以救回。

**分带输入，端点除外。** 每轮 payload 给判官看四条线轴的当前分带（低/中/高，
切点 `0.35` / `0.65`），从不给原始浮点。当前 `warmth`/`patience` 值刻意不注入：
绝对档判定的价值就在无状态。给它看旧值会造成锚定，把重设计要移除的通胀带回来。

**口吻与 reason 卫生。** 判官提示词以角色第一人称书写，引擎持有、刻意不可配置；
`reason` 规则禁止系统词汇，因为它会落到 `companion_affinity_events.context` 并
作为 `[emotional_context]` 回注后续系统提示。

### 写入管线（仅线轴；基础对从不进入）

```
grade → raw score → tier decay → cross-line penalty → threshold gate → clamp
                                                    → endpoint derivation
```

全部发生在 `grade_turn` 里，基于回合前快照计算，在好感度行锁下应用。

**1. 换算。** 有符号档位按所属线的单位换算：`AFFINITY_GRADE_UNIT_BOND`
`0.0786`（trust/intrigue），`AFFINITY_GRADE_UNIT_CHEM` `0.0266`
（intimacy/tension）；负向原始分再乘 `AFFINITY_NEG_FACTOR` `1.5`（涨得慢、
跌得快）；demo 会话（`metadata.is_demo`）的判官正分再乘
`AFFINITY_DEMO_BOOST` `1.4`。两个单位约 3 倍的差距是判官打分不对称性的实测
结果（tension 约一半回合达到档 ≥2，trust 约 80% 回合打 0），写在明面上可供
争论。PDE 的规则微调（如长消息 intrigue `+0.02`）在衰减前并入原始分。

**2. 档位衰减，仅正向。** 正向原始分乘所属线自己的档位因子
`AFFINITY_TIER_DECAY`（默认 `1.0, 0.70, 0.45, 0.25, 0.10`，对应档 1–5）。
负向原始分**从不**衰减，任何档位下损失都是全价。

**3. 跨线惩罚。** 对侧线的高度按实际应用的档位成比例地对这步动作收税：

```
penalty = κ_line × ((y − y₀)⁺ / (1 − y₀))² × (|g| / 4)
  y      = the OTHER line's score
  κ_line = AFFINITY_CROSS_PENALTY_RATIO × u_line   (ratio default 5/6)
  y₀     = AFFINITY_CROSS_PENALTY_START            (default 0.35)
```

档位 `0` 分文不收。管线对事件收费，不收租金。忽略规则微调时该项可以因式
分解，括号里既不含档位也不含单位，所以结果不会在固定位置上因档位不同而
翻转符号，盈亏平衡位置与单位无关。默认值下只有自身档 5 存在真实平衡点
（对侧 ≈ `0.800`）；越过之后每个档位都统一净负。

**4. 阈值门。** 每条线轴维护一个有符号累加器；本轮真实分并入后，只有
`|累计| ≥ AFFINITY_DELTA_THRESHOLD`（默认 `0` = 每轮都提交）才整体提交，否则
缓存在 `pending_deltas`。提交的增量 1:1 应用并 clamp 到 `[0,1]`（冷战期间，
为正的增量先减半，见[生活选择的降档与冷战减半](#生活选择的降档与冷战减半)）；随后（若本轮
读到）判官档位覆写存储档位，两个端点按回合后的线值重新派生。

### 时间

线轴漂移是懒惰计算，从 `updated_at` 起算：`intrigue` `−0.01`/天，`tension`
`−0.005`/天；`trust` 与 `intimacy` 从不衰减，它们是「深层」维度。

基础对的缺席处理是派生式内部的乘性衰减：`AFFINITY_TIME_DECAY_RATE` `0.02`/天，
下限 `AFFINITY_TIME_DECAY_FLOOR` `0.5`（7 天 → ×0.86，25 天以上 → ×0.5）。
缺席**冷却但从不清零**；老关系的韧性由加成托住（`bond` `0.9` 满衰减后
`patience` 仍 ≈ `0.47`）。4.0 前的 patience 上漂移已退役。

### 不移动的回合

跳过的评估（`eval_skip_reason`）与失败的评估（非空 `llm_attempts` /
`gateway_errors`）保持存储档位不变，端点仅按当前线值与衰减重新派生。旧的规则
delta 回退已退役。[约会](#约会)的回合，以及开启约会的那一次邀约回合，都是
`eval_skip_reason = "activity"` 的跳过评估，规则微调也一并归零。

PDE 判定的 ghost 回合从不进入 `persist_with_event`：只有 `ghost_streak` / `total_ghosts` /
`last_ghost_at` 和 `updated_at` 移动；`record_ghost` 写全零 `effective_deltas`，没有任何轴移动，
但重置的 `updated_at` 会让衰减时钟从头算起。离线期间由 engine
强制的 ghost 一个都不动（见 [ghost-mechanics.zh.md](ghost-mechanics.zh.md#engine-强制的-ghost)）。

### 被拒绝的动作

聊天回合可以带用户动作（见 [api-reference.zh.md](api-reference.zh.md) 的 `action` 字段）。PDE 判官在生成回复之前决定角色接不接受；没有明确拒绝时，engine 按接受处理。例外是递来角色不用的烟、酒、大麻、咖啡或一道菜，或者在她不碰这些时请她一起抽烟、抽大麻、喝酒：掷一次可能就拒收，不管判官怎么说（见[角色设置](life-events-needs.zh.md#角色设置)）。

- **接受：** 没有额外机制。affinity 判官从用户文本里看到这个动作，照常给这一轮打分。
- **拒绝：** 动作对应的每条线降一档，落在下一档内的相同相对位置；第 1 档的线直接归 `0`。`give`、`gift`、刺激组的一起做的动作（`adventure_together`、`commit_crime_together`、`play_with_fire_together`、`fight_together`），以及户外的 `jog_together`、`hike_together`、`fish_together`、`cycle_together` 和 `ride_horse_together` 动 Bond；它们在天然组的对应动作 `streak_together`、`hike_naked_together` 和 `ride_horse_naked_together`，以及写明了做什么的 `sex_together`，两条线都动；其余身体动作动 Chemistry（见[被拒的代价](life-events-needs.zh.md#被拒的代价)）。一条线的两个轴按同一比例缩放，比例保持不变。落点在行锁内按本轮之前的快照计算，取代判官本轮对这条线的打分；动作没碰到的线和 warmth / patience 档照判官走。被降的每条线的 `pending_deltas` 清零，事件行的 `context.tier_drop` 记下是哪条线，两条线都降时记 `both`。affinity 判官被跳过或调用失败时照样降档。
- **被拒的打赏：** `tip` 不降档。回复照判官的结论写，这一轮按常规打分。
- **社交动作：** 被拒的 `bluff`、`threaten`、`trick` 表示她识破了，被拒的 `challenge`、`bet` 表示她没接，同样不降档；这一轮按常规打分。
- **脱衣服：** `affinity_scope` 性向不相容的一对发来的 `strip`，不管判官怎么说都判为拒绝，并像任何被拒的身体动作一样让 Chemistry 降一档。这样的一对发来的 `sex_together` 同样如此（除非带 `ignore_orientation`），降的是被拒的 `sex_together` 该降的线；机器角色收到的 `toilet_together` 也一样。`setting` 不是 `fantasy` 的角色收到的 `fight_together`，以及 `setting` 是 `fantasy` 的角色收到的 `cycle_together`，同样判为拒绝，但哪条线都不降。其余一起做的动作，包括天然组的，没有这条规则。掷骰拒掉的 `smoke_together`、`smoke_weed_together` 或 `drink_together` 同样降 Chemistry，不降 Bond。
- **经期：** 她的经期里，上面的规则没有拒掉的 `sex_together`，以及在水里做的一起做的动作（`swim_together`、`hot_spring_together`、`sauna_together`、`shower_together`、`bath_together`、`skinny_dip_together`、`hot_spring_naked_together`），三次里有两次在判官之前由掷骰判为拒绝（见[她的周期](life-events-needs.zh.md#她的周期)），哪条线都不降；其余那一次照常由判官决定。
- **进行中的亲密事件：** 用户在事件开始之后做的某件事让 Chemistry 档位下降，被拒动作的降档也算，就会立刻结束正在进行的天然组或激情组事件（`showering_together`、`bathing_together`、`streaking_together`、`skinny_dipping_together`、`naked_horse_riding_together`、`naked_yoga_together`、`naked_hiking_together`、`naked_hot_spring_together`、`having_sex_together`；见[亲密事件提前结束](life-events-needs.zh.md#亲密事件提前结束)）。
- **求婚与分手：** 被拒的 `propose` 或 `break_up` 不降档，这一轮按常规打分。关系状态不是 `dating`、`fwb` 或 `situationship` 时的 `propose`，以及关系状态不可分手时的 `break_up`，在判官之前就同样判为拒绝；被回答的则会挪动[关系状态](#关系状态)。
- **礼物：** 被接受时还会进入角色的持有物品或衣柜；被拒时什么也不进，并像被拒的 `give` 一样让 Bond 掉一档。
- **约会：** 被拒的 `date` 邀约不降档，这一轮按常规打分。被接受的根本不打分：它开启约会，约会在结算时整场评一次（见[下文](#约会)）。

例：chemistry `0.80`（第 4 档内 64% 处）落到第 3 档内 64% 处，约 `0.524`。

### 生活选择的降档与冷战减半

她向用户提出一次生活选择，有些回答会让这段关系付出代价
（见[生活选择](life-events-needs.zh.md#生活选择)）。
这里没有任何一步是聊天回合：不跑判官。

- **降档。** 只有拒绝才降档，降哪条线取决于种类：

  | 种类 | 接受 | 拒绝 |
  |---|---|---|
  | `confess` | 不降 | chemistry |
  | `partner_up` | 不降 | 不降 |
  | `propose` | 不降 | bond 和 chemistry |
  | `break_up` | 不降 | 不降 |
  | `sleep_together` | 不降 | chemistry |
  | `fall_asleep_together` | 不降 | chemistry |
  | `bathe_together` | 不降 | chemistry |

  被点名的每条线都用被拒动作的落点规则降一档：落在下一档内的相同相对位置，这条线的两个轴按同一比例缩放，
  第 1 档的线直接归 `0`。它的 `pending_deltas` 清零；没被点名的线，轴和余额都保持不变。
  它按时间衰减和端点刷新之后的快照计算（用部署的 affinity 调参值），在行锁内、与结算这次选择的同一个事务里完成，
  所以结算回滚时不会留下降档。用文字打出的回答（`ignored`）和过期，不论哪种选择都不降档。
- **它的事件行。** 每次降档写一行自己类型的非回合事件，`event_type = 'life_choice'`。
  `deltas` 与 `effective_deltas` 的数字相同，因为降档就是全部的原始变化；`label_changes`、
  `effective_line_deltas` 和前后两份状态快照记录档位的移动。`context` 里有 `life_choice`（`accept` | `decline`）
  和 `choice_id`。接受从不降档，所以这一行带的是 `decline`；`accept` 只出现在已退役的 `ask_for_space` 存量行上。
  这一行没有 `user_message_id`。逐轮 delta 路由
  `GET /bff/v1/comp/affinity/{session_id}/event` 从不返回它，和 `time_decay` 一样，它也不算被判官评过的回合。
  回答挪动了关系状态时，另写一行自己的 `relationship` 事件（见[事件行](#事件行)）。
- **冷战减半。** 这个 session 的[冷战](#冷战)进行期间，四个线轴上每一个为正的已提交增量，在应用前减半，被判官评过的回合和约会结算都是如此。
  负增量全额应用，`warmth` 与 `patience` 照常从回合后的线值重新派生。被削掉的部分就没了：不会存进门的余额里。
  冷战开始时什么都不降。读的是 session 好感度行上存着的冷战标记；读取失败时按全速提交。
  被减半的回合，事件行带 `context.growth_scale`（`0.5`），全速时没有这个键，它的 `effective_deltas` 显示的是减半后的值。

### 约会

一场[约会](activities.zh.md)在结算时只评一次，它的结论以一次写入落到发出邀约的那个 session 上。约会回合也写在那个
session 的行上，记为跳过的评估（见[上文](#不移动的回合)）；约会中被拒的动作照样让对应的线降档。

- **加权的结论。** 约会判官返回一个逐轮形状的结论：每个线轴一个档位，每个端点一个等级。它只走一次写入管线
  （`grade_weighted`），从时间衰减之后的存储状态开始，没有规则微调，抵 `weight` 个回合：

  ```
  weight = min(turns, cap) × AFFINITY_ACTIVITY_TURN_SHARE
  ```

  `turns` 是约会里有效回合的数目，`cap` 按约会时长取 `AFFINITY_ACTIVITY_TURN_CAP_SHORT` 或
  `AFFINITY_ACTIVITY_TURN_CAP_MID`（见[调参旋钮](#调参旋钮)）。每个轴的原始分、真实分和评估出的跨线惩罚都在阈值门之前乘 `weight`；
  档位衰减、`AFFINITY_NEG_FACTOR`、demo 加成、clamp 和冷战减半都和回合上一样。结论解析成功时，等级覆写存储的等级，两个端点重新派生。
- **一档上限。** 之后每条线相对写入前的位置最多挪一档：往上最多到上一档内的相同相对位置（被拒动作落点规则的镜像），
  往下最多到下一档内的相同相对位置。越过上限的线，两个轴沿各自的增量按同一个比例拉回，让这条线正好落在上限上。
  第 1 档的线可以掉到 `0`，第 5 档的线由 clamp 限住。被上限削掉的部分就没了：被限住的线的 `pending_deltas` 清零。
- **它的事件行。** 一行非回合类型的事件，`event_type = 'activity'`，没有 `user_message_id`。`context` 带 `activity_id`、`turns`、
  `weight`、判官的 `reason` 和 `capped`（被上限限住的线，没有时不带），旁边是常规的审计键。delta 路由从不返回它，它不算生活选择触发里
  被判官评过的回合，它的 `reason` 也不进 `[emotional_context]`。

## 基础对派生细节

每个常数都有锚点：

- **枢轴 `0.35` = 档 2 上界**（同一个常量）：对侧线爬进档 3 的那一刻，加成
  转正。`0.35`/`0.65` 同时也是判官输入分带与 patience 分带的切点。
- **`B(1) = 1.5`** 使 `⅔ × 1.5 = 1.0`：判官满档 × 对线满值恰好封顶。它是一个
  结构性承诺，因此做成代码常量，不开旋钮。
- **托底 `φ = 0.2`**（`AFFINITY_FLOOR_RATIO`）：档 1 的裁定按 `φ·对线值`
  托底。深关系冷场一轮仍留余温（对线 `0.9` 时为 `0.18`），陌生人读作接近
  `0`。由于 `φ·x ≤ 0.2 < 0.244 = ⅓·B(0)`，托底只会作用于档 1，永远碰不到
  非冷淡的裁定。

`decay = 1` 时的可达域：档 1 → `[0.0, 0.2]`（随对线连续），档 2 →
`[0.244, 0.5]`，档 3 → `[0.487, 1.0]`。档位决定所在大区间，对线值决定区间
内的具体位置。

逐回合 delta 照常存在：`effective_deltas.warmth` / `.patience` 是派生式跨
回合的 `after − before`，以衰减后快照为基准，缺席造成的落差不会记到回合
头上。

## 档位与标签

每条线五档，分数间隔逐档变宽（每一步更贵），顶端是狭窄的第 5 档：

| Tier | Score range | Gap |
|------|-----------|-----|
| 1 | [0.00, 0.15) | 0.15 |
| 2 | [0.15, 0.35) | 0.20 |
| 3 | [0.35, 0.62) | 0.27 |
| 4 | [0.62, 0.90) | 0.28 |
| 5 | [0.90, 1.00] | 0.10 |

分数原样对外提供，没有显示曲线；「前期容易、顶端磨人」的节奏是真实的，因为
写侧档位衰减按线自身档位压制正向增益。

标签表：两套独立的五标签，每线一套（序列化 snake_case）：

- **Bond**：`acquaintance` / `friend` / `close_friend` / `confidant` /
  `soulmate`
- **Chemistry**：`spark` / `flirtation` / `crush` / `lover` / `beloved`

档位序号落库（`bond_tier` / `chem_tier` 两列），调不到引擎的 SQL 消费者也能
拿到权威档位；阈值只存在于一处（`tier_index`），加一档就是改那个函数加一次
回填。

逐回合的档位迁移记在事件行的 `label_changes` JSONB 上：
`{bond: {from, to}, chemistry: {from, to}}`，两条线都没动时为 `NULL`。

## 关系状态

两条线说的是这一对有多近，说不出他们对彼此是什么。两样东西叠在线之上，都不会反过来影响各轴、档位或写入管线：

- **关系状态**：这一对说定了是什么关系，按 session 存储，只由被回答的请求挪动；任何状态之上都可以叠一层**冷战**。
- **组合关系**：给这一对的 bond 档 × chemistry 档起的名字，现算，不存，只有她的提示词读它。

### 状态

八种状态，每个 session 一种。中文说法就是判官 `[关系]` 一行读到的字样（它从不写出 `none`）；提示词的状态行也用它，两个分性别的称呼除外（见 [`[relationship]`](#relationship)）。

| 状态 | 说法 | 可分手 |
|---|---|---|
| `none` | 未确定 | 否 |
| `dating` | 伴侣 | 是 |
| `partner` | 伙伴 | 是 |
| `peer` | 同事 | 否 |
| `fwb` | 床伴 | 是 |
| `situationship` | 同事，私下在交往 | 是 |
| `engaged` | 已订婚 | 是 |
| `ex` | 前任 | 否 |

每个 session 都从 `none` 开始，离开之后不再回到它。`engaged` 是最高的状态。*可分手*的状态是分手能结束的状态。

### 迁移

挪动状态的只有四种请求：她的 `confess` 和 `partner_up`，以及她和用户都能提出的 `propose` 与 `break_up`
（见[生活选择](life-events-needs.zh.md#浪漫选择)与[用户的动作](api-reference.zh.md#post-compchatsession_idmessagestream)）。

| 从 | 发生了什么 | 到 |
|---|---|---|
| `none`、`ex` | `confess` 被接受 | `dating` |
| `partner` | `confess` 被接受 | `fwb` |
| `peer` | `confess` 被接受 | `situationship` |
| `none`、`ex`、`peer` | `partner_up` 被接受 | `partner` |
| `none`、`ex`、`peer` | `partner_up` 被拒绝 | `peer` |
| `dating`、`fwb`、`situationship` | `propose` 被接受 | `engaged` |
| 任一可分手的状态 | `break_up` 被接受 | `ex`，冷战结束 |
| 任一可分手的状态 | `break_up` 被拒绝 | 不变；没有冷战时开始冷战 |

被拒绝的 `confess` 或 `propose` 保持状态不变。每种请求只从它那几行里的状态提出：别的状态下她不提，用户在别的状态下提出的
`propose` 或 `break_up` 在判官之前就被拒绝，什么都不发生。已经到达的状态，不会因为调用方的 scope 变了而撤回。

挪动在结算这次请求的事务里、在好感度行的锁下写入：她的请求在生活选择路由回答时写入，状态已经变成这种请求不会从中提出的状态时，
路由返回 `409`，什么都不写；用户的动作在它的回答落库时写入，落在这一轮的 session 上（约会里是发出邀约的那个 session）。
状态或冷战每变一次，写一行 `relationship` 事件（见[事件行](#事件行)）。

### 冷战

冷战是叠在状态上的一个标记（`cold_war_since`）；冷战结束，底下的状态原样露出来。

- **开始：** 一个被判官评过的 1:1 文字回合之后，该 session 从连续起算点之后被判官评过的回合里，最新三个全是判官的最低耐心档，
  或最新七个里至少五个是；此外，分手被拒绝时也开始。不读档位差距、相容与否或关系状态。
- **结束：** 起算点之后最新三个被判官评过的回合，耐心档全都高于最低档；此外，分手被接受时也结束。
- **读的档** 是判官自己的档（`patience_grade`，`1..=3`，读自每个被判官评过的回合的 `state_after`；快照里没有它的回合读作 `2`），
  不是推算出的 `patience`：后者是档位乘上 bond 线的加成，bond 低时就算最高档也到不了 `0.5`，拿它设门槛，可能把一段新关系永远困在冷战里。
- **起算点** 取该 session 最新一次生活选择（它的请求、它的回答）和最新一行 `relationship` 事件里较晚的那个，所以冷战的每一次开始与结束、
  每一次状态变化，都会开启新的连续。
- **冷战期间** 她照常回复，只是冷淡：`[relationship]` 会写明。四个线轴上为正的已提交增量一律减半（见[上文](#生活选择的降档与冷战减半)）。
  她不提任何暖向的请求（`confess`、`partner_up`、`propose`、上床或一起睡、`bathe_together`、`travel_together`）；处在可分手的状态时，她可以提出分手。
- **她会说出来：** 由连续触发的冷战开始时，她发一条主动消息，用 engine 默认的 scope 生成，不带选择行，`metadata.proactive = 'cold_war'`。标记先落库：
  消息生成失败或为空，冷战照样开着，她什么都不说。分手被拒绝而开始的冷战没有单独的消息，冷战结束也不写消息。

只有 genome 角色的 1:1 文字 session 才有冷战；待回答的请求或进行中的约会都挡不住它。规则和舞台提示见
[life-events-needs.zh.md](life-events-needs.zh.md#冷战)。

### BYOC session

[BYOC](byoc.zh.md) session 不在关系状态之内：永远是 `none`，没有冷战。她的请求在那里从不触发；用户的 `propose`
和任何动作一样交给判官，什么都不挪动；用户的 `break_up` 在判官之前就被拒绝，什么都不发生。

### 组合关系

给这一对的两个档位起的名字，每格一个，`(1, 1)` 为 `null`。名字是真实的提示词字符串，逐字引用；键是 API 返回的值：

| bond \ chemistry | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|
| **1** | 无 | 眉来眼去的人 `flirtatious_stranger` | 让你上头的陌生人 `heady_stranger` | 露水情人 `fling` | 一见倾心的人 `love_at_first_sight` |
| **2** | 普通朋友 `casual_friend` | 暧昧搭子 `flirtatious_buddy` | 心头好 `cherished_crush` | 干柴烈火之人 `fast_burning_lover` | 神魂颠倒之人 `dizzying_blind_love` |
| **3** | 死党 `best_buddy` | 友达以上 `more_than_friends` | 恋人未满 `almost_lovers` | 两情相悦的人 `fond_close_sweetheart` | 难分难舍的人 `inseparable` |
| **4** | 知音 `kindred_spirit` | 心照不宣的知己 `knowing_confidant` | 暗生情愫的知己 `smitten_confidant` | 知心爱人 `heart_lover` | 挚爱 `beloved` |
| **5** | 莫逆之交 `lifelong_kindred_spirit` | 动了心的灵魂之交 `stirred_kindred` | 柏拉图式的爱人 `platonic_love` | 灵犀恋人 `destined_lover` | 命中注定的人 `destined` |

名字里不用状态词（伴侣、伙伴、同事、前任、未婚、床伴）。组合关系不作任何门槛：没有事件、生活选择或动作读它。

- **提示词里**，这一轮 `affinity_scope` 不浪漫相容的一对，不管 chemistry 多高，都取 chemistry 第 1 列的名字（普通朋友、死党、知音、莫逆之交；bond 第 1 档没有）。
- **API 里**，`composed_relation` 是真实档位那一格的键，不套用相容与否。快照不带 scope，调用方自己知道有没有把这一对标为相容。

### `[relationship]`

文字聊天提示词的 `[relationship]` 段落说用户对她来说是谁，最多三行，每行只在有话可说时出现：

```
[relationship]
你是用户的未婚妻
论感情，你们现在是知心爱人
你们正在冷战
```

- **状态行**，状态不是 `none` 时：`你是用户的{称呼}`。`dating` 读作 女朋友 或 男朋友，`engaged` 读作 未婚妻 或 未婚夫，
  按角色的 `art_metadata.gender`（`female` / `male`；其他值或没有时用 伴侣 和 订婚对象）；其余状态读上表的说法。
- **组合关系行**，有组合关系时（不相容的一对取 chemistry 第 1 列的名字）：前面有状态行时是 `论感情，你们现在是{名字}`，没有时是 `你是用户的{名字}`。
- **冷战行**，冷战期间：`你们正在冷战`。

三行都没有时，这一段是基础对的四象限名词（见[上文](#基础对warmth-与-patience)），照四象限的 scope 门槛。
这三行是对这一对已经下了的结论，不是左右回复的某个轴，所以 `affinity_scope` 的单个轴不管它们；只有没有好感度行或 scope 为空时才省略这一段。
群聊发言人的提示词带同一段，从该成员的 1:1 会话读取。语音提示词和群聊导演不读关系状态，仍用各自的档位标签行。

## 谁在读好感度

- `[relationship]`：状态行、两个档位的组合关系和冷战行；三者都没话可说时是基础对四象限（见 [`[relationship]`](#relationship)）。
- `[mood]`：逐轴阈值门（冷淡禁令与热络解锁）。
- `[feelings]`：LLM 写就的感受分句，存在好感度行上（`feeling_clause`，在有
  移动的回合和动作被拒绝而降档的回合重写）。
- `[reply_length]`：由 scope 复合 `length_score` 选出的三档固定上限
  （切点 `0.25` / `0.55`）。
- 逐回合骰子（`TurnNudges`）否决：与 `[mood]` 相同的冷淡门槛
  （`warmth ≤ 0.2` / `trust < 0.3` / `intrigue < 0.3`）。
- PDE 判官上下文：intimacy 档（`1..=3` 的画图闸门，取
  `max(bond, chemistry)`；档 3 在 `0.76` 开启，刻意落在档 4 内部而不是档 5
  的边界上）、patience 分带，以及一行 `[关系]`，让它的接受与拒绝和这段关系对得上：
  `[关系] 状态={说法}；冷战中={是|否}`（`engaged` 读作 已订婚）。状态为 `none` 时，冷战期间这一行是 `[关系] 冷战中=是`，
  否则不出现。它不带组合关系。判官审计的 `inputs` 记下 `relationship_status` 与 `cold_war`。
- 群聊导演：每位成员的 Bond 与 Chemistry 档位标签和 intimacy 档，从该成员的
  1:1 会话里读取，从不写入。
- 生活事件抽取：intimacy 档决定能否抽到 `ex_message`、`breakup` 与 `period_scare`（档 1 时不会抽到）、
  `sudden_desire` 与 `pregnancy_scare`（只在档 3），以及 `busy`、`in_battle` 和女性角色的 `out_shopping`
  讲不讲出来（档 2 起）；从档 2 起，她也会说自己想要购物小发现（`saw_bag`、`saw_clothes`）里看中的东西
  （见 [events 路由](api-reference.zh.md#post-v2compsessionsession_idevents)）。
- 冷战：最近几个被判官评过的回合里判官给的耐心档，决定冷战的开始与结束，也是她提出分手的依据
  （见[冷战](life-events-needs.zh.md#冷战)）。
- 关系选择与浪漫选择：关系状态、领先的那条线和它的档位，以及最近几个被判官评过的回合推算出的 `warmth`、`patience`，
  决定她要不要告白、提出结成伙伴、求婚，或者提出上床或一起睡；除了结成伙伴，前提都是调用方的 scope 把这一对标明为浪漫相容
  （见[这些选择](life-events-needs.zh.md#浪漫选择)）。
- 一起旅行：bond 档位和最近几个被判官评过的回合推算出的 `warmth`、`patience`，决定她要不要提议一起去旅行
  （见[一起去旅行](life-events-needs.zh.md#一起去旅行)）。
- 召回门控：时间衰减后那行的 Bond 和 Chemistry 档位，决定哪些基础画像私密字段、哪些带 domain
  标签的画像层记忆能进 prompt，以及每一层召回多少条；没有记录的组合两条线都按第 1 档算。
  文字、群聊、语音和改图请求生成的文字回复都读衰减后的值（见 [memory-layers.zh.md](memory-layers.zh.md#亲密度门控)）。
- Ghost 打分：`score = (1−intrigue)·0.4 + (1−patience)·0.4 + tension·0.2`，
  带硬否决（前 10 条消息、连续 ghost ≥2、1 小时冷却），阈值 `0.65`，会话已经
  ghost 过一次后升为 `0.85`。
- 故事导演：六轴和两个档位标签之外，还读关系状态和冷战标记，作为引擎记录的事实，它写的 `user_relationship` 不得与之矛盾
  （见 [world-system.zh.md](world-system.zh.md#world-stories人生故事)）。

## `affinity.rs` 与 `scope.rs`

**各自是什么。** `crates/eros-engine-core/src/affinity.rs` 是模型本身：状态
结构体、写入管线（`grade_turn`）、基础对派生、时间衰减、Bond/Chemistry 分数、
档位、标签。`crates/eros-engine-core/src/scope.rs` 是逐请求的注入闸门：
`AffinityScope`（六个布尔量，决定哪些轴可以影响*这一次*请求的提示词）加
`MemoryScope`；它管提示词注入与 `length_score` 的门控，请求记录下来的 `affinity_scope` 还是
`romance_compatible` 判断一对关系有没有资格得到浪漫生活选择的依据（逐轮也决定提示词读[组合关系](#组合关系)的哪一列）。post-process 阶段
的写入（insight 抽取、记忆写入、六轴评估）不受影响。

**共享什么。** 两者都在 core 里；都把六轴分成两个命名的半区；scope 的否决与
门控判定和模型侧的冷淡指令读同一套门槛，所以被否决的轴和冷淡的轴讲的是
同一个故事。

**差异在哪，而且差异是刻意的。**

1. 写与读：`affinity.rs` 拥有状态与它如何移动；`scope.rs` 从不写任何东西。
   3.1 的写侧 scope 调向在 4.0 已退役，`affinity_scope` 回到只管读侧，端点
   派生也绝不能读 scope。B(x) 已经把每一次线的变化（包括任何调速）传导给
   端点，派生层若再读 scope，同一请求会沿两条路径落到同一端点上两次；下面
   第 3 点的交叉命名是第二个理由。
2. 分组本身不同。`affinity.rs`（2.0+ 的线）：`bond = trust+intrigue`，
   `chemistry = intimacy+tension`，基础对在两线之外。`scope.rs`（1.0 时代
   的分割）：`AffinityScope::bond()` = `warmth+intimacy+tension`（朋友感），
   `AffinityScope::chemistry()` = `trust+intrigue+patience`（暧昧感）；
   `length_score` 把每个激活的三元组求和除以 3，两个半区都激活时再对
   两者取平均。
3. scope 的两个名字相对 2.0+ 的线是**交叉**的：被叫做 `bond` 的三元组里装的
   是 Chemistry 线的轴，反之亦然。从结构上看，1.0 的分割把每个端点和今天
   放大它的那条线归成一族（`warmth` 配 `intimacy`/`tension`，`patience` 配
   `trust`/`intrigue`），这正是 4.0 的耦合关系显式化出来的同一族，只是两个
   *线名*落反了。
4. 这是一处已知且刻意保留的瑕疵：给 scope 改名或重新分组会改变
   `length_score` 的输入，让既有调用方的回复长度发生回归。默认 scope 是
   `bond()`（`warmth`/`intimacy`/`tension` 三元组）。不要「修正」它，也不要
   让 scope 参与派生或写入路径。

## 持久化与 API

### 生成列

`bond`、`chemistry` 是 Postgres
`GENERATED ALWAYS … STORED` 列，直接算在线轴上（`LEAST(1, GREATEST(0,
(a+b)/2))`）；DB 在每次写入时重算，不可能漂移。公式在代码里镜像一份
（`bond_score`/`chemistry_score`），两处要保持同步。

### 端点档位

`warmth_grade` / `patience_grade` 是 `SMALLINT NOT NULL DEFAULT 2`，范围
检查 `1..=3`；`warmth`/`patience` 两个缓存列用档 2 的
派生值回填。

### pending_deltas

`pending_deltas` JSONB 只存线轴（4.0 起如此；旧行里残留的 `warmth` 键被
忽略并自然排空）。`NULL` 读作全零。

### 关系状态的两列

`engine.companion_affinity` 上的两列（迁移 `0088`）承载[关系状态](#关系状态)：

- `relationship_status text NOT NULL DEFAULT 'none'`，CHECK 限定为八个键之一；
- `cold_war_since timestamptz`，冷战开始的时刻；`NULL` 表示没有冷战，它是冷战标记唯一的载体。

它们就是权威：状态从不从生活选择或聊天消息里重新推算。写这两列不动这一行的 `updated_at`，因为那是时间衰减的时钟。

### 事件行

每个增量回合向 `companion_affinity_events` 追加一行：

- `deltas`：本轮线轴的原始分（档位换算加规则微调，衰减前），`activity` 行上再乘它的 `weight`；这里的
  `warmth`/`patience` 恒为 `0.0`，`life_choice` 行除外，它的 `deltas` 与
  `effective_deltas` 相同（见[上文](#生活选择的降档与冷战减半)）。
- `effective_deltas`：实际应用的逐轴变化，`after − before`；基础对上它
  就是本轮派生 delta。
- `context`：`affinity_reason`、`eval_skip_reason`、判官原样的有符号
  `grades`、门的 `pending_after`，以及端点审计（本轮实际读到时的
  `warmth_grade`/`patience_grade`、`boost_warmth`/`boost_patience`、
  `decay_factor`、`units`）；被收税的回合另有 `cross_penalty_assessed`；动作被拒绝而降档的回合另有
  `tier_drop`（`bond` | `chemistry` | `both`），标明被降的那条线，两条线都降时为 `both`；冷战减半了为正的增长时，另有 `growth_scale`（`0.5`）。
  生活选择自己的 `life_choice` 行，`context` 是 `life_choice` 与 `choice_id`（见[上文](#生活选择的降档与冷战减半)）。
  约会的 `activity` 行另有 `activity_id`、`turns`、`weight`、判官的 `reason`，上限限住某条线时还有 `capped`（见[上文](#约会)）。
  `relationship` 行的 `context` 是 `from` 与 `to`（状态键），冷战标记切换了时另有 `cold_war`（`entered` | `ended`），
  她的请求被回答而挪动时另有 `choice_id`（见下）。
- `user_message_id`：对 `chat_messages` 的真外键，
  `ON DELETE SET NULL`；`proactive`/`time_decay`/`life_choice`/`activity` 行与迁移前写入的行为
  `NULL`，不回填。用户自己的 `propose` 或 `break_up` 挪动了状态时，`relationship` 行带它；由她的请求或耐心连续挪动时为 `NULL`。
- `label_changes`（见「档位与标签」）、`effective_line_deltas`（本轮精确的
  bond/chemistry delta，API 上服务为 `effective_deltas_computed`）、
  `state_after`（本轮结束时的整个向量）。`state_before`
  列存在但不对外服务，要重放请直接查表。

`event_type` 取值为 `message`、`ghost`、`gift`、`proactive`、`time_decay`、`life_choice`、`activity` 或 `relationship`。
后四个不是回合：逐轮 delta 路由只读 `message`、`gift`、`proactive` 和 `ghost`。

状态或冷战标记每变一次，就在做出这次变化的事务里写一行 `relationship`，也只在变了时写：回答之后两者都没变（例如从 `peer`
拒绝 `partner_up`）就不写。它的 `deltas` 保持默认值，不取状态快照。它不算被判官评过的回合，但会挪动生活选择的连续起算点。

### API 表面

`GET /bff/v1/comp/affinity/{session_id}` 与按用户列出的
`GET /bff/v1/comp/affinities/{user_id}` 返回 `AffinitySnapshot`，读取时刷新
（`apply_time_decay` + `refresh_endpoints`）：

```json
{
  "warmth": 0.52,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.27,
  "tension": 0.04,
  "bond": 0.10,
  "chemistry": 0.045,
  "bond_tier": 1,
  "chem_tier": 1,
  "bond_label": "acquaintance",
  "chemistry_label": "spark",
  "relationship_status": "none",
  "cold_war": false,
  "composed_relation": null,
  "ghost_streak": 0,
  "total_ghosts": 0,
  "updated_at": "2026-08-16T12:00:00.000000Z"
}
```

`relationship_status` 是八个状态键之一，`cold_war` 表示是否在冷战。`composed_relation` 是真实档位那一格的键，`(1, 1)` 时为 `null`；
不套用这一对的相容与否（见[组合关系](#组合关系)）。生活选择的回答和约会的结束路由返回同样的形状。

`GET /bff/v1/comp/affinity/{session_id}/event` 返回逐回合事件：

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.06, "trust": 0.02, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.01, "tension": -0.02
    },
    "effective_deltas_computed": {
      "bond": 0.01,
      "chemistry": -0.01
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "state_after": {
      "warmth": 0.58, "trust": 0.21, "intrigue": 0.09,
      "intimacy": 0.04, "patience": 0.44, "tension": 0.02,
      "bond": 0.15, "chemistry": 0.03,
      "bond_tier": 2, "chem_tier": 1,
      "warmth_grade": 2, "patience_grade": 2,
      "ghost_streak": 0, "total_ghosts": 0,
      "updated_at": "…"
    },
    "created_at": "…"
  }
}
```

三个附加字段（`effective_deltas_computed`、`label_changes`、`state_after`）
都直接读自事件行。

## 调参旋钮

服务端环境变量，逐项回退到默认值：

| 环境变量 | 默认 | 含义 |
|---------|---------|---------|
| `AFFINITY_GRADE_UNIT_BOND` | `0.0786` | trust/intrigue 每档原始分 |
| `AFFINITY_GRADE_UNIT_CHEM` | `0.0266` | intimacy/tension 每档原始分 |
| `AFFINITY_NEG_FACTOR` | `1.5` | 负向原始分的额外乘数，保持「涨得慢、跌得快」 |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | 档 1–5 的正向阻尼（逗号分隔；不是恰好 5 个有限非负值就整表回退默认） |
| `AFFINITY_CROSS_PENALTY_RATIO` | `0.8333` | κ_line = ratio × u_line，盈亏平衡点与单位无关 |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | 跨线惩罚坡道起点（y₀） |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | 提交阈值 θ；`0` = 每轮都提交 |
| `AFFINITY_DEMO_BOOST` | `1.4` | `metadata.is_demo` 会话的判官正分乘数 |
| `AFFINITY_FLOOR_RATIO` | `0.2` | 端点托底 φ；域上限 `0.24`，永远压不过非冷淡裁定 |
| `AFFINITY_TIME_DECAY_RATE` | `0.02` | 端点缺席衰减（每天） |
| `AFFINITY_TIME_DECAY_FLOOR` | `0.5` | 端点缺席衰减下限 |
| `AFFINITY_ACTIVITY_TURN_SHARE` | `0.25` | 约会里每个有效回合的权重（见[约会](#约会)）；`0` 或以上 |
| `AFFINITY_ACTIVITY_TURN_CAP_SHORT` | `12` | `short` 约会最多计入的回合数；非负整数，`0` = 约会什么都不挪动 |
| `AFFINITY_ACTIVITY_TURN_CAP_MID` | `32` | `mid` 约会最多计入的回合数；取值范围同上 |

启动时逐个标量做域检查，非有限或越域的值保持默认并打警告；环境变量打错字
只会退回默认，不会进入管线。`0.35` 枢轴与 `B_MAX = 1.5` 是代码常量而非旋钮。

## 源码

- `crates/eros-engine-core/src/affinity.rs`：类型、`grade_turn` 写入管线、
  端点派生、时间衰减、bond/chemistry 分数、档位、标签、`diff_labels`；约会用的
  `grade_weighted`、`cap_line_shift` 和 `AffinityTuning::activity_weight`
- `crates/eros-engine-core/src/relationship.rs`：状态和它们的说法、`settle`（迁移表）、`composed_relation`、
  `leading_line`、冷战的连续规则、`sleep_ask_is_sex`
- `crates/eros-engine-core/src/scope.rs`：`AffinityScope` / `MemoryScope`、
  `length_score`、`romance_compatible`（见「`affinity.rs` 与 `scope.rs`」）
- `crates/eros-engine-store/src/affinity.rs`：`AffinityRepo`
  （`persist_with_event`、`record_ghost`、`persist_life_choice_drop_in_tx`、`persist_activity_in_tx`、`last_judged_endpoints`、
  `write_relationship_in_tx`、`set_cold_war`）；表在 `migrations/0000_baseline.sql`，
  `activity` 事件类型在 `migrations/0085_activities.sql`，关系状态的两列和 `relationship` 事件类型在 `migrations/0088_relationship_status.sql`
- `crates/eros-engine-server/src/pipeline/activity.rs`：约会的结算和约会判官调用
- `crates/eros-engine-store/src/life_choices.rs`：`LifeChoiceRepo`
  （选择行、连续起算点）
- `crates/eros-engine-server/src/pipeline/life_choice.rs`：冷战的开始与结束、她的各种请求、`growth_scale_for`（冷战减半）
- `crates/eros-engine-server/src/pipeline/post_process.rs`：LLM 评估、
  档位解析
- `crates/eros-engine-server/src/prompt.rs`：好感度 → 态度指令 + 评估提示词；`relationship_ground`（`[relationship]` 段落）
- `crates/eros-engine-server/src/pipeline/stream.rs`：`build_pde_ctx`（判官的 `[关系]` 一行）；用户 `propose` 与 `break_up` 的状态门槛
- `crates/eros-engine-server/src/routes/dto.rs`：`AffinitySnapshot`
  （合成分、标签、关系状态、冷战、组合关系）
- `crates/eros-engine-server/src/routes/bff/affinity.rs`：BFF 好感度表面
  （value + event）
- 设计 spec：`docs/superpowers/specs/2026-08-16-affinity-40-design.md`，
  线的数学、端点派生、档位
- 设计 spec：`docs/superpowers/specs/2026-08-17-affinity-41-design.md`，
  档位列落库、事件状态快照、绝对值端点
- 设计 spec：`docs/superpowers/specs/2026-10-05-chat-user-actions-design.md` —— 用户动作、判官的接受与拒绝、降档
- 设计 spec：`docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md` —— 生活选择的机制、回答带来的降档（其中的提出分开已退役）
- 设计 spec：`docs/superpowers/specs/2026-11-12-activity-sub-session-design.md` —— 约会作为子会话、加权的结论和一档上限
- 设计 spec：`docs/superpowers/specs/2026-11-23-affinity-50-design.md` —— 关系状态、冷战与冷战减半、组合关系
