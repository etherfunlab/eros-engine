# 記憶層

[English](memory-layers.md) · [中文](memory-layers.zh.md)

兩張 pgvector 表存著人格對你的印象。它們服務不同的回憶需求，分開查詢。

## Profile vs Relationship

| 層 | `instance_id` | 存甚麼 | 生命周期 |
|---|---|---|---|
| **Profile** | `NULL` | 跨 session 的事實——任何人格都能知道的東西。 | 永久 |
| **Relationship** | `<uuid>` | per-session 的回想——這個特定人格跟這位用戶之間的小事。 | 隨 session |

這個區分要緊，因為 **跨人格的角色穩定性** 跟 **單個關係內的親密度** 是不同的需求。如果你跟 Aria 說你對花生過敏，那是 profile 事實——Kenji 也應該知道。如果 Aria 提到她今晚在讀 Bishop，那是 relationship 記憶——Kenji 不應該假裝知道這事。

## 存儲

單表，兩層用 `instance_id` 區分：

```sql
CREATE TABLE engine.companion_memories (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id   UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    user_id      UUID NOT NULL,
    instance_id  UUID,                         -- NULL = profile 層
    content      TEXT NOT NULL,
    embedding    VECTOR(512) NOT NULL,
    category     TEXT,                         -- fact|preference|event|emotion|relation；逐轮原文写入的行为 NULL
    metadata     JSONB,                        -- 每条记忆的不透明抽取载荷；逐轮原文写入的行为 NULL
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

兩個帶過濾條件的索引——一層一個——讓熱路徑檢索保持便宜：

```sql
CREATE INDEX idx_memories_user_profile
  ON engine.companion_memories(user_id)
  WHERE instance_id IS NULL;

CREATE INDEX idx_memories_session
  ON engine.companion_memories(session_id)
  WHERE instance_id IS NOT NULL;
```

## Embedding

默认 `voyage-4-lite` 走 Voyage 原生 API。512 维、多语言——schema 是 `VECTOR(512)`，非 Voyage 路由也会在请求里强制 `dimensions: 512`。

路由由 `[tasks.embedding]` 决定。`model` 带上 `@provider` 后缀就把调用发去别处：`@openrouter` 走内建的 OpenRouter embeddings 端点（URL 可用 `[providers].openrouter.embeddings` 覆盖），`@<name>` 走任何声明了 `embeddings` URL 的 `[providers]` 条目（密钥读 `<NAME>_API_KEY`）。另一种写法是配一对 `model_read`/`model_write`，把召回和入库模型拆开独立指定——两者必须成对出现、与 `model` 互斥，且只允许 voyage-4 系列及以上的 Voyage 模型：只有这个系列保证不同尺寸模型共享同一向量空间。

```rust
// crates/eros-engine-llm/src/embedding.rs — EmbeddingRouter
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;    // read 后端
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>; // write 后端
```

`embed_query` 服务每轮召回路径，`embed_document` 服务入库路径。在 Voyage 后端上，两者变成不同的 `input_type` 提示（`query` vs `document`）；OpenRouter 兼容协议没有这个提示，所以在那边这个区分只在 `model_read`/`model_write` 确实不同时才有意义。

密钥缺失时引擎依然**大声拒绝启动**——但 `VOYAGE_API_KEY` 这道 boot 闸只在解析出的路由包含 Voyage 后端时才触发；OpenRouter 和自定义路由改由各自的密钥把关（`OPENROUTER_API_KEY` / `<NAME>_API_KEY`）。閉源版的 eros-gateway 有個已知回歸：空 key 會悄悄關掉 embeddings；eros-engine 拒絕繼承這個坑。

## 檢索

走 pgvector 的 `<=>` 操作符做餘弦相似度，配 IVFFlat 索引：

```sql
CREATE INDEX idx_memories_embedding
  ON engine.companion_memories
  USING ivfflat (embedding vector_cosine_ops)
  WITH (lists = 100);
