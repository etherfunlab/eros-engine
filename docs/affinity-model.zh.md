# 好感度模型

[English](affinity-model.md) · [中文](affinity-model.zh.md)

好感度是一个六维向量，会在每轮对话后变化，并折叠成两条衍生线——**Bond**（友情轴）和
**Chemistry**（爱情轴）。每条线都有分层和标签。引擎是分数、标签、以及每轮标签变化的单一权威来源。

## 六个基础轴

| 轴 | 范围 | 默认种子 | 影响什么 |
|------|-------|--------------|----------------|
| `warmth` | −1.0 ↔ 1.0 | `0.1` | 语气、称呼。负值 = 戒备/敌意；正值 = 温暖/亲昵。折叠时对两条线均有贡献（取 0 为下界）。 |
| `trust` | 0.0 ↔ 1.0 | `0.0` | 话题深度，是否愿意暴露自己。Bond 轴。 |
| `intrigue` | 0.0 ↔ 1.0 | `0.0` | 好奇心、追问动力，抗 ghost 的主力。Bond 轴。 |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | 内部梗、昵称、回头呼应之前的细节。Chemistry 轴。 |
| `patience` | 0.0 ↔ 1.0 | `0.5` | 对短消息/敷衍回复的容忍度；ghost 阈值的输入。LLM 每轮给绝对值（0~1，每 0.1 档）+ 规则 delta，直接写入（不走 EMA，仍夹钳到 `[0, 1]`）。不计入两条线。 |
| `tension` | 0.0 ↔ 1.0 | `0.0` | 推拉、玩闹式的小摩擦、傲娇空间。Chemistry 轴。 |

只有 `warmth` 可以为负值。其余五个都限制在 `[0, 1]`。每次更新都会对六个轴做夹钳（clamp）。

**默认种子**值仅对迁移 `0029` 之后创建的新行生效，已有行不受影响。

### EMA 平滑

LLM 评估出的 deltas 会通过指数移动平均应用，避免大幅跳变：

```
new_value = clamp(old_value + (1 − ema_inertia) × delta)
```

默认 `ema_inertia = 0.5`（环境变量 `EMA_INERTIA` 可调）。在该默认值下，delta `+0.5` 在这一轮会移动 `+0.25`。

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

`patience` 不再是纯规则轴。每轮的 `affinity_evaluation`（与其余五轴同一次 LLM 调用，不产生新的往返）中，模型现在会额外给出一个**绝对**的 `patience` 读数（`0.0`–`1.0`，每 `0.1` 一档，代表当前对用户还剩多少耐心，而非变化量）。引擎把模型读数四舍五入到最近的 `0.1` 并夹钳到 `[0, 1]`，记为 `L`。

PDE 仍照常计算这一轮回复/主动消息的规则 delta `R`（`predict_reply_deltas`：长消息 +0.02 / 极短消息 −0.02 / 超过 24 小时未活动 −0.05）——这部分不变。

本轮目标值为 `patience_target = clamp(L + R, 0, 1)`；该和值**不会**再被四舍五入回 0.1 档（网格只约束 LLM 读数，`R` 可以把结果推离网格）。±0.4/−0.6 非对称上限在更早的 `parse_affinity_eval` 中就已作用于五个 LLM delta 轴——patience 从不受这两项上限约束。持久化时，`apply_deltas` 照常先跑一遍（六轴统一走 EMA 平滑 + `[0, 1]` 夹钳），随后 patience 被 `patience_target` **直接覆盖**——绕过 EMA 平滑（仍会夹钳到 `[0, 1]`）。因为 `L` 和 `R` 都与当前存储值无关，这个写入在并发场景下是安全的，不需要读改写。

**兜底：** 当本轮没有 LLM patience 读数时——Proactive、用户消息过短、助手回复为空、`no_persona_or_affinity`（persona 加载失败或不存在好感度行）、或评估调用报错/超时/模型省略了 `patience` 字段——`patience_target` 为 `None`，patience 走**旧路径**：把 `R` 通过 EMA 叠加并夹钳。

**Ghost 走独立路径，不是兜底。** Ghost 回合根本不会进入 `persist_with_event`——`persist_affinity` 把它分派给 `record_ghost`，该函数不接收任何 delta、从不跑 `apply_deltas`/EMA，只递增 `ghost_streak` / `total_ghosts` / `last_ghost_at`（写入的是全零 `effective_deltas`）。PDE 的 `ghost_affinity_deltas()`（patience `−0.05`、tension `+0.05`——与 `predict_reply_deltas` 是不同的函数）会被计算进 `ActionPlan`，但在持久化时被丢弃。因此 Ghost 回合的 `patience` 既不会被任何 delta 移动，也不会经过 EMA——只有 ghost 计数器会变化。

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

## 分档与进度条曲线

每条线有**五档**，分档的原始分数区间逐档拉宽（越往上越难），直到顶端一个窄小的第 5 档：

| 档位 | 原始分数区间 | 区间宽度 |
|------|-----------|-----|
| 1 | `[0.00, 0.15)` | 0.15 |
| 2 | `[0.15, 0.35)` | 0.20 |
| 3 | `[0.35, 0.62)` | 0.27 |
| 4 | `[0.62, 0.90)` | 0.28 |
| 5 | `[0.90, 1.00]` | 0.10 |

