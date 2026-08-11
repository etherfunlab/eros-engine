# Ghost 機制

[English](ghost-mechanics.md) · [中文](ghost-mechanics.zh.md)

人格決定 **不** 在這一輪回覆。默认情况下决策由规则引擎完成，不调用 LLM。配置了可选的 LLM PDE 判断器（`[tasks.pde_decision].filter_prompt`）之后，改由判断器提出本轮动作；下面这套打分仍然作为回退，并保留一道判断器无法推翻的硬安全否决。讓對話感覺像在跟一個有自己狀態的人說話，這個機制單獨做的工作最多。

## 為甚麼 ghost 重要

大多數 LLM 對話 UI 對甚麼都回。這會把用戶訓練成低成本發消息——反正無代價。eros-engine 的人格有有限的耐性、有限的好奇心，建模在好感度向量裡，兩個都低的時候就閉嘴。這個沉默同時做兩件事：

1. 推用戶把更多東西放進來（真正的對話，不是對著機械人速記）。
2. 讓關係感覺有質感——你會被 ghost，意味著你也可以贏回回覆。

## 評分公式

```
ghost_score = (1 − intrigue) × 0.4
            + (1 − patience) × 0.4
            + tension       × 0.2
```

- 高分＝人格無聊、煩躁、或處於摩擦期。傾向 ghost。
- 分數在 `[0, 1]` 範圍內。

`intrigue` 跟 `patience` 權重相等（各 0.4）；`tension` 是個小修正（0.2）。實現：

```rust
// crates/eros-engine-core/src/ghost.rs
pub fn score(a: &Affinity) -> f64 {
    (1.0 - a.intrigue) * 0.4 + (1.0 - a.patience) * 0.4 + a.tension * 0.2
}
```

## 四層保護

光靠分數不能直接決定。四條規則按優先級在閾值檢查之前先跑：

```
1. message_count < 10            → 永遠不 ghost
                                    （關係還幼嫩）

2. ghost_streak ≥ 2              → 不連 ghost 兩次
                                    （避免「她走了」的崖式體感）

3. last_ghost < 1 小時前          → 冷靜期
                                    （剛 ghost 過你，緩一緩）

4. 否則：
     基礎閾值          = 0.65
     若本会话 ghost 过：
       閾值 = 0.85               （门槛升高后，整段会话不再回落）
     score > 閾值 才 ghost
```

實現：

```rust
// 规则 1-3 单独成一个函数：LLM PDE 路径把它当否决用，不带分数阈值
// （是否值得 ghost 由判断器自己定）。
pub fn ghost_permitted(a: &Affinity, s: GhostSignals) -> bool {
    if s.message_count < 10 { return false; }
    if a.ghost_streak >= 2 { return false; }
    if matches!(s.hours_since_last_ghost, Some(h) if h < 1.0) { return false; }
    true
}

pub fn decide(a: &Affinity, s: GhostSignals) -> GhostDecision {
    if !ghost_permitted(a, s) { return GhostDecision::Reply; }
    let threshold = if s.hours_since_last_ghost.is_some() { 0.85 } else { 0.65 };
    if score(a) > threshold {
        GhostDecision::Ghost
    } else {
        GhostDecision::Reply
    }
}
```

升高的门槛不会衰减：分支判断的是 `hours_since_last_ghost.is_some()`，`last_ghost_at` 只会被写入、从不清除，而好感度行与聊天会话一一对应——所以一个会话只要 ghost 过，之后整段会话的阈值都是 0.85（1 小时冷静期除外，那里规则 3 本来就强制回复）。

## 實例計算

### 例 1：明確的 ghost

`intrigue=0.1, patience=0.1, tension=0.5`，message_count=50，沒有近期 ghost。

```
score = (1−0.1)×0.4 + (1−0.1)×0.4 + 0.5×0.2
      = 0.36 + 0.36 + 0.10
      = 0.82
```

`0.82 > 0.65` → **Ghost**。

### 例 2：被冷靜期擋下

跟例 1 一樣的好感度，但 `last_ghost = 30 分鐘前`。冷靜期規則（規則 3）在閾值檢查之前命中 → **Reply**。

### 例 3：高分但被 post-ghost 保護擋下

`intrigue=0.05, patience=0.05, tension=0.0`，last_ghost 在 2 小時前。ghost_streak=1。

```
score = (1−0.05)×0.4 + (1−0.05)×0.4 + 0×0.2
      = 0.38 + 0.38 + 0
      = 0.76
```

会话 ghost 过（`last_ghost_at` 已写入）→ 阈值为 `0.85`。`0.76 ≤ 0.85` → **Reply**（但會是個短而乾的回覆——好感度仍然差，人格只是選擇最少限度地參與，而不是消失）。

### 例 4：幼嫩的關係

`intrigue=0, patience=0, tension=1.0`，message_count=5。

`score = (1)×0.4 + (1)×0.4 + 1×0.2 = 1.0`——任何別的場合都會 ghost。但 message_count<10（規則 1）→ **Reply**。新關係永遠有回覆，無論用戶之前多麼難搞。

## 調參直覺

人格 ghost 太勤了 → 提高基礎閾值（0.70+）或加重 `tension` 權重。
人格從不 ghost → 檢查 LLM 好感度評估有沒有真的把 `intrigue` 跟 `patience` 在差的回合往下推。默認值假設評估器在工作、把這些指標推來推去。

## Ghost 不是甚麼

- **不是** 错误响应。HTTP 路由仍返 200。由于引擎采用 SSE 流式传输，ghost 轮会发出三帧后关闭流：`meta(action_type=ghost, model=null)` → `done(usage=null, generation_id=null)` → `final`。不会发出 `delta` 帧，也不会调用任何 LLM。
- **不是** LLM 调用失败。走默认规则引擎时决策纯 Rust，从不问 LLM。配置了可选的 LLM PDE 判断器之后，由判断器提出动作，但 `ghost_permitted` 仍会否决硬安全规则不允许的 ghost，而 `ghosting` 开关可以把每一个 ghost 判定强行拉回 `reply_text`——见 [model-config.zh.md](model-config.zh.md)。
- **不是** 回合沉默的唯一成因。回复文本解析为空是另一条路径：模型返回了空补全，或 `apply_output_regex` 把一条纯 artifact 的回复剥到一无所剩（那里的 fail-safe 是刻意移除的）。这种回合照常落一行内容为空的助手回复行，在线上表现为 `done(ghost_fallback=true)`、`metadata.fallback_reason` 为 `empty_completion` 或 `regex_strip`，并且 **不动** `ghost_streak` / `total_ghosts` / `last_ghost_at`——人格没有做任何决定，只是这条回复回来就是空的。
- **不是** 永遠沉默。時間衰退會恢復 `patience`、軟化 `tension`；最終人格會回應下一條消息。

## 源碼

- `crates/eros-engine-core/src/ghost.rs`——score + ghost_permitted + decide（12 個單元測試）
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`——其中的 `ActionType::Ghost` 分支：标记该行并记录 ghost，不构建 chat 请求
- `crates/eros-engine-store/src/affinity.rs::record_ghost`——持久化（增加 streak、total_ghosts、last_ghost_at），并插入一行 `event_type='ghost'`、增量全零的 `companion_affinity_events` 事件
- `crates/eros-engine-store/src/chat.rs::mark_user_message_ghosted`——把用户行的 `chat_messages.ghost_decision` 置为 true，让重放能把 ghost 结局和还在生成中的回合区分开
