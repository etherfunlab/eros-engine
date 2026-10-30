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

`pipeline::stream::run_stream(state: Arc<AppState>, user_msg: PersistedUserMessage, prefetched_persona: Option<CompanionPersona>)` 編排單輪對話，返回 SSE 帧流：

```
加載上下文              用 PersonaRepo 加載人格
                        load_or_create Affinity → apply_time_decay
                        計算 ConversationSignals
       │
       ▼
PDE 決策                eros_engine_core::pde::decide(&input) → ActionPlan
                        （默认规则型；可通过
                         [tasks.pde_decision].filter_prompt 启用 LLM 判断器，
                         失败时回退到规则引擎；判断器以流式调用，模型返回的
                         任何推理都会以 `reasoning` 帧到达客户端；判断结果
                         记录到 companion_decision_events——payload = 模型
                         返回的，inputs = 引擎供给它看的状态）
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
                        - insight  （LLM 抽事实 → human_facts 账本；
                          由按计划的重建从账本派生
                          human_insights 画像，绝不逐轮）
                        - character insight（LLM 抽 AI 角色自身的事实 →
                          character_insights UPSERT，实验特性）
                        - user insight（LLM 抽真人用户的事实，按这段关系分开
                          → user_insights UPSERT，实验特性——不是
                          human_insights，不会被注入任何 prompt）
                        （五个 future 经 tokio::join! 并发执行）
                        - 关系扫描：本轮先于这五个计入所在关系；待扫轮数到
                          20 时，在它们之后就地扫描（dreaming）
```

**群聊轮次**（`pipeline::group::run_group_turn`）通过 `[tasks.group_director]` 调用选出发言者，再让每位发言者走同一条回复路径；详见 [group-chat.zh.md](group-chat.zh.md)。

**Ghost streak 重置** 由編排器在 spawn post-process 之前處理：Reply 動作會在一個冪等 UPDATE 裡把 streak 清零；Ghost 動作則調 `AffinityRepo::record_ghost`。倉儲方法 `persist_with_event` 自身永遠不碰 streak。

