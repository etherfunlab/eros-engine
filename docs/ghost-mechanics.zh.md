# Ghost 機制

[English](ghost-mechanics.md) · [中文](ghost-mechanics.zh.md)

人格決定 **不** 在這一輪回覆。確定性——不調 LLM。讓對話感覺像在跟一個有自己狀態的人說話，這個機制單獨做的工作最多。

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
     若剛 ghost 過：
       閾值 = 0.85               （ghost 過之後，門檻提高）
     score > 閾值 才 ghost
```

實現：

```rust
pub fn decide(a: &Affinity, s: GhostSignals) -> GhostDecision {
    if s.message_count < 10 { return GhostDecision::Reply; }
    if a.ghost_streak >= 2 { return GhostDecision::Reply; }
    if matches!(s.hours_since_last_ghost, Some(h) if h < 1.0) {
        return GhostDecision::Reply;
    }
    let threshold = if s.hours_since_last_ghost.is_some() { 0.85 } else { 0.65 };
    if score(a) > threshold {
        GhostDecision::Ghost
    } else {
        GhostDecision::Reply
    }
}
```

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

近期 ghost 過 → 閾值升到 `0.85`。`0.76 ≤ 0.85` → **Reply**（但會是個短而乾的回覆——好感度仍然差，人格只是選擇最少限度地參與，而不是消失）。

### 例 4：幼嫩的關係

`intrigue=0, patience=0, tension=1.0`，message_count=5。

`score = (1)×0.4 + (1)×0.4 + 1×0.2 = 1.0`——任何別的場合都會 ghost。但 message_count<10（規則 1）→ **Reply**。新關係永遠有回覆，無論用戶之前多麼難搞。

## 調參直覺

人格 ghost 太勤了 → 提高基礎閾值（0.70+）或加重 `tension` 權重。
人格從不 ghost → 檢查 LLM 好感度評估有沒有真的把 `intrigue` 跟 `patience` 在差的回合往下推。默認值假設評估器在工作、把這些指標推來推去。

## Ghost 不是甚麼

- **不是** 錯誤響應。HTTP 路由仍返 200。響應體 `reply: null`（或引擎選定的「無回覆」形狀）。
- **不是** LLM 調用失敗。決策純 Rust，從不問 LLM。
- **不是** 永遠沉默。時間衰退會恢復 `patience`、軟化 `tension`；最終人格會回應下一條消息。

## 源碼

- `crates/eros-engine-core/src/ghost.rs`——score + decide（7 個單元測試）
- `crates/eros-engine-server/src/pipeline/handlers.rs::GhostHandler`——返回無 chat 請求的 handler
- `crates/eros-engine-store/src/affinity.rs::record_ghost`——持久化（增加 streak、total_ghosts、last_ghost_at）
