# 好感度模型

[English](affinity-model.md) · [中文](affinity-model.zh.md) · [日本語](affinity-model.ja.md)

## 概述

好感度是每个关系（即每个 session）一份的六轴向量，每轴限定在 `[0, 1]`，每次更新都
clamp。它只在文本通道、非 `product_qa` 的聊天回合上移动。语音回合与 `product_qa`
回合从不写好感度事件。判官（LLM 评估器）在任何地方都只输出 ordinal 的档位或等级；
引擎拥有每一个数字、每一个标签、每一次档位迁移，是唯一权威来源。

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

- `[relationship]` 提示词段落：`warmth` × `patience` 四象限，各以 `0.5` 为界。
  四格里的名词是真实的提示词字符串，逐字引用：

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
缓存在 `pending_deltas`。提交的增量 1:1 应用并 clamp 到 `[0,1]`；随后（若本轮
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
delta 回退已退役。

Ghost 回合从不进入 `persist_with_event`：只有 `ghost_streak` / `total_ghosts` /
`last_ghost_at` 移动；`record_ghost` 写全零 `effective_deltas`。

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

## 谁在读好感度

- `[relationship]`：基础对四象限（见「基础对：warmth 与 patience」）。
- `[mood]`：逐轴阈值门（冷淡禁令与热络解锁）。
- `[feelings]`：LLM 写就的感受分句，存在好感度行上（`feeling_clause`，在有
  移动的回合重写）。
- `[reply_length]`：由 scope 复合 `length_score` 选出的三档固定上限
  （切点 `0.25` / `0.55`）。
- 逐回合骰子（`TurnNudges`）否决：与 `[mood]` 相同的冷淡门槛
  （`warmth ≤ 0.2` / `trust < 0.3` / `intrigue < 0.3`）。
- PDE 判官上下文：intimacy 档（`1..=3` 的画图闸门，取
  `max(bond, chemistry)`；档 3 在 `0.76` 开启，刻意落在档 4 内部而不是档 5
  的边界上）与 patience 分带。
- Ghost 打分：`score = (1−intrigue)·0.4 + (1−patience)·0.4 + tension·0.2`，
  带硬否决（前 10 条消息、连续 ghost ≥2、1 小时冷却），阈值 `0.65`，会话已经
  ghost 过一次后升为 `0.85`。

## `affinity.rs` 与 `scope.rs`

**各自是什么。** `crates/eros-engine-core/src/affinity.rs` 是模型本身：状态
结构体、写入管线（`grade_turn`）、基础对派生、时间衰减、Bond/Chemistry 分数、
档位、标签。`crates/eros-engine-core/src/scope.rs` 是逐请求的注入闸门：
`AffinityScope`（六个布尔量，决定哪些轴可以影响*这一次*请求的提示词）加
`MemoryScope`；它只管提示词注入与 `length_score` 的门控。post-process 阶段
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

Migration `0048` 把 `bond`、`chemistry` 重定义为 Postgres
`GENERATED ALWAYS … STORED` 列，直接算在线轴上（`LEAST(1, GREATEST(0,
(a+b)/2))`）；DB 在每次写入时重算，不可能漂移。公式在代码里镜像一份
（`bond_score`/`chemistry_score`），两处要保持同步。

### 端点档位

`warmth_grade` / `patience_grade` 是 `SMALLINT NOT NULL DEFAULT 2`，范围
检查 `1..=3`（migration `0048`）；`warmth`/`patience` 两个缓存列用档 2 的
派生值回填。

### pending_deltas

`pending_deltas` JSONB 只存线轴（4.0 起如此；旧行里残留的 `warmth` 键被
忽略并自然排空）。`NULL` 读作全零。

### 事件行

每个增量回合向 `companion_affinity_events` 追加一行：

- `deltas`：本轮线轴的原始分（档位换算加规则微调，衰减前）；这里的
  `warmth`/`patience` 恒为 `0.0`。
- `effective_deltas`：实际应用的逐轴变化，`after − before`；基础对上它
  就是本轮派生 delta。
- `context`：`affinity_reason`、`eval_skip_reason`、判官原样的有符号
  `grades`、门的 `pending_after`，以及端点审计（本轮实际读到时的
  `warmth_grade`/`patience_grade`、`boost_warmth`/`boost_patience`、
  `decay_factor`、`units`）；被收税的回合另有 `cross_penalty_assessed`。
- `user_message_id`（migration `0056`）：对 `chat_messages` 的真外键，
  `ON DELETE SET NULL`；`proactive`/`time_decay` 行与迁移前写入的行为
  `NULL`，不回填。
- `label_changes`（见「档位与标签」）、`effective_line_deltas`（本轮精确的
  bond/chemistry delta，API 上服务为 `effective_deltas_computed`）、
  `state_after`（本轮结束时的整个向量，migration `0049`）。`state_before`
  列存在但不对外服务，要重放请直接查表。

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
  "ghost_streak": 0,
  "total_ghosts": 0,
  "updated_at": "2026-08-16T12:00:00.000000Z"
}
```

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

启动时逐个标量做域检查，非有限或越域的值保持默认并打警告；环境变量打错字
只会退回默认，不会进入管线。`0.35` 枢轴与 `B_MAX = 1.5` 是代码常量而非旋钮。

## 源码

- `crates/eros-engine-core/src/affinity.rs`：类型、`grade_turn` 写入管线、
  端点派生、时间衰减、bond/chemistry 分数、档位、标签、`diff_labels`
- `crates/eros-engine-core/src/scope.rs`：`AffinityScope` / `MemoryScope`、
  `length_score`（见「`affinity.rs` 与 `scope.rs`」）
- `crates/eros-engine-store/src/affinity.rs`：`AffinityRepo`
  （`persist_with_event`、`record_ghost`）、migrations 0048–0049
- `crates/eros-engine-server/src/pipeline/post_process.rs`：LLM 评估、
  档位解析
- `crates/eros-engine-server/src/prompt.rs`：好感度 → 态度指令 + 评估提示词
- `crates/eros-engine-server/src/routes/dto.rs`：`AffinitySnapshot`
  （合成分 + 标签）
- `crates/eros-engine-server/src/routes/bff/affinity.rs`：BFF 好感度表面
  （value + event）
- 设计 spec：`docs/superpowers/specs/2026-08-16-affinity-40-design.md`，
  线的数学、端点派生、档位
- 设计 spec：`docs/superpowers/specs/2026-08-17-affinity-41-design.md`，
  档位列落库、事件状态快照、绝对值端点
