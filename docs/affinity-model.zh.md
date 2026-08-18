# 好感度模型

[English](affinity-model.md) · [中文](affinity-model.zh.md)

好感度是一个六维向量，在每个文本通道、非 `product_qa` 的聊天回合更新。四条
**线轴**折叠出两条派生线——**Bond**（友情线）与 **Chemistry**（浪漫线）；两个
**端点轴**（`warmth`、`patience`）则是*派生量*：判官给出粗粒度的绝对档位，
引擎用对侧线分数把它折成连续值。语音通道与 `product_qa` 回合从不写好感度事件。
每条线有档位与标签，引擎是分数、标签与逐回合标签迁移的唯一权威来源。

## 六个基础轴

| 轴 | 范围 | 默认值 | 影响 |
|------|-------|--------|------|
| `warmth` | 0.0 ↔ 1.0 | ≈ `0.244`（派生） | 语气、称呼。**派生端点**（4.0）：`warmth = max(base(档位)·B(chemistry), φ·chemistry) × decay`。 |
| `trust` | 0.0 ↔ 1.0 | `0.0` | 话题深度、自我袒露意愿。Bond 轴。 |
| `intrigue` | 0.0 ↔ 1.0 | `0.0` | 好奇心、追问、反 ghost 驱动。Bond 轴。 |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | 内梗、昵称、呼应早前细节。Chemistry 轴。 |
| `patience` | 0.0 ↔ 1.0 | ≈ `0.244`（派生） | 对短消息/低投入消息的容忍度；ghost 阈值输入。**派生端点**（4.0）：`patience = max(base(档位)·B(bond), φ·bond) × decay`。 |
| `tension` | 0.0 ↔ 1.0 | `0.0` | 推拉、俏皮摩擦、傲娇余地。Chemistry 轴。 |

六轴全部限定在 `[0, 1]`，每次更新都 clamp。每行的权威事实是四条线轴、两个判官
**档位**（`warmth_grade` / `patience_grade`，`1..=3`，migration `0048`）与
`updated_at`；存储的 `warmth`/`patience` 值是派生结果的物化缓存，在时间衰减
运行的地方同步刷新。

### 档位化写入（线轴）

