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
    category     TEXT,                         -- fact|preference|event|emotion|relation；逐轮写入的行为 NULL
    metadata     JSONB,                        -- 每条记忆的不透明抽取载荷；逐轮写入的行为 NULL
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

默认 `voyage-4-lite` 走 Voyage API（可在 `[tasks.embedding]` 配置）。512 维、多语言。

```rust
// crates/eros-engine-llm/src/voyage.rs
pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError>;
pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError>;
```

`embed_document` 跟 `embed_query` 給 Voyage 不同的 `input_type` 提示——documents 為入庫檢索優化、queries 為餘弦匹配優化。所以引擎是兩個方法不是一個。

引擎在 `VOYAGE_API_KEY` 為空時 **大聲拒絕啟動**——缺密鑰直接拒絕 boot。閉源版的 eros-gateway 有個已知回歸：空 key 會悄悄關掉 embeddings；eros-engine 拒絕繼承這個坑。

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
SELECT id, content, 1 - (embedding <=> $2::vector) AS similarity
FROM engine.companion_memories
WHERE user_id = $1 AND instance_id IS NULL
ORDER BY embedding <=> $2::vector
LIMIT $3;
```

Relationship 層查詢加 `instance_id = $4`。`1 - distance` 讓你直接按相似度排序或閾值處理，不用記住 pgvector 是「距離」不是「相似度」這個約定。

`lists = 100` 是中小規模表（≲ 1M 行）的平衡默認值。語料更大就調高（經驗法則：`lists ≈ √rows`）。

## 甚麼會被 embed

两个互相独立的写入方，跑在不同的节奏上：

1. **逐轮写入**（`write_turn`，post-process）——在**每一个**实质轮次都跑（用户话非空，且至少有一条非空的助手消息）。它只把**用户的那句话**embed 进*两个*层——绝不存助手的散文，因为那会通过召回反馈进模型自己的 prompt，把回复塌缩成一句反复出现的话（issue #113）。关系层那份前面加 `用户：`，让召回出来的行仍读得出「这是用户说的」。这些行的 `category` 和 `metadata` 都是 NULL。
2. **Dreaming-lite 清扫**（`[tasks.memory_extraction]`，`pipeline::dreaming`）——后台的、由 session 闲置触发的一遍，问 LLM 要值得长期保留的记忆候选。它**只写 profile 层**，带 `category ∈ {fact, preference, event, emotion, relation}`（模型自创的其它类别一律收敛成 `fact`），外加一份不透明的 `metadata`。

`insight_extraction` 是**另一条**流水线，不往这张表写任何东西——它的结构化产出合并进 `companion_insights`（并镜像到扁平的 `human_insights` 表），不在这里 embed。

所以 embedding 是每个实质轮次生成一次，不是「只有 LLM 挑出来的高光才生成」。

## 甚麼不被存

原始對話消息存在 `engine.chat_messages` 裡（完整逐字記錄、純文本）。它們 **不被** embed。記憶表存的是 *摘要* 跟 *事實*，不是完整消息日誌。想拿真實對話內容直接查 `chat_messages`——那才是「說了甚麼」的真相之源。

## 检索与注入

每轮对话都会把记忆读回 prompt，由每次请求的 `memory_scope` 控制（取值与默认值
见 [api-reference.md](api-reference.md)；默认 `neutral_and_relationship`）。回复
handler（`pipeline::handlers`）从两个来源拼出画像 / 关系上下文块：

- **画像层** —— 由两个来源合并。基础画像 bullet 来自扁平的 **`human_insights`**
  镜像表（从 `companion_insights` 同步过来），**不是**直接读 `companion_insights`
  JSONB；`memory_scope` 决定是否带上私密字段（`full` / `insights_only`）还是只带
  中性子集（`neutral_*`）。与之并列，画像层的 `companion_memories` 行也会按相似度
  检索出来并按 `category` 分组注入。注意私密/中性这个区分只作用于 `human_insights`
  bullet——它**不**过滤哪些记忆类别会被注入。
- **关系层** —— `companion_memories` 行，按对当前轮的语义（embedding）相似度
  检索拉取，在 scope 保留关系记忆时纳入（`full` / `neutral_and_relationship` /
  `relationship_only`）。

`memory_scope = none` 完全跳过记忆注入。**`memory_scope` 只管 prompt 注入，
不管写入。** 即使是 `none`，逐轮写入照样把这一轮 embed 并存下来，insight 抽取和
好感度评估也照常跑；目前没有任何 scope 取值能抑制写入。前端的
`/comp/user/{user_id}/profile` 端点返回 `companion_insights` JSONB，作为已收集
内容的人类可读视图。

## 源碼

- `crates/eros-engine-store/src/memory.rs`——`MemoryRepo`（upsert + search，3 個 sqlx::test 集成測試）
- `crates/eros-engine-llm/src/voyage.rs`——embedding 客戶端
- `crates/eros-engine-server/src/pipeline/post_process.rs`——寫入路徑
- `crates/eros-engine-store/migrations/0003_memory.sql`——schema + 索引 DDL