**进度条值（0–1，由前端渲染）：** 第 1–4 档分别占进度条的 25% / 25% / 25% / 20%，第 5 档占顶端 5%，档内线性：

```
bar(raw) = band_lo(档位) + (raw − tier_lo) / (tier_hi − tier_lo) × band_width(档位)
  第 1 档: 0.00 + (raw − 0.00) / 0.15 × 0.25  →  [0.00, 0.25)
  第 2 档: 0.25 + (raw − 0.15) / 0.20 × 0.25  →  [0.25, 0.50)
  第 3 档: 0.50 + (raw − 0.35) / 0.27 × 0.25  →  [0.50, 0.75)
  第 4 档: 0.75 + (raw − 0.62) / 0.28 × 0.20  →  [0.75, 0.95)
  第 5 档: 0.95 + (raw − 0.90) / 0.10 × 0.05  →  [0.95, 1.00]
夹钳到 [0, 1]
```

由于高档跨越更大的原始好感度区间，进度条前期填充很快，接近 100% 时会放慢——前两档简单、后两档是磨练——无需字面的 `exp()`。固定的每轮原始 delta 在高档时对进度条的推动也*更小*。第 5 档是刻意收窄的 5% 顶档，让「满级」显得稀有，同时仍保留足够空间，使进度条能在其 0.10 的原始分跨度内继续移动（避免 lv4→lv5 的 damping）。

所有阈值和区间均为可调常量。

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

## 评估分布与非对称上限

**上限（非对称）。** 评估器的每轴原始输出在 `parse_affinity_eval` 中被非对称夹钳：

```
POS_CAP = +0.4    NEG_CAP = −0.6
effective_delta = raw.clamp(NEG_CAP, POS_CAP)
```

以 EMA blend 0.5（`ema_inertia = 0.5`）计，每轮单轴最大值为 **+0.2**（增益）和 **−0.3**（损失）——非对称上限让一次糟糕的回合比一次好的回合影响更大。

**分布（通过 prompt 塑造）。** 评估器会被引导成以下输出形态：

- **绝大多数轮次：恰好 `0`** —— 普通闲聊和应答得分为零。
- **罕见正值** —— 仅在真正推动关系的时刻（真实的温暖、自我披露、脆弱坦露、成功的调情）才产生；可以较大（单轴最高约 +0.4）。
- **更易触发的负值** —— 冷漠、敷衍/重复的回复、无聊、越界、冲突、被无视均会触发；可以更大（单轴最低约 −0.6）。

EMA 平滑和时间衰退**不变**——只有上限和 prompt 指引发生了变化。

## 持久化

### 生成列

迁移 `0029` 在 `engine.companion_affinity` 上新增 `bond` 和 `chemistry` 两个 Postgres `GENERATED ALWAYS … STORED` 列。DB 在每次行插入或更新时从六轴重新计算它们，因此它们不会漂移。已有行会在迁移时自动填充（无需回填，引擎写路径无需改动）：

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + trust    + intrigue) / 3))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + intimacy + tension)  / 3))) STORED
```

进度条曲线和分档标签仅存在于核心读层。DB 存储的原始合成值与 API 层面的进度条值是不同的概念。

### 降低的默认种子

新行的列默认值（同样在迁移 `0029` 中）被设置为使新会话的 bond ≈ chemistry ≈ 0.033——两条线均在第 1 档，遗留标签为 `stranger`。已有行不受影响。

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
  "bond": 0.32,
  "chemistry": 0.28,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` —— 进度条值（0–1，曲线映射后），非原始合成值。
- `bond_label` / `chemistry_label` —— 上述 10 个档位键之一。
- `relationship_label` —— 遗留映射值（`stranger / friend / slow_burn / romantic`）。

### BFF `/bff/v1/comp/affinity/{session_id}/event`

此接口返回每轮好感度 delta，不受 `EXPOSE_AFFINITY_DEBUG` 控制。除现有的 `effective_deltas`（每轴 EMA 后）外，事件现还包含：

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

- `effective_deltas_computed` —— 本轮精确的 bond/chemistry 行增量，在持久化时从取下界前后的分数计算得出，存储于事件行（`companion_affinity_events.effective_line_deltas`）。取值单位为原始合成增量（非进度条百分比），适合每轮"+X bond / +Y chemistry"的脉冲显示。迁移前的旧行此字段为 `null` / 缺省。
- `label_changes` —— 本轮引擎权威的档位变化；无档位变化时为 `null`（或缺省）。前端无需自行计算变化，直接消费此字段。

两个字段同样镜像到调试接口 `GET /comp/affinity/{session_id}/event` 的条目上。

## 源码

- `crates/eros-engine-core/src/affinity.rs` —— 类型、EMA、时间衰退、bond/chemistry 分数、分档、进度条、标签、diff_labels
- `crates/eros-engine-store/src/affinity.rs` —— `AffinityRepo`（persist_with_event、record_ghost），迁移 0029
- `crates/eros-engine-server/src/pipeline/post_process.rs` —— LLM 评估，非对称夹钳
- `crates/eros-engine-server/src/prompt.rs` —— 好感度 → 态度指令 + 评估 prompt
- `crates/eros-engine-server/src/routes/dto.rs` —— `AffinitySnapshot`（进度条 + 标签）
- `crates/eros-engine-server/src/routes/bff/affinity.rs` —— BFF 事件接口
- `crates/eros-engine-server/src/routes/debug.rs` —— 调试事件日志
