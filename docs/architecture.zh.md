# 架構

[English](architecture.md) · [中文](architecture.zh.md)

## Crate 結構

```
┌─────────────────────────────┐
│ eros-engine-server          │   Axum HTTP、auth 中間件、
│   ↓ 依赖全部三者            │   pipeline 接線
├─────────────────────────────┤
│ eros-engine-llm             │   OpenRouter + Voyage 客戶端、
│                             │   TOML 模型配置
│ eros-engine-store           │   Postgres + pgvector 倉儲、
│                             │   sqlx 遷移
│   ↓ 两者都只依赖 core       │
├─────────────────────────────┤
│ eros-engine-core            │   純領域——好感度、ghost、
│                             │   PDE、人格、類型。零 I/O。
└─────────────────────────────┘
```

依賴圖嚴格向下——`core` 不知道有 `llm`，`llm` 不知道有 `store`，如此類推。這意味著：

- `core` 是一個普通 Rust crate，可以拉進任何別的項目。沒有 async、沒有 Postgres、沒有 HTTP。毫秒級單元測試。
- `llm` 跟 `store` 是獨立集成。换掉 embedder 现在是配置层面的事——`[tasks.embedding]` 可以指向内建的 Voyage 客户端、内建的 OpenRouter 端点，或任何 `[providers].<name>.embeddings` URL——不必换 crate；换掉 pgvector 用别的向量库，还是得换 `store` 这个 crate。
- `server` 把這些黏起來。如果你不要 HTTP，直接依賴 `core + llm + store` 把引擎當庫嵌進去就行。

## Pipeline

`pipeline::stream::run_stream(state: Arc<AppState>, user_msg: PersistedUserMessage)` 編排單輪對話，返回 SSE 帧流：

```
加載上下文              用 PersonaRepo 加載人格
                        load_or_create Affinity → apply_time_decay
                        計算 ConversationSignals
       │
       ▼
PDE 決策                eros_engine_core::pde::decide(&input) → ActionPlan
                        （默认规则型；可通过
                         [tasks.pde_decision].filter_prompt 启用 LLM 判断器，
                         失败时回退到规则引擎；判断结果记录到
                         companion_decision_events——payload = 模型返回的，
                         inputs = 引擎供给它看的状态）
       │
       ▼
action 分派             run_stream 里的 inline `match plan.action_type`：
                        Ghost     → 标记该行 ghosted + record_ghost（不调 LLM）
                        ProductQa → 独立的产品问答执行器
                        其余      → 回复路径构建 ChatRequest
       │
       ▼
chat 執行               若有 ChatRequest：state.openrouter.execute(req).await?
                        （reply_text_image：图片合成器在 chat 调用前就已
                         tokio::spawn，文本回复流完后才 join——合成这一跳
                         藏在 chat 调用下面）
                        經 ChatRepo 寫入 assistant 消息
       │
       ▼
spawn post_process     tokio::spawn——跟返回響應並行：
                        - affinity 寫入（LLM 判官报档位 →
                          引擎写入管线 → DB）
                        - memory   （Voyage embed → pgvector upsert）
                        - insight  （LLM 抽事实 → human_insights UPSERT，
                          按列增量更新）
                        （三个 future 经 tokio::join! 并发执行）
```

**Ghost streak 重置** 由編排器在 spawn post-process 之前處理：Reply / Proactive 動作會在一個冪等 UPDATE 裡把 streak 清零；Ghost 動作則調 `AffinityRepo::record_ghost`。倉儲方法 `persist_with_event` 自身永遠不碰 streak。

**PDE 动作列表。** 判断器（或规则引擎）每轮给出的 action 是以下之一：
`reply_text` | `ghost` | `reply_image` | `reply_text_image` | `product_qa`。
其中三个是有条件可用的：`reply_image` 和 `reply_text_image` **同时**要求请求带
`image` 块**且** `[tasks.chat_image_prompt_compose]` 已配置——判断器不再写
image-prompt 种子之后，合成器是唯一能产出图片 prompt 的东西，缺了这个任务
图片轮次就不可用；`product_qa` 要求 `[tasks.chat_product_qa]` 已配置（且 LLM
PDE 已启用）。各自在不可用时降级为 `reply_text`——只降级、绝不升级。
`product_qa` 会短路整条伴侣链路（不注入人格 prompt、不跑 post-process）：
它路由到一个独立的产品问答执行器，而不是回复路径。各动作的启用门槛见
[model-config.zh.md](model-config.zh.md)。