判官报告的是逐轴*档位*而非数值增量；引擎把档位换算成分数、做衰减与门控（见
[写入管线](#写入管线affinity-40)），再把提交的增量 1:1 应用：

```
new_value = clamp(old_value + committed_delta)
```

提交增量就是字面含义——阻尼是管线里的档位衰减，发生在写入之前而非写入之上。
`metadata.is_demo` 的会话给判官正分乘 `AFFINITY_DEMO_BOOST`（默认 `1.4`）。

### 时间衰减

无活动时两条线轴随真实时间漂移，在每次加载时从 `updated_at` 惰性计算：

```
days_elapsed = (now − updated_at) / 1 天

intrigue = clamp(intrigue − 0.01  × days_elapsed, 0.0, 1.0)
tension  = clamp(tension  − 0.005 × days_elapsed, 0.0, 1.0)
```

`trust` 与 `intimacy` 不衰减——它们是「深层」维度。旧的 `patience` 每日上漂移
已退役：端点的缺席处理是派生式里的乘性衰减（见下），它**冷却**而非治愈。

## 派生端点（affinity 4.0）

`warmth` 与 `patience` 不再是累积状态。每个判定回合，判官对每个端点报一个绝对
**档位**——`1` 冷淡/不耐烦、`2` 常态（压倒性常见的裁定）、`3` 明显热络/上心——
引擎据此派生连续值：

```
base(档位)  = (档位 − 1) / 3                     ∈ {0, 1/3, 2/3}
B(x)        = 1 + λ·(x − 0.35)                   λ = (1.5−1)/(1−0.35) = 10/13
decay(Δt)   = max(FLOOR, 1 − RATE·天数)          Δt 自 updated_at 起算

warmth   = clamp01( max(base(w档)·B(chemistry), φ·chemistry) × decay )
patience = clamp01( max(base(p档)·B(bond),      φ·bond)      × decay )
```

耦合方向的语义是**加成，不是相关**：chemistry 越深表达越温暖，bond 越深耐心
越足。低 bond × 高 chemistry 天然给出傲娇形态（没耐心但热络）；高 bond × 低
chemistry 给出老友形态（有耐心但冷静）——不需要提示词特判。

每个常数都有锚点，没有拍脑袋数字：

- **枢轴 `0.35` = 第 2 档上界**（同一常量）：对侧线爬进第 3 档那一刻加成转正；
  之下真实值被压到基础分以下。`0.35`/`0.65` 同时也是判官输入与 patience 分带
  的切点。
- **`B(1) = 1.5`** 使 `⅔ × 1.5 = 1.0`：判官满档 × 对线满值恰好封顶。
- **托底 `φ = 0.2`**（`AFFINITY_FLOOR_RATIO`）：档 1 的裁定读作
  `φ·对线值` 而非绝对零——深关系冷场一轮仍有余温（对线 `0.9` 时为 `0.18`），
  陌生人则归零。由于 `φ·x ≤ 0.2 < 0.244 = ⅓·B(0)`，托底只会作用于档 1，
  永远不会改写非冷淡裁定。
- **衰减**（`AFFINITY_TIME_DECAY_RATE` `0.02`/天，`AFFINITY_TIME_DECAY_FLOOR`
  `0.5`）：7 天 → ×0.86，25 天以上 → ×0.5。久别冷却但不清零——老关系的韧性
  由加成托住（bond `0.9` 满衰减后 patience 仍 ≈ `0.48`）。

`decay = 1` 时的可达域：档 1 → `[0.0, 0.2]`（随对线连续），档 2 →
`[0.244, 0.5]`，档 3 → `[0.487, 1.0]`。档位决定大区间，对线值决定区间内位置。

**逐回合 delta 照常输出。**`effective_deltas.warmth` / `.patience` 是派生值
跨回合的 `after − before`，以衰减后快照为基准——缺席造成的落差不会记到回合头上。

**跳过的回合保持档位。**评估被跳过（`eval_skip_reason`）或失败时（自 v1.4.0
起，改由非空的 `llm_attempts` / `gateway_errors` 说明——调用失败不算跳过），
存储档位保持不变，端点按当前线值与衰减重新派生。旧的规则 delta 回退（±0.02 消息长度
微调、超时 −0.05）已退役——停滞规则被衰减吸收。

**Ghost 是独立路径。**Ghost 回合不进 `persist_with_event`，只更新
`ghost_streak` / `total_ghosts` / `last_ghost_at`。PDE 的 ghost 增量只碰
`tension`。

## 两条派生线

四条线轴产出两个合成分数。4.0 起两线不共享任何轴：

```
bond      = (trust    + intrigue) / 2    ∈ [0, 1]
chemistry = (intimacy + tension)  / 2    ∈ [0, 1]
```

`bond` 是友情——信任加持续的兴趣；`chemistry` 是浪漫——亲近加张力。端点按构造
被排除在两线之外（它们是线的*输出*）。

默认种子（线轴全 0）下，新会话从 bond = chemistry = 0 开始——两线都在第 1 档
（stranger）——两个端点在档 2 的受抑基础值 ≈ `0.244`。

> **命名注记：**`AffinityScope::bond()/chemistry()`（用于注入范围、
> `length_score`）采用*另一套*轴分组——那是 1.0 时代的分割，刻意不动以避免
> 回复长度回归。按结构看它早就把 `warmth` 与 `intimacy`/`tension` 归为一族、
> `patience` 与 `trust`/`intrigue` 归为一族——正是 4.0 耦合显式化的同一分族——
> 但它的两个*线名*相对 2.0+ 的线是交叉的，这也是 scope 绝不能参与派生的
> 原因之一。

## 档位

每条线有**五个档位**，分数间隔逐档变宽（每一步更贵），顶端是狭窄的第 5 档：

| 档位 | 分数区间 | 间隔 |
|------|-----------|-----|
| 1 | `[0.00, 0.15)` | 0.15 |
| 2 | `[0.15, 0.35)` | 0.20 |
| 3 | `[0.35, 0.62)` | 0.27 |
| 4 | `[0.62, 0.90)` | 0.28 |
| 5 | `[0.90, 1.00]` | 0.10 |

API 原样报告每条线的分数：`AffinitySnapshot.bond` / `.chemistry` 就是真实存储
的合成分，0..1，无显示曲线。「前期容易、后期磨」的节奏是真实的：写侧档位衰减
按线自身档位压制正向增益。想画进度条的前端用分数和上表档界自行推导。

所有档界都是可调常量。

## 档位标签

两套独立的五标签，每线一套（序列化 snake_case）：

| 线 | 档 1 | 档 2 | 档 3 | 档 4 | 档 5 |
|------|--------|--------|--------|--------|--------|
| **Bond** | `acquaintance` | `friend` | `close_friend` | `confidant` | `soulmate` |
| **Chemistry** | `spark` | `flirtation` | `crush` | `lover` | `beloved` |

`bond_label` 与 `chemistry_label` 永远取各自五个值之一。哪条线都还没起步的
关系读作 `acquaintance` + `spark`，两条都是第 1 档——没有单独的「stranger」
状态，4.1 起也没有承载它的遗留标签了。

## 档位序号会落库

每次写入都会把 `tier_index` 自己的结果落到 `companion_affinity.bond_tier` /
`.chem_tier`，这样调不到引擎的 SQL 消费者也能拿到权威档位，而不必照着抄来的
阈值表自己从分数换算。这两列对引擎侧是只写的：引擎代码手上有分数，直接调
`bond_tier()`。

阈值只存在于一处（`tier_index`），所以加一档是改那个函数加一次回填——表的形状
并不编码档位有几个。

## 判官协议：全面 ordinal

**判官在任何地方都不输出连续数值。**四条线轴（`trust` / `intrigue` /
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

- **档位口径：**`0` = 无事发生（寒暄、附和——压倒性常见的裁定）；`1` = 微小但
  真实的波动；`2` = 明确的推进或伤害；`3` = 罕见的重要时刻；`4` = 里程碑
  （极罕见）。
- **方向**为 `"up"` / `"down"`；负面时刻被提示要果断给出。
- **端点档口径：**`1` = 冷淡/不耐烦（明显冷场、敷衍、被冒犯）；`2` = 常态——
  压倒性常见的裁定；`3` = 明显热络/上心。端点档是*本轮状态判读*，不是增量。

模型是可靠的 ordinal 评级者、不可靠的校准算术器——判官选桶，引擎拥有一切数字。
用户看到的连续 `warmth`/`patience` 分布由上面的派生式从离散档折出来；4.0 移除
了最后一处连续输出（旧的 0.1 步进 patience 读数，prod 实测挤在天花板）。

**畸形裁定整份拒收。**无法解析的 JSON、任一畸形轴（非整数或越界档位、未知
方向）、任一畸形端点档，都会让 `parse_affinity_eval` 拒掉整份裁定：档位全零、
无端点读数、reason 置空。当回合的规则增量照常持久化，判官失败不会丢事件。
（缺省轴或 `null` 档位读作档 0；缺省或 `null` 端点档读作「保持存储档位」；
带引号的整数如 `"grade": "2"`、`"warmth": "3"` 可救回。）

**分带输入、端点除外。**每轮 payload 给判官看四条*线轴*的当前分带（低/中/高，
切点 0.35 / 0.65），从不给原始浮点。当前 `warmth`/`patience` 值刻意**不注入**：
绝对档判定的价值正在于无状态——给它看旧值会造成锚定，把这次重设计要移除的
通胀原样带回来。

**口吻与 `reason` 卫生。**判官提示词以角色第一人称书写，不是第三方评审；以
静态 `system` 指令加逐轮 `user` payload 发送。`reason` 规则禁止系统词汇
（AI/助手/模型、拒绝、政策），禁止为触达回复的模板拒答背书。这是承重设计：
`reason` 落 `companion_affinity_events.context` 并作为 `[emotional_context]`
回注后续系统提示。提示词由引擎持有、刻意不可配置——见
`docs/superpowers/specs/2026-08-02-affinity-eval-hygiene-design.md`。

## 写入管线（affinity 4.0）

判官报档位，引擎拥有一切数字。每份裁定经过四个引擎侧阶段（`grade_turn`，
`eros-engine-core/src/affinity.rs`），基于回合前快照计算、在好感度行锁下应用。
端点从不进入这条管线。

```
档位 → 原始分 → 档位衰减 → 跨线惩罚 → 阈值门 → clamp
                                      → 端点派生
```

**1. 换算（按线）。**有符号档位 `g` 按所属线的单位换算：

```
r = g × u_line                        （正向；demo 会话另乘 AFFINITY_DEMO_BOOST）
r = g × u_line × AFFINITY_NEG_FACTOR  （负向）

u_line = AFFINITY_GRADE_UNIT_BOND  （trust / intrigue，  默认 0.0786）
       | AFFINITY_GRADE_UNIT_CHEM  （intimacy / tension，默认 0.0266）
```

两个单位约 3 倍的差距是判官打分不对称性的实测结果（tension 约一半回合达到
档 ≥2，trust 约 80% 回合打 0），写在明面上可供争论，而不是藏进档位重映射。
PDE 规则微调（如长消息 intrigue `+0.02`）在衰减前并入原始分。

**2. 档位衰减（仅正向）。**正向原始分乘所属线的档位因子
`AFFINITY_TIER_DECAY`（默认 `1.0, 0.70, 0.45, 0.25, 0.10` 对应档 1–5）。
`trust`/`intrigue` 读 Bond 的档位；`intimacy`/`tension` 读 Chemistry 的。
负向原始分**从不**衰减——任何档位下损失都是全价。

**3. 跨线惩罚。***对侧*线的高度对这步动作收税——按实际应用的档位成比例，
上限定义为所属线单位的倍数：

```
penalty = κ_line × ((y − y₀)⁺ / (1 − y₀))² × (|g| / 4)
  y      = 对侧线分数
  κ_line = AFFINITY_CROSS_PENALTY_RATIO × u_line   （比率默认 5/6）
  y₀     = AFFINITY_CROSS_PENALTY_START            （默认 0.35）
```

高 Bond 让 Chemistry 更难长，反之亦然；档位 `0` 分文不收——管线对事件收费，
不收租金。忽略规则微调，该项可因式分解：

```
g > 0:  ρ = g·u · (D_k − ratio·φ(y)/4)
g < 0:  ρ = g·u · (λ⁻ + ratio·φ(y)/4)      （负向部分从不衰减）
```

两个括号里既没有 `g` **也没有 `u`**：固定位置上结果不会随档位翻符号，且
盈亏平衡位置 `φ(y*) = 4·D_k/ratio` 对两条线一致、与单位无关——κ 绑定单位，
正是防止按线单位悄悄挪动双高之墙的机制。默认值下只有自身第 5 档存在真实
平衡点（对侧 ≈ `0.761`）；越过之后每个档位都统一净负。

**4. 阈值门。**每条线轴维护一个有符号累加器；本轮真实分并入后，只有
`|累计| ≥ AFFINITY_DELTA_THRESHOLD`（默认 `0` = 每轮都提交）才整体提交，
否则缓存在 `companion_affinity.pending_deltas`。

提交的增量 1:1 应用并 clamp 到 `[0,1]`。之后（若本轮读到）判官档位覆写存储
档位，两个端点按回合后的线值重新派生。

### 调参旋钮

服务端环境变量，逐项回退到默认值：

| 环境变量 | 默认 | 含义 |
|---------|---------|---------|
| `AFFINITY_GRADE_UNIT_BOND` | `0.0786` | trust/intrigue 每档原始分 |
| `AFFINITY_GRADE_UNIT_CHEM` | `0.0266` | intimacy/tension 每档原始分 |
| `AFFINITY_NEG_FACTOR` | `1.5` | 负向原始分的额外乘数——保持「涨得慢、跌得快」 |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | 档 1–5 的正向阻尼（逗号分隔；不是恰好 5 个有限非负值则整表保持默认） |
| `AFFINITY_CROSS_PENALTY_RATIO` | `0.8333` | κ_line = ratio × u_line——平衡点与单位无关 |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | 惩罚坡道起点（y₀） |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | 提交阈值 θ；`0` = 每轮提交 |
| `AFFINITY_DEMO_BOOST` | `1.4` | `metadata.is_demo` 会话判官正分乘数 |
| `AFFINITY_FLOOR_RATIO` | `0.2` | 端点托底 φ；域检查封顶 `0.24`，确保永不改写非冷淡裁定 |
| `AFFINITY_TIME_DECAY_RATE` | `0.02` | 端点缺席衰减（每天） |
| `AFFINITY_TIME_DECAY_FLOOR` | `0.5` | 端点缺席衰减下限 |

每个标量在启动时做域检查——非有限或越域的值保持默认并打警告，环境变量打错字
只会退回默认，不会进入管线。

端点锚点——枢轴 `0.35`（= 第 2 档上界）与 `B_MAX = 1.5`——是**代码常量**而非
旋钮：它们是结构承诺（`⅔ × 1.5 = 1` 的恰好封顶性质），做成环境变量反而允许
拧坏派生依赖的不变量。

## Scope 调向：已退役

Affinity 3.1 的写侧 scope 调向（`ScopeMode`、bond 加成与 chemistry 档位阶梯）
在 4.0 退役。`affinity_scope` 重新回到只管读侧——门控提示注入与
`length_score`，不再触碰写路径。端点派生也绝不能读 scope：B(x) 本来就把线的
一切变化（包括任何调速）传导给端点，派生层若再读 scope，同一请求会沿两条路径
落在同一端点上；且 scope 的 1.0 时代线名相对 2.0+ 的线是交叉的。
`companion_affinity_events.context` 不再携带 `scope_mode` /
`effective_grades`——新行上 `effective_grades` 的缺席就是退役落地的最干净验证。

## 持久化

### 生成列

Migration `0048` 把 `engine.companion_affinity` 上的 `bond`、`chemistry`
重定义为 Postgres `GENERATED ALWAYS … STORED` 列（drop + 重建：Postgres 不能
原地改生成表达式）。DB 在每次插入或更新时从线轴重算，不可能漂移：

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (trust    + intrigue) / 2))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (intimacy + tension)  / 2))) STORED
```

不迁移任何轴数据：合成分在判官实际给过的分数上重定义。由此造成的存量行标签
变动已在设计 spec 中测量并接受（中位两轮即可回位）。

### 端点档位

Migration `0048` 同时新增 `warmth_grade` / `patience_grade`（`SMALLINT NOT
NULL DEFAULT 2`，范围检查 `1..=3`）——权威判官档位——并用档 2 的派生值回填
`warmth`/`patience` 缓存列。新行默认把两个端点放在 ≈ `0.244`（陌生人开局
耐心有限——刻意为之）。

### Pending deltas（阈值门）

`engine.companion_affinity` 上的 `pending_deltas JSONB` 存阈值门尚未放行的
逐轴余额（4.0 起仅线轴；旧行里残留的 `warmth` 键被忽略并自然排空）。`NULL`
读作全零。

### 事件行

每个增量回合向 `engine.companion_affinity_events` 追加一行：

- `deltas` —— 本轮线轴的**原始分**（档位换算加规则微调，衰减前）；这里的
  `warmth`/`patience` 恒为 `0.0`。
- `effective_deltas` —— **实际应用**的逐轴变化，`after − before`。线轴上它
  涵盖档位衰减、惩罚、门控与 clamp；端点上它*就是*本轮派生 delta。
- `context` —— `affinity_reason`、未跑评估时的 `eval_skip_reason`、判官原样
  的有符号 `grades`、门的 `pending_after`，以及 4.0 端点审计：
  `warmth_grade`/`patience_grade`（仅本轮实际读到时）、
  `boost_warmth`/`boost_patience`（当轮生效的 B 值）、`decay_factor`、
  `units`（当轮生效的按线单位）。被收税的回合另有 `cross_penalty_assessed`。

### 逐回合标签变动

`engine.companion_affinity_events` 上的 `label_changes JSONB` 记录引擎权威的
本轮档位迁移：

```
label_changes = {
  bond:      { from: "<tier_key>", to: "<tier_key>" }  // bond 档位变化时
  chemistry: { from: "<tier_key>", to: "<tier_key>" }  // chemistry 档位变化时
}
// 本轮无档位移动时为 NULL
```

## API 表面

### `AffinitySnapshot`

由 `GET /bff/v1/comp/affinity/{session_id}` 返回，读取时会重新刷新
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

- `warmth` / `patience` —— 派生端点值（0–1，4.0 起无负值）。
- `bond` / `chemistry` —— 真实存储的合成分（0–1），无显示曲线。
- `bond_tier` / `chem_tier` —— 1..=5 档位序号，即 `tier_index` 自己的结果。
  会持久化到行上的 `bond_tier` / `chem_tier` 两列供 SQL 消费者使用；客户端读这
  两者之一，不要自己从分数换算。
- `bond_label` / `chemistry_label` —— 上表 10 个档位键之一。

### BFF `/bff/v1/comp/affinity/{session_id}/event`

该端点返回逐回合好感度增量。除
`effective_deltas`（逐轴实际变化，`after − before`——`warmth`/`patience` 上
即本轮派生 delta）外，事件还携带：

- `effective_deltas_computed` —— 精确的本轮 bond/chemistry delta，持久化时
  从前后分数计算并落在事件行上
  （`companion_affinity_events.effective_line_deltas`）。旧行上为 `null`/缺省。
- `label_changes` —— 引擎权威的本轮档位迁移；无档位移动时为 `null`（或缺省）。
- `state_after` —— 本轮结束时的整个向量（六轴、两条线的分数、两个档位序号、
  两个判官档位、ghost 计数、`updated_at`），取自
  `companion_affinity_events.state_after`。migration 0049 之前写入的行上缺省。

三个字段都落在事件行上。对应的 `state_before` 列不在此端点返回——要重放一段
回合请直查 `engine.companion_affinity_events`。

## 源码

- `crates/eros-engine-core/src/affinity.rs` —— 类型、`grade_turn` 写入管线、端点派生、时间衰减、bond/chemistry 分数、档位、标签、diff_labels
- `crates/eros-engine-store/src/affinity.rs` —— `AffinityRepo`（persist_with_event、record_ghost）、migrations 0048–0049
- `crates/eros-engine-server/src/pipeline/post_process.rs` —— LLM 评估、档位解析
- `crates/eros-engine-server/src/prompt.rs` —— 好感度 → 态度指令 + 评估提示词
- `crates/eros-engine-server/src/routes/dto.rs` —— `AffinitySnapshot`（合成分 + 标签）
- `crates/eros-engine-server/src/routes/bff/affinity.rs` —— BFF 好感度表面（value + event）
- 设计 spec：`docs/superpowers/specs/2026-08-16-affinity-40-design.md` —— 线的数学、端点派生、档位
- 设计 spec：`docs/superpowers/specs/2026-08-17-affinity-41-design.md` —— 档位列落库、事件状态快照、绝对值端点
