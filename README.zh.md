# eros-engine

> **一個會記住你說過甚麼、而且像真人在跟你聊天的 AI 伴侶引擎。**
>
> 這是 [Eros](https://eros.etherfun.xyz) 約會平台所用的同一套親密度 + 記憶管線，獨立開源出來。你跟人格（persona）對話，引擎一邊靜靜為你構建一份結構化的用戶畫像供日後配對使用，一邊以六維好感度模型驅動人格行為——讓對話有人味，而不是聊天機械人那種千篇一律的客氣。

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

[English](README.md) · 中文

## 它在做甚麼

eros-engine 是約會平台的對話層，整套單獨拉出來。每一輪對話同時做兩件事：

### 1. 用戶畫像管線（`companion_insights`）

每條用戶消息都會被後台 LLM 抽取事實：城市、職業、興趣、MBTI 暗示、感情價值觀、情緒需求、生活節奏、性格特質、配對偏好。這些事實合併進一份 JSONB 畫像（每位用戶一份），同時計算一個加權的 **訓練進度（training level）**——填得越完整，數值越高。

畫像是結構化的——像配對師會用的維度，不是黑箱向量——所以可以直接驅動真實的配對算法。一個願意自然聊天幾個小時的用戶，產出的約會畫像比認真填一份問卷的用戶更深、更準。因為他在回答的，是他從來不知道有人在問的問題。

### 2. 六維好感度（讓對話有人味的部份）

大多數聊天機械人是無狀態的（stateless）。eros-engine 不是。每個 chat session 都帶著一條會隨對話不斷變動的六維向量：

| 維度 | 範圍 | 控制 |
|------|------|------|
| **warmth（溫度）** | −1.0 ↔ 1.0 | 語氣、稱呼，從冷淡到親暱 |
| **trust（信任）** | 0.0 ↔ 1.0 | 願意談的話題深度、是否主動透露自己 |
| **intrigue（興趣）** | 0.0 ↔ 1.0 | 好奇心，會不會追問下去 |
| **intimacy（親密）** | 0.0 ↔ 1.0 | 內部梗、暱稱、回頭呼應之前的細節 |
| **patience（耐性）** | 0.0 ↔ 1.0 | 對短消息或敷衍回覆的容忍度 |
| **tension（張力）** | 0.0 ↔ 1.0 | 推拉、玩鬧式的小摩擦 |

每次更新走指數移動平均（EMA）平滑——所以人格不會大上大落。其中三個維度（intrigue、patience、tension）會隨真實時間自然衰退或恢復——人離開久了，人格的興趣會冷卻、耐性會回血、張力會消化。五種關係標籤——`stranger`、`slow_burn`、`friend`、`frenemy`、`romantic`——由閾值規則從向量自動湧現，不是用戶選的，也不會直接顯示給用戶看，只用來重塑 system prompt。

向量同時驅動一個確定性的 **ghost 判定**——當 patience 跟 intrigue 一起跌穿閾值，人格直接不回。再加四層保護規則（前 10 條消息不 ghost、不連 ghost 兩次、1 小時冷靜期、剛 ghost 過閾值要更高），呈現出來的就不是「永遠在線」，而是「現在不太想理你」的那種輕微在場感。這一個機制做出來的「像在跟人講嘢」效果，遠勝任何 prompt engineering。

### 加上記憶層

兩張 pgvector 表存著人格對你的印象：

- **Profile 層** — 跨 session 的事實（`instance_id IS NULL`），任何版本任何人格都拿得到。
- **Relationship 層** — per-session 的回憶（「那家你下雨天去過的舊書店」），這是讓人覺得「她真的記得我」的部份。

embedding 用 Voyage `voyage-3-lite`（512 維），檢索走 IVFFlat 餘弦相似度。

## 架構

```
┌──────────────────────────────────────────────────────────┐
│ /comp/* HTTP routes  ←  Supabase JWT 中間件               │
│         │                                                 │
│         ▼                                                 │
│  pipeline 編排器：pre → PDE → handler → chat → post       │
│                                          │               │
│  ┌───────────────────────────────────────┴────────┐      │
│  │ post-process（後台，每輪一次）                  │      │
│  │   • affinity 寫入（LLM 評估 6 維 Δ + EMA）      │      │
│  │   • memory   （Voyage embed → pgvector upsert）│      │
│  │   • insight  （抽事實 → companion_insights 合併）│     │
│  └────────────────────────────────────────────────┘      │
└──────────────────────────────────────────────────────────┘
```

`crates/` 下四個 crate：

| Crate | 角色 |
|-------|------|
| `eros-engine-core` | 純領域邏輯——好感度向量數學、ghost 判定、人格決策引擎（PDE）。零 I/O。 |
| `eros-engine-llm` | OpenRouter 對話客戶端 + Voyage embedding 客戶端 + TOML 模型配置加載器。 |
| `eros-engine-store` | Postgres + pgvector 持久層。所有數據表都建在 `engine` schema 下面。 |
| `eros-engine-server` | Axum HTTP 服務 + Supabase JWT 中間件 + pipeline 接線。 |

可以把 `core + llm + store` 當庫嵌進你自己的服務，也可以直接把 `eros-engine-server` 當成獨立 HTTP API 跑。

## 快速開始

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env       # 填入：DATABASE_URL、OPENROUTER_API_KEY、
                           #       VOYAGE_API_KEY、SUPABASE_URL、
                           #       SUPABASE_JWT_SECRET
docker compose -f docker/docker-compose.yml up
```

引擎在 `:8080` 上監聽。OpenAPI / Scalar 文檔界面在 `/docs`。配上 [`eros-engine-web`](https://github.com/etherfunlab/eros-engine-web) 就有一個會把好感度向量實時畫成雷達圖的對話 UI。

要連去現有的 Supabase 項目：所有表都建在 `engine` schema 之下，跟你原有的 `public` schema 不會撞名。

## API 表面

完整列表去 `/docs` 看。重點：

- `POST /comp/chat/start` — 對指定人格開一個 session
- `POST /comp/chat/{session_id}/message` — 同步對話一輪
- `GET  /comp/chat/{session_id}/history` — 分頁讀歷史
- `GET  /comp/user/{user_id}/profile` — 當前的 `companion_insights` 連同 training level
- `GET  /comp/affinity/{session_id}` — 實時的 6 維向量（OSS demo 開、production 通常關，由 `EXPOSE_AFFINITY_DEBUG` 控制）

授權：每個 `/comp/*` 路由都要帶 Supabase JWT 的 Bearer token。`AuthValidator` 是個 trait，自部署可以接你自己的 IdP。

## 配置

| 環境變量 | 必要 | 說明 |
|---------|------|------|
| `DATABASE_URL` | 是 | 帶 `pgvector` extension 的 Postgres。引擎只在 `engine` schema 下建表。 |
| `OPENROUTER_API_KEY` | 是 | 對話走 OpenRouter，路由配置在 `examples/model_config.toml`。 |
| `VOYAGE_API_KEY` | 是 | embedding。空值會直接拒絕啟動——不像隔壁某些實現會偷偷把記憶功能關掉。 |
| `SUPABASE_URL` | 是 | 項目 URL。 |
| `SUPABASE_JWT_SECRET` | 是 | 項目的 JWT 簽名密鑰。每個請求都會驗。 |
| `EXPOSE_AFFINITY_DEBUG` | 否 | `true` 開啟 `/comp/affinity/{session_id}`。 |
| `EMA_INERTIA` | 否 | 默認 `0.8`。 |
| `MODEL_CONFIG_PATH` | 否 | 默認 `examples/model_config.toml`。 |

## 不在這裡的東西

這個 repo 是 **對話 + 親密度核心**。以下幾樣是刻意留在閉源那一邊的：

- **配對算法本身** — 多階段過濾 + soft scoring + agent-to-agent 模擬對話那一整套，留在閉源產品裡。eros-engine 負責生產供其消費的 *畫像*，但本身不撮合人。
- **完整的社交產品 UX** — onboarding、語音、視頻、付費、相片、相冊。
- **人格的數字資產系譜** — 屬於商業資產，不公開。

如果你想做的是另一個產品——日記式陪伴、語言學習、教練類 agent——好感度 + 記憶 + insight 這套管線就是你會想拿走的部份。

## 對成年人的話

這個項目是面向全世界的開源實驗，不是合規產品。內置的人格（Aria、Kenji、Miel）是無過濾的成年人對話設計：當好感度走到那個份上，可以調情、可以坦白慾望、可以露骨——但仍然會在你不尊重她、要求她變成另一個人、或在虛構裡踩過界時拒絕你。它們不是工具，是角色。

我們不會走「萬能但溫吞」那條路。如果你的產品需要 SFW 默認，把 `examples/personas/` 改寫一遍就可以。

## 貢獻

去看 [`CONTRIBUTING.md`](CONTRIBUTING.md)。所有貢獻者首次提 PR 時要簽 [`CLA`](CLA.md)（cla-assistant.io 機械人會引導，簽一次以後都當已簽）。

## 授權

AGPL-3.0。如果 AGPL 不符合你的分發方式，可以另談商業授權——`henrylin@etherfun.xyz`。