## Auth

中间件（`auth::middleware::require_auth`）保护除 `/healthz`（在 auth 层之外合并）和 `/docs`（在 `main.rs` 里合并）以外的一切——目前即 `/comp/*`、`/bff/v1/*`、`/world/*`、`/persona/*`。该层挂在 `routes/mod.rs` 的合并子路由上，而不是某个路径前缀上，所以新增命名空间（如 `/persona/*`）不需要任何额外的鉴权接线。它讀 `Authorization: Bearer …` 頭，調 `state.auth.validate(token)`，把 `AuthUser(user_id)` 作為 extension 注入請求。每個受保護的 handler 讀 `Extension(AuthUser(user_id))`；請求體裡的 `user_id` 永不被信任。

默认验证器是 `SupabaseJwtValidator`；它依 token 的 `alg` 头派发，优先用基于 JWKS 的非对称验证（ES256/RS256/EdDSA）——这是 Supabase 自 2025 年 JWT Signing Keys 上线后的默认机制——来源是 `SUPABASE_JWKS_URL` 或由 `SUPABASE_URL` 推导；未迁移的项目则回退到用 `SUPABASE_JWT_SECRET` 的旧版 HS256。自部署用其他 IdP 的話實現 `AuthValidator` trait，把實例注入 `AppState.auth` 即可。

## 數據流

```
瀏覽器 / 手機客戶端
    │  Authorization: Bearer <Supabase JWT>
    ▼
eros-engine-server :8080
    │
    ├─► auth 中間件 → 從 JWT claims 提取 user_id
    │
    ├─► pipeline::stream::run_stream(state, user_msg)
    │       │
    │       └─► spawn post_process(state.clone(), …)
    │              │
    │              ▼
    ├─► routes::persona（/persona/{instance_id}/image/compose）
    │       └─► 出图 prompt 合成器 LLM——落一行到 chat_images_events，
    │           其余不落任何表（无聊天状态、无好感度、无记忆）
    │
    └────────────► Postgres（`engine` schema）
                       chat_sessions / chat_messages
                       companion_affinity / companion_affinity_events
                       companion_memories（vector(512)）
                       persona_genomes / persona_instances
                       human_insights
                       companion_decision_events
                       chat_images_events / chat_vision_events
```

post-process spawn 返回 `()` 是 fire-and-forget 設計——用戶面前的響應不會被 affinity / memory / insight 寫入阻塞。它們任何一個失敗，對話回覆還是會落地；失敗會記日誌但不會冒給用戶。

**`chat_messages.channel`**：`NULL` = 普通文本；`'voice'` = 语音频道；
`'product_qa'` = 出戏产品问答——非 NULL 的行会被排除在短期回忆、对话信号、
好感度评估、insight 抽取之外，但在实时流、重放和客户端历史记录里完全可见。
dreaming 是例外：它默认也会读 `'voice'` 的行（通话空闲后才读；可用
`DREAMING_VOICE_DISABLED` 关掉），但仍然排除 `'product_qa'`。

**频道归属在写入侧双向强制。** `companion_stream` 拒绝语音 session，`voice`
拒绝非语音 session，两者都在落库任何一行之前返回 `409 wrong_channel`。少了
这层对称性，文本轮次会落进语音会话，它的 `client_msg_id` 还会和语音轮次的
查找撞车。

