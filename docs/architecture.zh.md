# 架構

[English](architecture.md) · [中文](architecture.zh.md)

## Crate 結構

```
┌─────────────────────────────┐
│ eros-engine-server          │   Axum HTTP、auth 中間件、
│   ↓ 依賴                    │   pipeline 接線
│ eros-engine-store           │   Postgres + pgvector 倉儲、
│   ↓                         │   sqlx 遷移
│ eros-engine-llm             │   OpenRouter + Voyage 客戶端、
│   ↓                         │   TOML 模型配置
│ eros-engine-core            │   純領域——好感度、ghost、
│                             │   PDE、人格、類型。零 I/O。
└─────────────────────────────┘
```

依賴圖嚴格向下——`core` 不知道有 `llm`，`llm` 不知道有 `store`，如此類推。這意味著：

- `core` 是一個普通 Rust crate，可以拉進任何別的項目。沒有 async、沒有 Postgres、沒有 HTTP。毫秒級單元測試。
- `llm` 跟 `store` 是獨立集成。想換掉 Voyage 用別的 embedder、或者換掉 pgvector 用別的向量庫，替換一個 crate 就好。
- `server` 把這些黏起來。如果你不要 HTTP，直接依賴 `core + llm + store` 把引擎當庫嵌進去就行。

## Pipeline

`pipeline::run(state, session_id, event)` 編排單輪對話：

```
加載上下文              用 PersonaRepo 加載人格
                        load_or_create Affinity → apply_time_decay
                        計算 ConversationSignals
       │
       ▼
PDE 決策                eros_engine_core::pde::decide(&input) → ActionPlan
                        （規則型：Reply / Ghost / Proactive / GiftReaction）
       │
       ▼
handler 分派            Reply  → ReplyHandler  構建 ChatRequest
                        Ghost  → GhostHandler  返回 None（不調 LLM）
                        Proact → ProactiveHandler
                        Gift   → GiftHandler   用 event 帶來的 deltas
       │
       ▼
chat 執行               若有 ChatRequest：state.openrouter.execute(req).await?
                        經 ChatRepo 寫入 assistant 消息
       │
       ▼
spawn post_process     tokio::spawn——跟返回響應並行：
                        - affinity 寫入（LLM 評估 6 維 Δ → DB）
                        - memory   （Voyage embed → pgvector upsert）
                        - insight  （LLM 抽事實 → companion_insights 合併）
```

**Ghost streak 重置** 由編排器在 spawn post-process 之前處理：Reply / Proactive / GiftReaction 動作會在一個冪等 UPDATE 裡把 streak 清零；Ghost 動作則調 `AffinityRepo::record_ghost`。倉儲方法 `persist_with_event` 自身永遠不碰 streak。

## Auth

中間件（`auth::middleware::require_auth`）只掛在 `/comp/*` 上。它讀 `Authorization: Bearer …` 頭，調 `state.auth.validate(token)`，把 `AuthUser(user_id)` 作為 extension 注入請求。每個受保護的 handler 讀 `Extension(AuthUser(user_id))`；請求體裡的 `user_id` 永不被信任。

默認驗證器是 `SupabaseJwtValidator`（HS256，密鑰用 `SUPABASE_JWT_SECRET`）。自部署用其他 IdP 的話實現 `AuthValidator` trait，把實例注入 `AppState.auth` 即可。

## 數據流

```
瀏覽器 / 手機客戶端
    │  Authorization: Bearer <Supabase JWT>
    ▼
eros-engine-server :8080
    │
    ├─► auth 中間件 → 從 JWT claims 提取 user_id
    │
    ├─► pipeline::run(session_id, event)
    │       │
    │       └─► spawn post_process(state.clone(), …)
    │              │
    │              ▼
    └────────────► Postgres（`engine` schema）
                       chat_sessions / chat_messages
                       companion_affinity / companion_affinity_events
                       companion_memories（vector(512)）
                       persona_genomes / persona_instances
                       companion_insights
```

post-process spawn 返回 `()` 是 fire-and-forget 設計——用戶面前的響應不會被 affinity / memory / insight 寫入阻塞。它們任何一個失敗，對話回覆還是會落地；失敗會記日誌但不會冒給用戶。

## 為甚麼 core 必須純領域

兩個原因：

1. **思考負擔。** 好感度數學、ghost 決策、PDE 規則——這些是承重邏輯。把它們做成無 I/O 的，意味著 0 依賴的 cargo test 0ms 跑完，不會因為網絡抖動而 flake。`core` 的 25 個測試是上層所有東西的安全網。
2. **可嵌入性。** 任何想在這個基礎上做別的產品的人——日記式 agent、語言教練、教練類陪伴——可以只拉 `core` 進來，不用繼承 HTTP 的形狀、Postgres schema、JWT auth。六維好感度模型才是別人最想拿走的部份；我們把這件事做成輕巧的。

## 文件結構

```
crates/
├── eros-engine-core/
│   └── src/
│       ├── affinity.rs       # 6 維向量 + EMA + 時間衰退 + 標籤
│       ├── ghost.rs          # 評分公式 + 4 層保護
│       ├── pde.rs            # 規則型動作決策
│       ├── persona.rs        # PersonaGenome + Instance + CompanionPersona
│       └── types.rs          # ActionType / Event / DecisionInput / ConversationSignals
├── eros-engine-llm/
│   └── src/
│       ├── openrouter.rs     # ChatRequest / ChatResponse / fallback 重試
│       ├── voyage.rs         # 512 維 embedding，空 key 直接 fail
│       └── model_config.rs   # TOML 加載器
├── eros-engine-store/
│   ├── migrations/           # 0000_schema → 0005_insights
│   └── src/
│       ├── pool.rs           # PgPoolOptions 構造
│       ├── chat.rs           # ChatRepo
│       ├── affinity.rs       # AffinityRepo（persist_with_event、record_ghost）
│       ├── memory.rs         # MemoryRepo（Profile / Relationship 兩層）
│       ├── insight.rs        # InsightRepo（加權 training_level）
│       └── persona.rs        # PersonaRepo（upsert_genome 給 seed 用）
└── eros-engine-server/
    └── src/
        ├── main.rs           # serve | migrate | seed-personas 子命令
        ├── state.rs          # AppState（pool / auth / openrouter / voyage / config）
        ├── error.rs          # AppError → axum IntoResponse
        ├── auth/             # AuthValidator trait + Supabase 實現 + 中間件
        ├── pipeline/         # mod（編排器）/ handlers / post_process
        ├── prompt.rs         # system prompt 構造（affinity → 行為指令）
        ├── routes/           # health / companion / debug / mod
        └── openapi.rs        # utoipa ApiDoc 元數據
```

## 子頁面

- [好感度模型](affinity-model.zh.md)——6 個維度、EMA、時間衰退、關係標籤
- [Ghost 機制](ghost-mechanics.zh.md)——評分公式 + 保護規則 + 實例計算
- [記憶層](memory-layers.zh.md)——profile vs relationship、Voyage、pgvector 檢索
- [部署](deploying.zh.md)——Docker、Fly.io、自帶 Postgres / IdP
- [API 參考](api-reference.zh.md)——每個 `/comp/*` 端點