**离线。** 角色的离线类生活事件（`away_from_phone`、`busy`、`in_battle`、`asleep`、两个聚会、`having_sex`、女性角色的 `out_shopping`，以及她提出的分开）进行期间，不带用户动作、也不带 `image.force` 的文字轮在 PDE 之前就被 ghost：该行被标记为已 ghost，发出 ghost 帧，判官和 `record_ghost` 都不运行。见 [ghost-mechanics.zh.md](ghost-mechanics.zh.md#engine-强制的-ghost)。

**PDE 动作列表。** 判断器（或规则引擎）每轮给出的 action 是以下之一：
`reply_text` | `ghost` | `reply_image` | `reply_text_image` | `product_qa`。
其中三个是有条件可用的：`reply_image` 和 `reply_text_image` **同时**要求请求带
`image` 块**且** `[tasks.chat_image_prompt_compose]` 已配置——判断器不再写
image-prompt 种子之后，合成器是唯一能产出图片 prompt 的东西，缺了这个任务
图片轮次就不可用；`product_qa` 要求 `[tasks.chat_product_qa]` 已配置（且 LLM
PDE 已启用）。各自在不可用时降级为 `reply_text`——只降级、绝不升级。带用户动作的轮次里，`ghost` 和 `product_qa` 同样降级为 `reply_text`。
`product_qa` 会短路整条伴侣链路（不注入人格 prompt、不跑 post-process）：
它路由到一个独立的产品问答执行器，而不是回复路径。各动作的启用门槛见
[model-config.zh.md](model-config.zh.md)。

## Auth

中间件（`auth::middleware::require_auth`）保护除 `/healthz`（在 auth 层之外合并）、`/docs`（在 `main.rs` 里合并）和 `/svc/v1/*`（三棵子树，各以自己的共享 secret header 放行，各自只在设置了那个 secret 时挂载）以外的一切——目前即 `/comp/*`、`/v2/comp/*`、`/bff/v1/*`、`/world/*`、`/persona/*`。该层挂在 `routes/mod.rs` 的合并子路由上，而不是某个路径前缀上，所以新增命名空间（如 `/persona/*`）不需要任何额外的鉴权接线。它讀 `Authorization: Bearer …` 頭，調 `state.auth.validate(token)`，把 `AuthUser(user_id)` 作為 extension 注入請求。每個受保護的 handler 讀 `Extension(AuthUser(user_id))`；請求體裡的 `user_id` 永不被信任。同一个子路由上还有第二层 `auth::middleware::require_caller_secret`，先于它运行：设置了 `CALLER_SECRET` 时，没带匹配 `x-caller-secret` header 的请求直接被拒为 `403 caller_forbidden`，根本不会去看它的 JWT；未设置时这一层全部放行。

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
    │       └─► 出图 prompt 合成器 LLM——落一行到 chat_images_events 和
    │           llm_generations；无聊天状态、无好感度、无记忆
    │
    └────────────► Postgres（`engine` schema）
                       chat_sessions / chat_messages / chat_session_members
                       companion_affinity / companion_affinity_events
                       companion_memories（vector(512)）
                       relationship_scan_state / relationship_episodes /
                       relationship_arcs / relationship_journal /
                       relationship_scan_events
                       persona_genomes / persona_instances / persona_instance_items
                       human_facts / human_profile_state / human_insights
                       user_erasures
                       companion_decision_events
                       chat_images_events / chat_vision_events
                       llm_generations（每次 LLM 调用，审计）
```

insight 这一步把每轮的事实记进每个用户各一份的账本，按计划的画像重建（累积 20 条新事实，或闲置一段时间后）再从账本派生出 `human_insights` 行；见 [insights.zh.md](insights.zh.md)。

post-process spawn 返回 `()` 是 fire-and-forget 設計——用戶面前的響應不會被 affinity / memory / insight 寫入阻塞。它們任何一個失敗，對話回覆還是會落地；失敗會記日誌但不會冒給用戶。

**`chat_messages.channel`**：`NULL` = 普通文本；`'voice'` = 语音频道；
`'product_qa'` = 出戏产品问答——非 NULL 的行会被排除在短期回忆、对话信号、
好感度评估、insight 抽取之外，但在实时流、重放和客户端历史记录里完全可见。
关系扫描（dreaming）是例外：它默认也会读 `'voice'` 的行，和同一段关系的文字行一起读（可用
`DREAMING_VOICE_DISABLED` 关掉），但仍然排除 `'product_qa'`。

**频道归属在写入侧双向强制。** `companion_stream` 拒绝语音或群 session，`voice`
拒绝非语音 session，群发送路由拒绝非群 session，都在落库任何一行之前返回 `409 wrong_channel`。少了
这层对称性，文本轮次会落进语音会话，它的 `client_msg_id` 还会和语音轮次的
查找撞车。

**一个语音轮次最多挂一条 assistant 回复**，由
`(user_message_id) WHERE role='assistant' AND channel='voice'` 上的 partial
unique index 保证（migration 0041）。文本路径没有这条约束——它本来就会通过
`continues_from_message_id` 为同一个用户轮次写多条 assistant 行。这条索引存在
是因为一个语音轮次有两个可能的写入方：流式生成器，以及上报客户端实际播出内容
的 barge-in interrupt 端点。两者靠 user 行上共享的 `FOR UPDATE` 锁排序：
`content` 归 interrupt，审计列（`generation_id`——join 进 `llm_generations` 的
key，模型和花费都在那张表里）归生成器。
user 行上带 `metadata.voice_interrupt` 表示这一轮是用户主动打断的——这也正是
区分「重试」与「可修复的掉线」的依据。详见
[voice barge-in](superpowers/specs/2026-08-11-voice-barge-in-interrupt-design.md)。

## 為甚麼 core 必須純領域

兩個原因：

1. **思考負擔。** 好感度數學、ghost 決策、PDE 規則——這些是承重邏輯。把它們做成無 I/O 的，意味著 0 依賴的 cargo test 0ms 跑完，不會因為網絡抖動而 flake。`core` 的 89 個測試是上層所有東西的安全網。（可选 LLM 判断器层在 `server` 里，不在 `core` 里，所以 `core` 保持零 I/O。）
2. **可嵌入性。** 任何想在這個基礎上做別的產品的人——日記式 agent、語言教練、教練類陪伴——可以只拉 `core` 進來，不用繼承 HTTP 的形狀、Postgres schema、JWT auth。六維好感度模型才是別人最想拿走的部份；我們把這件事做成輕巧的。

## 文件結構

```
crates/
├── eros-engine-core/
│   └── src/
│       ├── affinity.rs       # 6 維向量 + 档位写入管线 + 時間衰退 + 標籤
│       ├── effects.rs        # 被接受的 give 的余效（曲线 + active_effects）
│       ├── events.rs         # 生活事件的分类、种类与两段抽取
│       ├── ghost.rs          # 評分公式 + 4 層保護
│       ├── group.rs          # 群聊导演输出规整、兜底点名、转录角色映射
│       ├── needs.rs          # 角色的六项需求与习惯（档位 + 按时间推算）
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
│   ├── migrations/           # 0000_baseline（v2.3.1 schema）+ 其後增量
│   └── src/
│       ├── pool.rs           # PgPoolOptions 構造
│       ├── chat.rs           # ChatRepo
│       ├── events.rs         # EventRepo（persona_events：进行中的事件、历史、离线时段、虚惊）
│       ├── group.rs          # GroupRepo（群聊会话、成员关系）
│       ├── life_choices.rs   # LifeChoiceRepo（角色提出分开的选择：选择行、待回答、结算、放缓窗口）
│       ├── affinity.rs       # AffinityRepo（persist_with_event、record_ghost、persist_life_choice_drop_in_tx）
│       ├── memory.rs         # MemoryRepo（Profile / Relationship 兩層）
│       ├── insight.rs        # InsightEventRepo（human_insights_events 审计行）
│       ├── human_fact.rs     # HumanFactRepo（用户的事实账本：去重写入、导出 keyset）
│       ├── profile_state.rs  # ProfileStateRepo（重建计数器、认领、结果）
│       ├── erasure.rs        # ErasureRepo（用户抹除、删除流）
│       ├── human_insight.rs  # HumanInsightRepo（类型化画像列，由重建整行替换）
│       ├── character_insight.rs # CharacterInsightRepo + 事件/快照 repo（AI 角色自己的画像，实验特性）
│       ├── user_insight.rs   # UserInsightRepo + 事件/快照 repo（真人用户按关系分开的画像，实验特性）
│       ├── scan_state.rs     # ScanStateRepo（关系扫描的游标、待扫轮数、认领、回填窗口）
│       ├── episode.rs        # EpisodeRepo（片段摘要、扫描的分段读取）
│       ├── arc.rs            # ArcRepo（关系弧线）
│       ├── journal.rs        # JournalRepo（带日期的时刻；引擎自己写的行）
│       ├── scan_event.rs     # ScanEventRepo（relationship_scan_events 读数）
│       └── persona.rs        # PersonaRepo（upsert_genome 給 seed 用）
└── eros-engine-server/
    └── src/
        ├── main.rs           # serve | migrate | seed-personas | print-openapi | backfill-dreaming 子命令
        ├── state.rs          # AppState（pool / auth / openrouter / embed / config）
        ├── error.rs          # AppError → axum IntoResponse
        ├── auth/             # AuthValidator trait + Supabase 實現 + 中間件
        ├── pipeline/         # stream（run_stream）/ group + group_director / reasoning_call / handlers / post_process / dreaming / …
        ├── prompt.rs         # system prompt 構造（affinity → 行為指令）
        ├── routes/           # health / companion / companion_stream / companion_async / insight / image_edit / voice / persona / session_open / session_events / session_effects / life_choices / persona_items / reaction / group / world_town / svc（服务间导出，`/svc/v1/*`）/ bff / dto / mod
        └── openapi.rs        # utoipa ApiDoc 元數據
```

## 子頁面

- [好感度模型](affinity-model.zh.md)——6 個維度、档位写入管线、時間衰退、關係標籤
- [Ghost 機制](ghost-mechanics.zh.md)——評分公式 + 保護規則 + 實例計算
- [群聊](group-chat.zh.md)——一位用户加至多三位角色、导演、发言者接力
- [生活事件与需求](life-events-needs.zh.md)——角色的需求及其漂移、事件抽取、背景事件、离线类、虚惊、提出分开的选择
- [記憶層](memory-layers.zh.md)——profile vs relationship、Voyage、pgvector 檢索
- [部署](deploying.zh.md)——Docker、自带 Postgres / IdP
- [API 參考](api-reference.zh.md)——每個 `/comp/*` 端點
