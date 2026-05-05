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

`voyage-3-lite` 走 Voyage API。512 維、多語言、約每百萬輸入 token $0.02 美元。

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

post-process 在每輪的後台階段插入記憶。兩條路徑：

1. **Insight 抽取**——LLM 識別出值得記住的事實小塊（「用戶提到自己是圖書管理員」）。這些進 profile 層（`instance_id = NULL`）。
2. **Relationship 時刻**——任何 session 特有的東西（人格剛說過的回呼、用戶吐露的小心事）。進 relationship 層。

不是每條消息都會被 embed——只有 insight 抽取器標出來值得記住的才會。容量保持節制。

## 甚麼不被存

原始對話消息存在 `engine.chat_messages` 裡（完整逐字記錄、純文本）。它們 **不被** embed。記憶表存的是 *摘要* 跟 *事實*，不是完整消息日誌。想拿真實對話內容直接查 `chat_messages`——那才是「說了甚麼」的真相之源。

## 懶式檢索

pipeline 在每輪對話之前 **不會** 主動拉記憶。system prompt 只用人格基因 + 好感度向量 + 關係標籤構建。記憶會在 LLM（在 insight 或 chat 任務裡）通過將來的 tool-use API 主動要的時候才浮上來——v0.1 還沒接，計劃在後續階段做。

目前記憶是寫多讀少：引擎累積一份結構化的關係記錄，把它再餵回 LLM 是另一條工作流。前端的 `/comp/user/{user_id}/profile` 端點返回結構化的 `companion_insights` JSONB，那是已收集內容的人類可讀視圖。

## 源碼

- `crates/eros-engine-store/src/memory.rs`——`MemoryRepo`（upsert + search，3 個 sqlx::test 集成測試）
- `crates/eros-engine-llm/src/voyage.rs`——embedding 客戶端
- `crates/eros-engine-server/src/pipeline/post_process.rs`——寫入路徑
- `crates/eros-engine-store/migrations/0003_memory.sql`——schema + 索引 DDL
