# 好感度模型

[English](affinity-model.md) · [中文](affinity-model.zh.md)

每輪對話都會變動的六維向量。它是讓人格感覺像人而不是聊天機械人的承重結構。

## 六個維度

| 字段 | 範圍 | 默認值 | 影響甚麼 |
|------|------|--------|----------|
| `warmth` | −1.0 ↔ 1.0 | `0.3` | 語氣、稱呼。負值＝戒備、敵意；正值＝溫暖、親暱。 |
| `trust` | 0.0 ↔ 1.0 | `0.2` | 話題深度，是否願意暴露自己。 |
| `intrigue` | 0.0 ↔ 1.0 | `0.5` | 好奇心、追問動力，抗 ghost 的主力。 |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | 內部梗、暱稱、回頭呼應之前的細節。 |
| `patience` | 0.0 ↔ 1.0 | `0.5` | 對短消息／敷衍回覆的容忍度；ghost 閾值的輸入。 |
| `tension` | 0.0 ↔ 1.0 | `0.1` | 推拉、玩鬧式的小摩擦、傲嬌空間。 |

只有 `warmth` 可以變負值。其餘五個被 `[0, 1]` 鎖定。每次更新都做夾鉗（clamp）。

## EMA 平滑

LLM 評估出來的 deltas 走指數移動平均應用，避免大上大落：

```
new_value = clamp(old_value + (1 − ema_inertia) × delta)
```

默認 `ema_inertia = 0.8`（環境變量 `EMA_INERTIA` 可調）。默認下，LLM 建議的 `+0.5` 變化在這一輪只會把值移動 `+0.1`——其餘部份在後續對話延續同一方向時補上。

```rust
// crates/eros-engine-core/src/affinity.rs
pub fn apply_deltas(&mut self, d: &AffinityDeltas, ema_inertia: f64) {
    let blend = 1.0 - ema_inertia;
    self.warmth   = clamp(self.warmth   + blend * d.warmth,   -1.0, 1.0);
    self.trust    = clamp(self.trust    + blend * d.trust,     0.0, 1.0);
    // … intrigue / intimacy / patience / tension 同樣處理
    self.updated_at = Utc::now();
}
```

### 實例計算

初始 `warmth = 0.3`。LLM 評估這輪 delta 為 `+0.5`。默認慣性。

```
new_warmth = clamp(0.3 + (1 − 0.8) × 0.5)
           = clamp(0.3 + 0.10)
           = 0.40
```

連續三輪 `+0.5` deltas（仍然默認慣性）下，warmth 會走 0.3 → 0.4 → 0.5 → 0.6。人格在四輪裡慢慢熱起來，而不是一輪暴走。

## 時間衰退

六維裡有三個會在沒人陪它的時候按真實時間漂移。衰退是 **懶式計算**——每次加載時讀 `updated_at` 算：

```
days_elapsed = (now − updated_at) / 1 天

intrigue = clamp(intrigue − 0.01  × days_elapsed,  0.0, 1.0)
patience = clamp(patience + 0.005 × days_elapsed,  0.0, 1.0)
tension  = clamp(tension  − 0.005 × days_elapsed,  0.0, 1.0)
```

`warmth`、`trust`、`intimacy` 不衰退——它們是「深層」維度。一旦贏得了信任，離開一星期不應該歸零；只是這段時間人格會稍微少了點好奇心、多了點寬容。

10 天沒回：

- `intrigue` 跌 `0.10`
- `patience` 漲 `0.05`
- `tension` 軟化 `0.05`

## 關係標籤

五個標籤從閾值規則湧現出來，不是用戶選的。匹配按優先級（先中先得）：

| 標籤 | 條件 |
|------|------|
| `romantic` | `warmth ≥ 0.7` 且 `tension ≥ 0.3` 且 `intimacy ≥ 0.4` |
| `friend` | `warmth ≥ 0.7` 且 `trust ≥ 0.6` 且 `tension < 0.2` |
| `frenemy` | `warmth < 0.4` 且 `tension ≥ 0.6` 且 `intrigue ≥ 0.5` |
| `slow_burn` | `intrigue ≥ 0.6` 且 `tension ≥ 0.4` 且 `intimacy < 0.4` |
| `stranger` | 以上都不命中 |

標籤反饋進人格的 system prompt——`prompt.rs` 會根據當前標籤改寫態度指令。用戶看不到標籤，只會在人格的語氣裡感覺到它的後果。

## 持久化

`engine.companion_affinity` 表，每個 chat session 一行（`session_id UNIQUE FK` 鎖 1:1）。每次變動同時往 `engine.companion_affinity_events` 追加一條：

| `event_type` | 何時 |
|--------------|------|
| `message` | Reply 成功；deltas 由 LLM 評估 |
| `ghost` | Ghost 判定命中；ghost_streak / total_ghosts 加一（無 deltas） |
| `gift` | 禮物事件落地；deltas 來自請求體 |
| `time_decay` | 預留（目前未用——衰退在加載時懶算） |

事件表只追加、永不修改。完整歷史可查、可審計、可重建一段關係的演變過程。

## 源碼

- `crates/eros-engine-core/src/affinity.rs`——類型、EMA、時間衰退、標籤推導（10 個單元測試）
- `crates/eros-engine-store/src/affinity.rs`——`AffinityRepo`（persist_with_event、record_ghost）
- `crates/eros-engine-server/src/pipeline/post_process.rs`——LLM 對每輪 deltas 的評估
- `crates/eros-engine-server/src/prompt.rs`——好感度 → 態度指令
