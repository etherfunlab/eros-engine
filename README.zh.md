# eros-engine

> **一個用 Rust 寫的開源 AI 伴侶引擎：記憶、關係狀態、結構化用戶洞察。**
>
> `eros-engine` 是 [Eros](https://eros.etherfun.xyz) 背後的 companion-chat 核心，現在抽成可獨立部署的服務。它把每輪對話轉成三種可持久化的信號：結構化用戶畫像、雙層長期記憶，以及會持續影響 persona 行為的六維好感度模型。

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

[English](README.md) · 中文

## 為甚麼需要它

很多 AI character app 把記憶當成 prompt append，把關係當成一段 system prompt 描述。Demo 階段可以運作，但長 session 很容易漂移，也很難調試。

`eros-engine` 把這些能力變成明確狀態：

- **Memory** 放在 Postgres + pgvector，拆成 profile memory 和 relationship memory。
- **Affinity** 是數值向量，透過 EMA 平滑與真實時間衰退更新。
- **User insight** 是可查詢的 JSONB 用戶畫像。
- **Persona behavior** 先由規則型 Persona Decision Engine（PDE）規劃，再交給 LLM 生成。

這不是通用 agent framework，而是為「同一個 persona 長期跟同一個用戶互動」設計的引擎：AI 伴侶、日記式陪伴、coaching agent、language tutor、character chat 都屬於這類場景。

## 核心特性

### 雙層記憶

`eros-engine` 用兩種語義 scope 存記憶：

| 層級 | Scope | 用途 |
|---|---|---|
| Profile memory | `user_id`，且 `instance_id IS NULL` | 跨 session、跨 persona 共享的穩定用戶事實。 |
| Relationship memory | `user_id + persona instance` | 共同經歷、回頭呼應、未解決話題，以及只屬於這段關係的上下文。 |

Embedding 使用 Voyage `voyage-3-lite`，512 維向量。檢索走 pgvector IVFFlat cosine search。

### 六維好感度

每個 chat session 都有一條關係向量：

| 維度 | 範圍 | 控制 |
|---|---:|---|
| `warmth` | -1.0 到 1.0 | 語氣與稱呼，從疏離到親近。 |
| `trust` | 0.0 到 1.0 | 話題深度，以及 persona 是否願意透露自己。 |
| `intrigue` | 0.0 到 1.0 | 好奇心與追問傾向。 |
| `intimacy` | 0.0 到 1.0 | 暱稱、內部梗、對過往細節的呼應。 |
| `patience` | 0.0 到 1.0 | 對低 effort 或重複消息的容忍度。 |
| `tension` | 0.0 到 1.0 | 推拉、摩擦、玩鬧式抵抗。 |

更新使用指數移動平均（EMA），避免 persona 在情緒狀態間突然跳變。`intrigue`、`patience`、`tension` 也會隨真實時間自然衰退或恢復。

`stranger`、`slow_burn`、`friend`、`frenemy`、`romantic` 這些關係標籤由閾值規則從向量中推導出來。它們是內部狀態，不是展示給用戶看的 badge。

### 確定性的 ghost 機制

同一條 affinity vector 也驅動 ghost decision。當 patience 和 intrigue 下降到一定程度，persona 可以選擇不回覆。

四條保護規則避免它變得隨機：

- 前 10 條消息不 ghost；
- 不連續 ghost 兩次；
- ghost 後有 1 小時 cooldown；
- 最近 ghost 過時提高下一次 ghost 閾值。

這是 Rust 裡的 domain logic，不是 prompt 裡的一句建議。

### 結構化用戶洞察

`companion_insights` 表為每個用戶保存一份 JSONB profile：城市、職業、興趣、MBTI 信號、感情價值觀、情緒需求、生活節奏、性格特質、配對偏好。

每個字段會貢獻一部分加權 `training_level`。因此這份 profile 不只服務聊天，也能被 matchmaking、onboarding、coaching logic、analytics 或產品 gating 直接查詢。

## 架構

```txt
┌─────────────────────────────────────────────────────────┐
│ /comp/* HTTP routes  ←  Supabase JWT middleware          │
│         │                                                │
│         ▼                                                │
│ pipeline orchestrator: load → PDE → handler → chat → post│
│                                          │              │
│  ┌───────────────────────────────────────┴────────┐     │
│  │ post-process，在回覆後背景執行                 │     │
│  │   • affinity: 寫入 6D delta + EMA              │     │
│  │   • memory:   Voyage embed → pgvector upsert   │     │
│  │   • insight:  抽取 facts → JSONB merge         │     │
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

Workspace 拆成四個 crate：

| Crate | 職責 |
|---|---|
| `eros-engine-core` | 純 domain logic：affinity math、ghost decision、PDE、persona types。零 I/O。 |
| `eros-engine-llm` | OpenRouter chat client、Voyage embedding client、TOML model-config loader。 |
| `eros-engine-store` | Postgres + pgvector 持久層，所有表都在 `engine` schema 下。 |
| `eros-engine-server` | Axum HTTP service、Supabase JWT middleware、OpenAPI docs、pipeline wiring。 |

你可以直接跑 `eros-engine-server` 當 HTTP API，也可以把 `core + llm + store` 嵌入自己的 Rust service。

## 文檔

- [架構](docs/architecture.zh.md)——crate 邊界、pipeline 階段、data flow。
- [好感度模型](docs/affinity-model.zh.md)——六個維度、EMA、時間衰退、關係標籤。
- [Ghost 機制](docs/ghost-mechanics.zh.md)——score formula、保護規則、示例。
- [記憶層](docs/memory-layers.zh.md)——profile vs relationship memory、Voyage、pgvector retrieval。
- [模型配置](docs/model-config.zh.md)——`model_config.toml` schema、任務名、解析優先級、0.x 穩定性承諾。
- [部署](docs/deploying.zh.md)——Fly.io、Docker、自帶 Postgres / IdP。
- [API 參考](docs/api-reference.zh.md)——每個 `/comp/*` endpoint。

## 快速開始

前置需求：

- `rust-toolchain.toml` 指定的 Rust toolchain。
- Postgres 16+，並啟用 `pgvector` extension。
- OpenRouter API key。
- Voyage API key。
- Supabase JWT secret，或你自己的 `AuthValidator` 實現。

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env
```

填好 `DATABASE_URL`、`OPENROUTER_API_KEY`、`VOYAGE_API_KEY`、`SUPABASE_JWT_SECRET` 之後執行：

```bash
cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

Server 默認監聽 `0.0.0.0:8080`。Scalar API docs 在 `/docs`，OpenAPI JSON 在 `/api-docs/openapi.json`。

官方 Eros Chat web client 是閉源產品。`eros-engine` 本身可以獨立運行；你可以自帶 UI，也可以把 crates 嵌進另一個 service。

## API 表面

默認情況下，所有 `/comp/*` route 都需要 `Authorization: Bearer <Supabase JWT>`。

重點 endpoint：

- `GET  /comp/personas`——列出啟用中的 persona genomes。
- `POST /comp/chat/start`——對指定 persona 開啟 chat session。
- `POST /comp/chat/{session_id}/message`——同步聊天一輪。
- `POST /comp/chat/{session_id}/message_async`——非同步聊天一輪，之後輪詢 pending status。
- `GET  /comp/chat/{session_id}/pending/{message_id}`——輪詢非同步回覆完成狀態。
- `GET  /comp/chat/{session_id}/history`——分頁讀聊天歷史。
- `GET  /comp/chat/{user_id}/sessions`——列出用戶 sessions。
- `GET  /comp/user/{user_id}/profile`——讀取目前的 `companion_insights` 和 `training_level`。
- `POST /comp/chat/{session_id}/event/gift`——套用外部 gift event 與 affinity delta。
- `GET  /comp/chat/{session_id}/gifts`——列出某個 session 的 gift events。
- `GET  /comp/affinity/{session_id}`——debug-only 即時 affinity vector，由 `EXPOSE_AFFINITY_DEBUG=true` 開啟。

如果你不用 Supabase，可以實現 `AuthValidator` trait 接自己的 identity provider。

## 配置

| 環境變量 | 必要 | 說明 |
|---|---|---|
| `DATABASE_URL` | 是 | 帶 `pgvector` 的 Postgres；表建在 `engine.*`。 |
| `OPENROUTER_API_KEY` | 是 | Chat completions；默認由 `examples/model_config.toml` 路由。 |
| `VOYAGE_API_KEY` | 是 | Embeddings。空 key 會拒絕啟動。 |
| `SUPABASE_URL` | 否 | Supabase project URL。保留在 `.env.example` 裡方便 client / deploy 約定；目前 server 不讀取它。 |
| `SUPABASE_JWT_SECRET` | 是 | 默認 auth 使用的 JWT signing secret。 |
| `BIND_ADDR` | 否 | 默認 `0.0.0.0:8080`。 |
| `EXPOSE_AFFINITY_DEBUG` | 否 | 設為 `true` 會開啟 `/comp/affinity/{session_id}`。 |
| `EMA_INERTIA` | 否 | 默認 `0.8`。 |
| `MODEL_CONFIG_PATH` | 否 | 默認 `examples/model_config.toml`。 |
| `RUST_LOG` | 否 | 默認 `info`。 |

## 刻意不包含的東西

這個 repo 是 conversation、memory、relationship-state core。它不包含：

- **Matchmaking**——多階段 filter、soft scoring、agent-to-agent matching simulation 留在閉源產品裡。
- **完整社交產品 UX**——onboarding、video、voice、billing、photos、moderation UI、mobile clients。
- **Persona provenance / marketplace logic**——這是商業產品代碼，不屬於 engine。

如果你要做另一個產品，最值得重用的是 affinity + memory + insight pipeline。

## 內容說明

`examples/personas/` 裡的人格是成人 character-chat 示例。當 relationship state 走到相應位置，它們可以調情、表達慾望；同時仍會拒絕不尊重或越界的要求。如果你的產品需要 SFW default，部署前請替換這些 persona files。

## 貢獻

請先閱讀 [`CONTRIBUTING.md`](CONTRIBUTING.md)。所有貢獻者首次提 PR 時都需要透過 cla-assistant.io 接受 [`CLA`](CLA.md)。

## 授權

`eros-engine` 使用 AGPL-3.0-only 授權。如果 AGPL 不符合你的分發方式，可以洽談商業授權：`henrylin@etherfun.xyz`。