**一个语音轮次最多挂一条 assistant 回复**，由
`(user_message_id) WHERE role='assistant' AND channel='voice'` 上的 partial
unique index 保证（migration 0041）。文本路径没有这条约束——它本来就会通过
`continues_from_message_id` 为同一个用户轮次写多条 assistant 行。这条索引存在
是因为一个语音轮次有两个可能的写入方：流式生成器，以及上报客户端实际播出内容
的 barge-in interrupt 端点。两者靠 user 行上共享的 `FOR UPDATE` 锁排序：
`content` 归 interrupt，审计列（`model` / `usage` / `generation_id`）归生成器。
user 行上带 `metadata.voice_interrupt` 表示这一轮是用户主动打断的——这也正是
区分「重试」与「可修复的掉线」的依据。详见
[voice barge-in](superpowers/specs/2026-08-11-voice-barge-in-interrupt-design.md)。

## 為甚麼 core 必須純領域

兩個原因：

1. **思考負擔。** 好感度數學、ghost 決策、PDE 規則——這些是承重邏輯。把它們做成無 I/O 的，意味著 0 依賴的 cargo test 0ms 跑完，不會因為網絡抖動而 flake。`core` 的 69 個測試是上層所有東西的安全網。（可选 LLM 判断器层在 `server` 里，不在 `core` 里，所以 `core` 保持零 I/O。）
2. **可嵌入性。** 任何想在這個基礎上做別的產品的人——日記式 agent、語言教練、教練類陪伴——可以只拉 `core` 進來，不用繼承 HTTP 的形狀、Postgres schema、JWT auth。六維好感度模型才是別人最想拿走的部份；我們把這件事做成輕巧的。

## 文件結構

```
crates/
├── eros-engine-core/
│   └── src/
│       ├── affinity.rs       # 6 維向量 + 档位写入管线 + 時間衰退 + 標籤
│       ├── ghost.rs          # 評分公式 + 4 層保護
│       ├── pde.rs            # 規則型動作決策
│       ├── persona.rs        # PersonaGenome + Instance + CompanionPersona
│       ├── scope.rs          # 逐请求注入范围（InsightMode / MemoryScope）
│       └── types.rs          # ActionType / Event / DecisionInput / ConversationSignals
├── eros-engine-llm/
│   └── src/
│       ├── openrouter.rs     # ChatRequest / ChatResponse / fallback 重試
│       ├── voyage.rs         # 512 維 embedding，空 key 直接 fail
│       └── model_config.rs   # TOML 加載器
├── eros-engine-store/
│   ├── migrations/           # 0000_schema → 0047_character_insights
│   └── src/
│       ├── pool.rs           # PgPoolOptions 構造
│       ├── chat.rs           # ChatRepo
│       ├── affinity.rs       # AffinityRepo（persist_with_event、record_ghost）
│       ├── memory.rs         # MemoryRepo（Profile / Relationship 兩層）
│       ├── insight.rs        # InsightEventRepo（companion_insights_events 审计行）
│       ├── human_insight.rs  # HumanInsightRepo（类型化画像列，按字段 UPSERT）
│       ├── character_insight.rs # CharacterInsightRepo + 事件/快照 repo（AI 角色自己的画像，实验特性）
│       └── persona.rs        # PersonaRepo（upsert_genome 給 seed 用）
└── eros-engine-server/
    └── src/
        ├── main.rs           # serve | migrate | seed-personas | print-openapi 子命令
        ├── state.rs          # AppState（pool / auth / openrouter / embed / config）
        ├── error.rs          # AppError → axum IntoResponse
        ├── auth/             # AuthValidator trait + Supabase 實現 + 中間件
        ├── pipeline/         # stream（run_stream）/ handlers / post_process / dreaming / …
        ├── prompt.rs         # system prompt 構造（affinity → 行為指令）
        ├── routes/           # health / companion / companion_stream / voice / persona / world_town / bff / dto / mod
        └── openapi.rs        # utoipa ApiDoc 元數據
```

## 子頁面

- [好感度模型](affinity-model.zh.md)——6 個維度、档位写入管线、時間衰退、關係標籤
- [Ghost 機制](ghost-mechanics.zh.md)——評分公式 + 保護規則 + 實例計算
- [記憶層](memory-layers.zh.md)——profile vs relationship、Voyage、pgvector 檢索
- [部署](deploying.zh.md)——Docker、自带 Postgres / IdP
- [API 參考](api-reference.zh.md)——每個 `/comp/*` 端點