```

Profile 層查詢：

```sql
SELECT id, session_id, user_id, instance_id, content, category, metadata, created_at
FROM engine.companion_memories
WHERE user_id = $1 AND instance_id IS NULL
ORDER BY embedding <=> $2::vector
LIMIT $3;
```

Relationship 层查询改为过滤 `instance_id = $2`，并多一条 `content NOT LIKE '%\nAI：%'`（`$3`）排除旧版逐字转录行（issue #113），embedding 和 LIMIT 随之后移到 `$4`、`$5`。代码里任何地方都不计算相似度分数——结果直接按 `<=>` 余弦距离原值排序，检索就是 `LIMIT` 取 top-K；没有相似度阈值，最近的 K 行不管绝对距离多远都会返回。

`lists = 100` 是中小規模表（≲ 1M 行）的平衡默認值。語料更大就調高（經驗法則：`lists ≈ √rows`）。

## 甚麼會被 embed

两个互相独立的写入方，跑在不同的节奏上：

1. **逐轮原文写入**（`write_turn`，post-process）——在**每一个**实质轮次都跑：用户话语非空，且至少有一条非空的助手消息。它只把**用户话语**embed 进两个层，不存助手的回复文本——助手回复经记忆召回重新进入模型自己的 prompt 会形成反馈回路，让后续回复反复出现同一句话（issue #113）。关系层那一份带 `用户：` 前缀，召回出来后仍能认出是用户原话。这些行的 `category` 和 `metadata` 都是 NULL。
2. **Dreaming-lite 清扫器**（`[tasks.memory_extraction]`，`pipeline::dreaming`）——后台的、由 session 闲置触发的一遍，问 LLM 要值得长期保留的记忆候选。它**只写 profile 层**，带 `category ∈ {fact, preference, event, emotion, relation}`（模型自创的其它类别一律收敛成 `fact`），外加一份不透明的 `metadata`。

`insight_extraction` 是**另一条**流水线，不往这张表写任何东西——它的结构化产出合并进 `companion_insights`（并镜像到扁平的 `human_insights` 表），不在这里 embed。

所以 embedding 是每个实质轮次生成一次，不是「只有 LLM 挑出来的高光才生成」。

## 甚麼不被存

原始對話消息存在 `engine.chat_messages` 裡（完整逐字記錄、純文本）。它們 **不被** embed。記憶表存的是 *摘要* 跟 *事實*，不是完整消息日誌。想拿真實對話內容直接查 `chat_messages`——那才是「說了甚麼」的真相之源。

## 检索与注入

每轮对话都会把记忆读回 prompt，由每次请求的 `memory_scope` 控制（取值与默认值
见 [api-reference.zh.md](api-reference.zh.md)；默认 `neutral_and_relationship`）。回复
handler（`pipeline::handlers`）从两个来源拼出画像 / 关系上下文块：

- **画像层** —— 由两个来源合并。基础画像 bullet 来自扁平的 **`human_insights`**
  镜像表（从 `companion_insights` 同步过来），**不是**直接读 `companion_insights`
  JSONB；`memory_scope` 决定是否带上私密字段（`full` / `insights_only`）还是只带
  中性子集（`neutral_*`）。与之并列，画像层的 `companion_memories` 行也会按相似度
  检索出来并按 `category` 分组注入，但仅当 scope 保留画像记忆时才会跑（`full` /
  `neutral_and_relationship`）——`relationship_only`、`neutral_only`、
  `insights_only`、`none` 都会整段跳过这一半，只剩下 `human_insights` bullet
  （如果该 scope 下还有 bullet 的话）。
- **关系层** —— `companion_memories` 行，按对当前轮的语义（embedding）相似度
  检索拉取，在 scope 保留关系记忆时纳入（`full` / `neutral_and_relationship` /
  `relationship_only`）。

`memory_scope = none` 完全跳过记忆注入。**`memory_scope` 只管 prompt 注入，
不管写入。** 即使是 `none`，逐轮原文写入照样把这一轮 embed 并存下来，insight 抽取和
好感度评估也照常跑；目前没有任何 scope 取值能抑制写入。前端的
`/comp/user/{user_id}/profile` 端点返回 `companion_insights` JSONB，作为已收集
内容的人类可读视图。

### 语音轮次

语音端点（`POST /comp/voice/{session_id}/turn/stream`）读取同样的两层，但走
一条更精简的语音专属路径——请求体见
[api-reference.zh.md](api-reference.zh.md#post-compvoicesession_idturnstream)，
`[tasks.chat_voice]` 块见 [model-config.zh.md](model-config.zh.md)。

- **引导快照** —— 仅在 session 首轮组装一次，冻结进
  `chat_sessions.metadata.voice_bootstrap`，此后每轮原样重新注入（OpenRouter
  是无状态的，线路上不存在"只注入一次"这回事——之后的轮次也改不了它）。分两
  部分：默认中性档的 `human_insights` bullets（档位可由首轮的 `memory_scope`
  调整），加上上一通语音通话最后 8 条消息渲染出的纯文字记录。两部分各自独立
  降级；组装失败时标记位不落，下一轮重试。
- **逐轮召回** —— 与聊天路径相同的 `companion_memories` 检索（受
  `memory_scope` 门控，经 `memory_hygiene` 做跨层去重），但 K 值小得多：
  分组画像 1/类别 + 原始兜底 2 + 关系记忆 2（聊天路径是 2/4/3），整体包在
  300 ms 预算里——超时或检索出错只是丢掉这一轮的召回块，打一条 warn 日志，
  绝不是 error 帧。去掉空白和标点后不足 4 个字母数字字符的话语（嗯 / 好啊 /
  哈哈这类应和词）直接跳过召回，不发起 embedding 调用。部署方可以用
  `[tasks.chat_voice] recall = false`（默认 `true`）强制关闭，优先级高于
  请求里的 `memory_scope`。
- **只在通话结束后写入** —— 语音轮次进行中从不写入：没有逐轮原文写入，没有
  insight 抽取，也没有好感度评估。一通电话留下的东西是在它结束之后才写的：
  session 空闲满 `DREAMING_IDLE_SECS` 之后，dreaming-lite 清扫器会读它的通话
  记录，蒸馏出带 category 的画像层 `companion_memories` 行——和文字 session
  的处理完全一样。这些行就是普通记忆，之后的语音通话和文字聊天都能召回。
  语音助手行里的 TTS 音频标签（开了 `tts_audio_tags` 时的 `[laughs]`、
  `[sighs]`）会先被剥掉，舞台提示不会变成记忆文本。部署方可以用
  `DREAMING_VOICE_DISABLED=1` 整个关掉，恢复到早先只扫文字的过滤条件；注意
  在开关打开期间已被扫过的通话，关掉开关后不会重扫（`classified_at` 只盖一次）。
  关系层的行仍然从不由语音写入。

设计文档：
[2026-08-09-voice-memory-bootstrap-recall-design.md](superpowers/specs/2026-08-09-voice-memory-bootstrap-recall-design.md)
（引导快照 + 召回）与
[2026-08-09-voice-dreaming-ingestion-design.md](superpowers/specs/2026-08-09-voice-dreaming-ingestion-design.md)
（通话结束后的记忆写入）。

## 源碼

- `crates/eros-engine-store/src/memory.rs`——`MemoryRepo`（upsert + search，7 个 sqlx::test 集成测试）
- `crates/eros-engine-store/src/human_insight.rs`——注入时读取的扁平 `human_insights` 镜像
- `crates/eros-engine-llm/src/voyage.rs`——Voyage 原生客户端
- `crates/eros-engine-llm/src/embedding.rs`——`EmbeddingRouter` + OpenRouter 兼容客户端
- `crates/eros-engine-server/src/pipeline/post_process.rs`——逐轮原文写入路径
- `crates/eros-engine-server/src/pipeline/dreaming.rs`——dreaming-lite 清扫器
- `crates/eros-engine-server/src/pipeline/handlers.rs`——检索 + prompt 注入
- `crates/eros-engine-server/src/pipeline/voice.rs`——语音轮次的引导快照 + 逐轮召回
- `crates/eros-engine-store/migrations/0003_memory.sql`——schema + 索引 DDL
- `crates/eros-engine-store/migrations/0006_memory_category.sql`——`category` 列
- `crates/eros-engine-store/migrations/0032_companion_memories_metadata.sql`——`metadata` 列
