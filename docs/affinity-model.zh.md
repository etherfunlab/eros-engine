# 好感度模型

[English](affinity-model.md) · [中文](affinity-model.zh.md)

好感度是一个六维向量，会在每个文本频道、非 `product_qa` 的对话轮次后变化，并折叠成两条衍生线——**Bond**（友情轴）和
**Chemistry**（爱情轴）。语音频道和 `product_qa` 轮次从不写入好感度事件。每条线都有分层和标签。引擎是分数、标签、以及每轮标签变化的单一权威来源。

## 六个基础轴

| 轴 | 范围 | 默认种子 | 影响什么 |
|------|-------|--------------|----------------|
| `warmth` | −1.0 ↔ 1.0 | `0.1` | 语气、称呼。负值 = 戒备/敌意；正值 = 温暖/亲昵。折叠时对两条线均有贡献（取 0 为下界）。 |
| `trust` | 0.0 ↔ 1.0 | `0.0` | 话题深度，是否愿意暴露自己。Bond 轴。 |
| `intrigue` | 0.0 ↔ 1.0 | `0.0` | 好奇心、追问动力，抗 ghost 的主力。Bond 轴。 |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | 内部梗、昵称、回头呼应之前的细节。Chemistry 轴。 |
| `patience` | 0.0 ↔ 1.0 | `0.5` | 对短消息/敷衍回复的容忍度；ghost 阈值的输入。当该轮 LLM 给出绝对值读数时（0~1，每 0.1 档），与规则 delta 合并后直接写入；没有读数时规则 delta 单独按 1:1 应用（见下文）。始终夹钳到 `[0, 1]`。不计入两条线。 |
| `tension` | 0.0 ↔ 1.0 | `0.0` | 推拉、玩闹式的小摩擦、傲娇空间。Chemistry 轴。 |

只有 `warmth` 可以为负值。其余五个都限制在 `[0, 1]`。每次更新都会对六个轴做夹钳（clamp）。

**默认种子**值仅对迁移 `0029` 之后创建的新行生效，已有行不受影响。

### 按档位写入

评估器报告的是每轴**档位（grade）**而非数字 delta；引擎把档位换算成分数、做衰减与门控（见[写入管线](#写入管线好感度-30)），再把提交的 delta 按 1:1 应用：

```
new_value = clamp(old_value + committed_delta)
```

提交的 delta 就是字面意义上的变化量——阻尼由管线里的分档衰减负责，作用在写入之前而不是写入之上。以 `metadata.is_demo` 开启的会话会把正向评分乘以 `AFFINITY_DEMO_BOOST`（默认 `1.4`），让 demo 场次的好感度表在短暂的轮次预算内有可见的移动。

### 时间衰退

六个轴中有三个会在没有活动时随真实时间漂移。衰退采用懒计算，每次加载时根据 `updated_at` 计算：

```
days_elapsed = (now − updated_at) / 1 天

intrigue = clamp(intrigue − 0.01  × days_elapsed, 0.0, 1.0)
patience = clamp(patience + 0.005 × days_elapsed, 0.0, 1.0)
tension  = clamp(tension  − 0.005 × days_elapsed, 0.0, 1.0)
```

`warmth`、`trust`、`intimacy` 不衰退——它们是"深层"维度。

### Patience：LLM 绝对读数 + 规则 delta

`patience` 不是档位轴。每轮的 `affinity_evaluation`（与其余五轴同一次 LLM 调用，不产生新的往返）中，模型会额外给出一个**绝对**的 `patience` 读数（`0.0`–`1.0`，每 `0.1` 一档，代表当前对用户还剩多少耐心，而非变化量）。引擎把模型读数四舍五入到最近的 `0.1` 并夹钳到 `[0, 1]`，记为 `L`。

PDE 仍照常计算这一轮回复/主动消息的规则 delta `R`（`predict_reply_deltas`：长消息 +0.02 / 极短消息 −0.02 / 超过 24 小时未活动 −0.05）——这部分不变。

本轮目标值为 `patience_target = clamp(L + R, 0, 1)`；该和值**不会**再被四舍五入回 0.1 档（网格只约束 LLM 读数，`R` 可以把结果推离网格）。持久化时，写入管线照常先跑一遍——但 `patience` 从管线中原样穿过：它的规则 delta 从不参与分档衰减、跨线惩罚或阈值门控，按 1:1 应用并夹钳。随后 patience 被 `patience_target` **直接覆盖**（仍会夹钳到 `[0, 1]`）。因为 `L` 和 `R` 都与当前存储值无关，这个写入在并发场景下是安全的，不需要读改写。

**兜底：** 当本轮没有 LLM patience 读数时——Proactive、用户消息过短、助手回复为空、`no_persona_or_affinity`（persona 加载失败或不存在好感度行）、或评估调用报错/超时/模型省略了 `patience` 字段——`patience_target` 为 `None`，只应用规则 delta `R`（1:1，夹钳）。

**Ghost 走独立路径，不是兜底。** Ghost 回合根本不会进入 `persist_with_event`——`persist_affinity` 把它分派给 `record_ghost`，该函数不接收任何档位或 delta、从不跑写入管线，只递增 `ghost_streak` / `total_ghosts` / `last_ghost_at`（写入的是全零 `effective_deltas`）。PDE 的 `ghost_affinity_deltas()`（patience `−0.05`、tension `+0.05`——与 `predict_reply_deltas` 是不同的函数）会被计算进 `ActionPlan`，但在持久化时被丢弃。因此 Ghost 回合的 `patience` 完全不动——只有 ghost 计数器会变化。

## 两条衍生线

六个轴会生成两个合成分数。`warm_pos` 是 `warmth.max(0.0)` —— 以 0 为下界，而不是整体平移；因此中性或冷漠的会话贡献为零：

```
bond      = (warm_pos + trust   + intrigue) / 3    ∈ [0, 1]
chemistry = (warm_pos + intimacy + tension)  / 3    ∈ [0, 1]
```

`warmth` 会进入两条线：冷漠的回复会同时拉低 Bond 和 Chemistry。
`patience` 不计入任何一条线——它由 LLM 绝对读数 + 规则 delta 维护，直接写入；两条线仍不含 patience（设计如此）。

以默认种子（`warmth 0.1`，`trust/intrigue/tension 0`）开始，新会话的
bond ≈ chemistry ≈ 0.033——两条线均在第 1 档（陌生人）。

> **命名注意：** `AffinityScope::bond()/chemistry()`（用于 prompt 注入范围控制、`length_score`）采用的是*不同的*轴分组——那是一套更早的独立划分，为避免回复长度的回归而有意保留。此处的 `bond_score`/`chemistry_score` 与其完全独立。

## 分档

每条线有**五档**，分档的分数区间逐档拉宽（越往上越难），直到顶端一个窄小的第 5 档：

| 档位 | 分数区间 | 区间宽度 |
|------|-----------|-----|
| 1 | `[0.00, 0.15)` | 0.15 |
| 2 | `[0.15, 0.35)` | 0.20 |
| 3 | `[0.35, 0.62)` | 0.27 |
| 4 | `[0.62, 0.90)` | 0.28 |
| 5 | `[0.90, 1.00]` | 0.10 |

API 按原样返回每条线的分数：`AffinitySnapshot.bond` / `.chemistry` 是真实存储的合成值（0..1），不再套任何显示曲线（好感度 3.0 删掉了旧的分档进度条投影）。投影曾经在渲染层伪造的「前期快、后期磨」节奏现在是真实的：写侧的分档衰减（见[写入管线](#写入管线好感度-30)）按本线档位削减正向增益，高档确实要更多轮次才能跨越。前端要做分档进度条，可用分数和上表的档位边界自行推导。

所有分档阈值均为可调常量。

## 分档标签

共有两组各五个标签，每条线一组（序列化为蛇形命名键）：

| 线 | 第 1 档 | 第 2 档 | 第 3 档 | 第 4 档 | 第 5 档 |
|------|--------|--------|--------|--------|--------|
| **Bond** | `acquaintance`（点头之交） | `friend`（朋友） | `close_friend`（好友） | `confidant`（知己） | `soulmate`（灵魂挚友） |
| **Chemistry** | `spark`（来电） | `flirtation`（暧昧） | `crush`（心动） | `lover`（恋人） | `beloved`（至爱） |

`bond_label` 和 `chemistry_label` 始终是各自五个值之一——永不输出 `stranger`。`stranger` 状态仅由遗留字段传达（见下文）。

## 遗留 `relationship_label`

遗留字段保留旧名称集，保持对现有消费者的向后兼容。它现在是两个原始分数的纯函数（取代旧的临时 `infer_label` 启发式）：

```
legacy_relationship_label(bond, chemistry):
  if tier(bond) == 1 AND tier(chemistry) == 1  →  stranger
  let higher = (chemistry > bond) ? Chemistry : Bond   // 平局 → Bond
  match higher:
    Bond                                         →  friend
    Chemistry if tier(chemistry) in {1, 2}       →  slow_burn
    Chemistry if tier(chemistry) in {3, 4, 5}    →  romantic
```

`frenemy` 已停止输出，但在枚举中仍可解析，供历史行使用。`stranger` 现在是明确的"两条线均在第 1 档"情况——不再需要旧五个阈值条件全部未命中。

## 评估器协议：档位，不是数字

**档位（好感度 3.0）。** 评估器从不输出数字 delta。五个档位轴（`warmth` / `trust` / `intrigue` / `intimacy` / `tension`）每轴报告一个 `0`–`4` 的整数**档位（grade）**加一个**方向（direction）**：

```json
{
  "warmth":   {"grade": 0, "direction": "up"},
  "trust":    {"grade": 1, "direction": "up"},
  "intrigue": {"grade": 0, "direction": "up"},
  "intimacy": {"grade": 0, "direction": "up"},
  "tension":  {"grade": 2, "direction": "down"},
  "patience": 0.5,
  "reason": "…"
}
```

- **档位口径：** `0` = 无事发生（寒暄、附和——绝大多数轮次的裁决）；`1` = 微小但真实的波动；`2` = 明确的推进或伤害；`3` = 罕见的重要时刻（真诚的自我袒露、脆弱、成功的调情；明显的冒犯或被无视）；`4` = 里程碑——这段关系被重新定义的一轮（极罕见）。
- **方向**为 `"up"` / `"down"`；负面时刻（冷漠、敷衍/重复的回复、无聊、越界、冲突、被无视）在 prompt 中被引导为更常见也更该出手。
- `patience` 仍是 0~1 的绝对读数、每 0.1 一档（见上文），不是档位。

引擎把 `{grade, direction}` 折叠成 `−4..+4` 的有符号整数。模型做序数评级可靠、做校准算术不可靠——所以判官选档位，数字全部由引擎持有。

**畸形裁决整体拒绝。** JSON 不可解析，或任一轴畸形——非整数或越界的档位、未知的方向——都会让 `parse_affinity_eval` 拒绝整份裁决：档位全零、无 patience 读数、reason 为空。该轮的规则 delta 仍会持久化，评估器失败从不丢失好感度事件。（缺省的轴或 `null` 档位不算畸形——按档位 0 处理；`"grade": "2"` 这种带引号的整数会被抢救回来。）

**单轮包络。** 按默认调参，档位 `+4` 换算为单轴 `+0.20`、档位 `−4` 为 `−0.30`。「一次糟糕的回合比一次好的回合影响更大」的非对称性由 `AFFINITY_NEG_FACTOR` 承担。

**档位化输入。** 每轮 payload 给评估器看的六轴当前状态是粗档位（冷/低/中/高，
切点 0.35 / 0.65 与耐心档一致；冷 = warmth 为负），从不给裸浮点——报档位的判官
不该看见数字，否则会被重新锚定回档位协议刚移除的算术上。

**语域与 `reason` 卫生规则。** 评估器 prompt 用角色的第一人称写（「你就是这个角色，
这一轮之后你对他的感觉变了多少」），不是第三人称分析型评审；调用时拆成静态 `system`
指令 + 每轮 `user` 数据两条消息。`reason` 规则禁止出现系统词汇（AI／助手／模型、拒绝
机制、政策等），也禁止为回复里出现的套话式拒绝辩护。这条规则是有承重作用的：`reason`
会写入 `companion_affinity_events.context`，并作为 `[emotional_context]` 重新注入后续
系统提示——评估器若为一次拒绝找理由，那个立场就被写进了角色的持久状态。该 prompt 由
引擎持有，刻意不可配置，见
`docs/superpowers/specs/2026-08-02-affinity-eval-hygiene-design.md`。

## 写入管线（好感度 3.0）

判官报档位，数字全部由引擎持有。每份裁决在引擎侧经过四个阶段（`eros-engine-core/src/affinity.rs` 的 `grade_turn`），对回合前快照计算、在好感度行锁下应用：

```
档位 → 原始分 → 分档衰减 → 跨线惩罚 → 阈值门控 → 夹钳
```

**1. 换算。** 有符号档位 `g` 换算成原始分 `r`：

```
r = g × AFFINITY_GRADE_UNIT                        （正向；demo 会话另乘 AFFINITY_DEMO_BOOST）
r = g × AFFINITY_GRADE_UNIT × AFFINITY_NEG_FACTOR  （负向）
```

默认值 `0.05` / `1.5`：档位 `+4` = `+0.20`、`−4` = `−0.30`——即 2.0 的包络。PDE 的规则微调（如长消息 intrigue `+0.02`）在衰减前并入原始分。

**2. 分档衰减（仅正向）。** 正向原始分乘以本线档位对应的系数 `AFFINITY_TIER_DECAY`（默认第 1–5 档为 `1.0, 0.70, 0.45, 0.25, 0.10`）。`trust`/`intrigue` 读 Bond 的档位；`intimacy`/`tension` 读 Chemistry 的；`warmth`（两条线共享）读较高一线的档位（两者取 max）。负向原始分**从不**衰减——损失在任何档位都是全价。读侧进度条投影删除后，「前期快、后期磨」的节奏就落在这里。

**3. 跨线惩罚。** 在只属于一条线的轴上，*另一条*线的高度会对这次移动收税，**且按实际生效的档位按比例收**：

```
penalty = κ × ((y − y₀)⁺ / (1 − y₀))² × (|g| / 4)
  y  = 另一条线的分数
  g  = 实际生效的档位（经 3.1 scope 转向之后）
  κ  = AFFINITY_CROSS_PENALTY        （默认 0.05）
  y₀ = AFFINITY_CROSS_PENALTY_START  （默认 0.35）
```

高 Bond 让 Chemistry 更难涨，反之亦然。`warmth` 豁免（它同时供给两条线）；档位为 `0` 时不收税——纯规则轮次与被 3.1 门槛滤掉的档位都落在这里。管线只对事件收费，不收租金。

`|g|/4` 这个因子的作用，是让结果不再取决于裁决**有多大**。忽略规则微调，整项可以因式分解：

```
g > 0:  ρ = g × (D_k·u − κ·φ(y)/4)
g < 0:  ρ = g × (u·λ⁻ + κ·φ(y)/4)      （负向部分从不衰减）
```

两个括号里都没有 `g`，所以**在同一个位置上，结果不会随档位改变符号**——档位决定幅度，不决定方向。这正是定额收税的失效模式：它让符号在 g1 与 g4 之间某处翻转，于是一次诚实的小推进反而掉分、更大的推进才涨分。负向那个括号恒为正，所以负向裁决总是让轴下降。

这**不是**在保证「任何正向裁决都是净涨」。正向括号本身是不是正的，取决于**位置**，收支平衡点在 `φ(y*) = 4·D_k·u/κ`——见下。

默认参数下这个平衡点在第 1–4 档都 > 1，也就是**这四档在任何档位下都收不成墙**。只有自身第 5 档有真正的平衡点，落在对手线 ≈`0.761`——越过它之后**每一个**档位都净亏，且是齐平地亏。「不能既是挚友又是恋人」在顶点依然成立，只是不再对普通的中段回合开火。

规则微调在这个因式分解之外：它在衰减前并入原始分，却不计入惩罚所用的档位，所以足够大的反向微调理论上能把符号翻过来。今天做不到——能落到档位轴上的规则微调只有 `intrigue +0.02` 与 `tension +0.03`，两个都是正的。

**4. 阈值门控。** 每轴维护一个有符号累加器。本轮的实际分并入后，只有当 `|累计值| ≥ AFFINITY_DELTA_THRESHOLD`（默认 `0` = 每轮都提交）时整笔余额才提交；未达阈值时缓存在 `companion_affinity.pending_deltas`（JSONB，迁移 `0043`），该轴本轮不动。

提交的 delta 随后按 1:1 应用并夹钳到各轴区间（`warmth` 为 `[-1,1]`，其余为 `[0,1]`）。`patience` 绕过全部四个阶段——它的规则 delta 原样穿过，绝对值覆盖在其后进行（见上文）。

### 调参旋钮

服务端环境变量，逐项回退到默认值；默认值复现 2.0 的每轮有效包络：

| 环境变量 | 默认值 | 含义 |
|---------|---------|---------|
| `AFFINITY_GRADE_UNIT` | `0.05` | 每档位对应的原始分 |
| `AFFINITY_NEG_FACTOR` | `1.5` | 负向原始分的附加乘数——延续「涨得慢、跌得快」 |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | 第 1–5 档的正向衰减系数（逗号分隔；不是恰好 5 个有限非负值时整表保持默认——大于 1 即放大，属合法调参方向） |
| `AFFINITY_CROSS_PENALTY` | `0.05` | 跨线惩罚上限 κ |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | 惩罚开始生效的另一线分数（y₀） |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | 提交阈值 θ；`0` = 每轮都提交 |
| `AFFINITY_DEMO_BOOST` | `1.4` | `metadata.is_demo` 会话对判官正向原始分的乘数（规则微调不受影响） |
| `AFFINITY_SCOPE_BOND_BOOST` | `1.5` | 仅点名 bond 半边的 scope 下，bond 独占轴正向原始分的常数乘数（见下）；`1.0` 即关闭 |
| `AFFINITY_SCOPE_CHEM_LADDER` | `0,0,1,3,4` | 触及 chemistry 半边的 scope 下，chemistry 独占轴的档位门槛表，按正向档位 0–4 索引（逗号分隔；须恰好 5 个 `0..=4` 的值且第 0 格为 `0`，否则整表保持默认）；`0,1,2,3,4` 即关闭 |

每个标量在启动时做域校验——非有限或越域的值（负的单位/系数/惩罚/阈值/乘数、
不在 `[0, 1)` 内的起罚点）保持默认并记录警告：环境变量打错字只会退回默认值，
不会进入管线。

## Scope 转向（好感度 3.1）

请求里的 [`affinity_scope`](api-reference.zh.md#post-compchatsession_idmessagestream)
做两件事。除了决定哪些轴进入 prompt，它还决定本轮档位怎么折算 —— 同一个「关系取向」
既决定伴侣**被告知**什么，也决定这段关系**挣到**什么。

这里借用的是它两个具名值所携带的 bond/chemistry **心智**，不是它们背后的轴三元组。
`ScopeMode` 对六个 bool 是全函数，所以轴数组的转向和具名值一样可预期：

| 解析后的 scope | 模式 | 效果 |
|---|---|---|
| 空（`none`） | `neutral` | 与 3.0 逐格相同 |
| 含任一 chemistry 半边轴 —— `chemistry`、`full`、混合数组 | `suppress_chemistry` | `intimacy` / `tension` 的正向档位过 `AFFINITY_SCOPE_CHEM_LADDER` |
| 其余 —— `bond`、bond 半边数组 | `boost_bond` | `trust` / `intrigue` 的正向原始分 × `AFFINITY_SCOPE_BOND_BOOST` |

三条性质由构造保证：

- **共享的 `warmth` 两个方向都豁免。** 它同时喂两条合成线，缩放它会把修正泄漏到
  另一条线上 —— 与跨线惩罚豁免 warmth 是同一个理由。只有线独占轴被转向。
- **损失从不被转向。** 负向原始分照付 `AFFINITY_NEG_FACTOR`，此外什么都不加：
  这个修正抬高门槛，不减轻伤害。
- **被门槛滤掉的档位不计跨线惩罚。** 门槛在管线**之前**生效，映射到 `0` 的档位
  读作「判官没有触碰这条轴」，而惩罚只对被触碰的轴收取，于是根本不触发。
  这正是默认门槛表不引入任何 3.0 没有的失衡格的原因：它的 `g3`/`g4` 两行与未转向时
  逐格相同，`g2` 恰好落在未转向的 `g1` 行上。

默认门槛表 `0,0,1,3,4` 滤掉 `g1`、把 `g2` 减半、里程碑原样 ——
即「闲聊不再算作浪漫，真正的时刻照算」。

**审计。** `companion_affinity_events.context` 每轮都记 `scope_mode`，
门槛实际改动了什么时另记 `effective_grades`。`grades` 保持判官原始verdict，
所以一个提交为 `0` 的值始终可归因：是判官什么都没说，还是被门槛滤掉了。
没有这一对，跨模型轮换观察判官漂移时会把引擎自己的修正读成模型的移动。
被计入跨线税的回合另记 `cross_penalty_assessed`——惩罚改为按档位比例收之后，
一个回合到底被收了多少，已经无法只凭档位与存储分数反推。
是**计入**不是**落地**：它在阈值门控之前从 `ρ` 里扣除，
所以被门控缓存的回合上它还没碰到轴，而是留在 `pending_after` 里。

## 持久化

### 生成列

迁移 `0029` 在 `engine.companion_affinity` 上新增 `bond` 和 `chemistry` 两个 Postgres `GENERATED ALWAYS … STORED` 列。DB 在每次行插入或更新时从六轴重新计算它们，因此它们不会漂移。已有行会在迁移时自动填充（无需回填，引擎写路径无需改动）：

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + trust    + intrigue) / 3))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + intimacy + tension)  / 3))) STORED
```

分档标签仅存在于核心读层；API 直接返回存储的合成值本身——不存在独立的显示值。

### 降低的默认种子

新行的列默认值（同样在迁移 `0029` 中）被设置为使新会话的 bond ≈ chemistry ≈ 0.033——两条线均在第 1 档，遗留标签为 `stranger`。已有行不受影响。

### 待提交余额（阈值门控）

迁移 `0043` 在 `engine.companion_affinity` 上新增 `pending_deltas JSONB`——阈值门控尚未放行的每轴余额。只由带档位的消息路径写入；`NULL`（所有 3.0 之前的旧行，以及从未被门控的行）等同于全零。

### 事件行

每个 delta 轮次向 `engine.companion_affinity_events` 追加一行：

- `deltas` —— 本轮的**原始分**：档位换算（含 demo boost）加规则微调，衰减前。
- `effective_deltas` —— **实际应用**的每轴变化，`after − before`。它涵盖分档衰减、跨线惩罚、门控、patience 覆盖与夹钳；被门控缓存的轮次为全零。
- `context` —— `affinity_reason`（评估器的 `reason`）、未跑评估时的 `eval_skip_reason`、判官的有符号 `grades` 原样，以及门控的 `pending_after` 余额。

### 每轮标签变化

迁移 `0029` 还在 `engine.companion_affinity_events` 上新增了 `label_changes JSONB` 列。每轮之后，引擎会对比 delta 前后的档位，范围限定在与 `effective_deltas` 相同的衰退窗口内：

```
label_changes = {
  bond:      { from: "<档位键>", to: "<档位键>" }  // 若 bond 档位发生变化
  chemistry: { from: "<档位键>", to: "<档位键>" }  // 若 chemistry 档位发生变化
}
// 本轮无档位变化时为 NULL
```

`from`/`to` 是档位键（如 `"acquaintance"`、`"friend"`）。遗留 `relationship_label` 的变化不包含在内，因为它可由快照推导。纯衰退导致的档位漂移不记录为离散事件；绝对快照始终可通过快照端点获取。

## API 接口

### `AffinitySnapshot`

由 `GET /comp/affinity/{session_id}`（调试，受 `EXPOSE_AFFINITY_DEBUG` 控制）返回。快照包含：

```json
{
  "warmth": 0.42,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.55,
  "tension": 0.04,
  "bond": 0.21,
  "chemistry": 0.17,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` —— 真实存储的合成分数（0–1）；不套任何显示曲线。
- `bond_label` / `chemistry_label` —— 上述 10 个档位键之一。
- `relationship_label` —— 遗留映射值（`stranger / friend / slow_burn / romantic`）。

### BFF `/bff/v1/comp/affinity/{session_id}/event`

此接口返回每轮好感度 delta，不受 `EXPOSE_AFFINITY_DEBUG` 控制。除现有的 `effective_deltas`（每轴实际应用的变化，`after − before`）外，事件现还包含：

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.06, "trust": 0.02, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.0, "tension": -0.02
    },
    "effective_deltas_computed": {
      "bond": 0.027,
      "chemistry": 0.013
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "created_at": "…"
  }
}
```

- `effective_deltas_computed` —— 本轮精确的 bond/chemistry 行增量，在持久化时从取下界前后的分数计算得出，存储于事件行（`companion_affinity_events.effective_line_deltas`）。取值单位为合成分增量——与快照的 `bond`/`chemistry` 同一 0..1 刻度——适合每轮"+X bond / +Y chemistry"的脉冲显示。迁移前的旧行此字段为 `null` / 缺省。
- `label_changes` —— 本轮引擎权威的档位变化；无档位变化时为 `null`（或缺省）。前端无需自行计算变化，直接消费此字段。

两个字段同样镜像到调试接口 `GET /comp/affinity/{session_id}/event` 的条目上。

## 源码

- `crates/eros-engine-core/src/affinity.rs` —— 类型、`grade_turn` 写入管线、时间衰退、bond/chemistry 分数、分档、标签、diff_labels
- `crates/eros-engine-store/src/affinity.rs` —— `AffinityRepo`（persist_with_event、record_ghost），迁移 0029/0043
- `crates/eros-engine-server/src/pipeline/post_process.rs` —— LLM 评估，档位解析
- `crates/eros-engine-server/src/prompt.rs` —— 好感度 → 态度指令 + 评估 prompt
- `crates/eros-engine-server/src/routes/dto.rs` —— `AffinitySnapshot`（合成分数 + 标签）
- `crates/eros-engine-server/src/routes/bff/affinity.rs` —— BFF 事件接口
- `crates/eros-engine-server/src/routes/debug.rs` —— 调试事件日志
