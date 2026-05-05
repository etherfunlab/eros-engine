# API 參考

[English](api-reference.md) · [中文](api-reference.zh.md)

任何運行中的實例 **`/docs`** 路徑下都有實時、可瀏覽的參考文檔（utoipa 註解生成的 Scalar UI）。在線 demo：<https://erosnx.etherfun.net/docs>。

這個頁面是手寫的端點摘要。Scalar UI 是權威 spec。

## 鑒權

每個 `/comp/*` 端點都需要 `Authorization: Bearer <Supabase JWT>`。JWT 必須是 HS256 簽名、密鑰為 `SUPABASE_JWT_SECRET`。`sub` claim 必須是個 UUID；該 UUID 即該請求的 user_id。

`/healthz` 跟 `/docs` 是公開的。

## 公開端點

### `GET /healthz`

存活探針。無需鑒權。

```bash
curl https://erosnx.etherfun.net/healthz
```

```json
{
  "status": "ok",
  "service": "eros-engine",
  "version": "0.1.0",
  "timestamp": "2026-05-05T19:06:05.309302232+00:00"
}
```

## 人格

### `GET /comp/personas`

列出所有處於 active 狀態的人格基因。需鑒權。

```bash
curl -H "Authorization: Bearer $JWT" \
  https://erosnx.etherfun.net/comp/personas
```

```json
{
  "personas": [
    {
      "id": "11d6a45a-1fd9-4fe6-a943-3f049035eb68",
      "name": "Aria",
      "system_prompt": "…",
      "tip_personality": "warm-but-reserved",
      "avatar_url": "https://avatars.etherfun.xyz/aria.png",
      "art_metadata": { "age": 27, "mbti": "INFJ", "model": "x-ai/grok-4-fast", … },
      "is_active": true
    }
  ]
}
```

## 對話生命周期

### `POST /comp/chat/start`

對指定人格基因開新 chat session。如果 `(genome_id, jwt_user_id)` 對應的 `persona_instance` 還不存在，服務器先建一個，然後建一個引用該 instance 的 `chat_session`。

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"genome_id":"11d6a45a-1fd9-4fe6-a943-3f049035eb68"}' \
  https://erosnx.etherfun.net/comp/chat/start
```

```json
{
  "session_id": "5f7e…",
  "persona_name": "Aria",
  "is_new": true
}
```

`is_new=false` 表示同一用戶用同一個 `genome_id` 再調 `/start`——引擎恢復已有 session 而不是建重複的。

### `POST /comp/chat/{session_id}/message`

同步對話一輪。阻塞到 LLM 響應為止。

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{"content":"嗨，今天讀甚麼書？"}' \
  https://erosnx.etherfun.net/comp/chat/<session_id>/message
```

```json
{
  "reply": "Bishop。三月份我總是回到的同一本。",
  "lead_score": 4.2,
  "should_show_cta": false,
  "typing_delay_ms": 1340,
  "agent_training_level": 0.18
}
```

`reply: null` 意味著人格在這一輪 ghost 了你（看 [Ghost 機制](ghost-mechanics.zh.md)）。HTTP 狀態仍是 200。

### `POST /comp/chat/{session_id}/message_async`

跟 `/message` 形狀一樣，但立即返回一個 `message_id`。LLM 調用在後台跑；輪詢 `/pending/{message_id}` 直到 ready。

### `GET /comp/chat/{session_id}/pending/{message_id}`

```json
{ "ready": false }
```

或者：

```json
{ "ready": true, "reply": { /* 跟 /message 響應形狀一致 */ } }
```

### `GET /comp/chat/{session_id}/history?limit=50&offset=0`

分頁讀消息歷史，最新在前。

```json
{
  "messages": [
    { "id": "…", "role": "assistant", "content": "Bishop。", "sent_at": "…" },
    { "id": "…", "role": "user",      "content": "嗨…",     "sent_at": "…" }
  ]
}
```

`role` ∈ `user | assistant | gift_user | system_error`。

## 用戶畫像

### `GET /comp/chat/{user_id}/sessions`

該 `user_id` 名下的所有 chat sessions。路徑裡的 `user_id` **必須** 等於 JWT 裡的 user_id；否則 403。

### `GET /comp/user/{user_id}/profile`

當前的 `companion_insights` JSONB 加上加權的 `training_level`。同樣的 `user_id` 等值檢查。

```json
{
  "insights": {
    "city": "Hong Kong",
    "occupation": "graphic designer",
    "interests": ["jazz", "long walks"],
    "mbti_guess": "INFP"
  },
  "training_level": 0.42
}
```

`training_level` 是九個字段加權後的分數（city 0.05、occupation 0.05、interests 0.10、mbti_guess 0.15、love_values 0.15、emotional_needs 0.15、life_rhythm 0.10、personality_traits 0.15、matching_preferences 0.10）。權重總和為 1.0。

## 禮物事件

### `POST /comp/chat/{session_id}/event/gift`

把外部事件帶來的好感度 deltas 應用上去（虛擬禮物、表情反應、任何你想建模成「用戶剛做了件好事」的事）。路由會寫一條 `chat_messages`（`role='gift_user'`）並通過好感度持久化路徑應用 deltas。

```bash
curl -X POST -H "Authorization: Bearer $JWT" -H "Content-Type: application/json" \
  -d '{
        "deltas": {"warmth": 0.05, "intimacy": 0.03, "tension": -0.02},
        "label": "玫瑰",
        "metadata": {"source": "frontend-shop", "amount": 100}
      }' \
  https://erosnx.etherfun.net/comp/chat/<session_id>/event/gift
```

v0.1 的禮物路由 **不會** 觸發 LLM 反應（`reply` 為 `null`）。人格在用戶下一輪消息裡承認這份禮物，那時新的好感度狀態塑造回覆。同步反應變體是後續增強。

### `GET /comp/chat/{session_id}/gifts`

列出該 session 的所有禮物事件，分頁。

## Debug

### `GET /comp/affinity/{session_id}`

實時 6 維向量 + ghost 統計 + 關係標籤。受 `EXPOSE_AFFINITY_DEBUG=true` 環境變量控制；關閉時返 404。

```json
{
  "warmth": 0.42,
  "trust": 0.28,
  "intrigue": 0.61,
  "intimacy": 0.15,
  "patience": 0.55,
  "tension": 0.18,
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "stranger",
  "updated_at": "2026-05-05T19:42:00.000000Z"
}
```

生產部署通常關著（好感度向量是魔法的一部份——把它暴露出來會破壞錯覺）。如果你的前端想實時畫好感度雷達圖，再把它打開。

## 錯誤響應

所有錯誤都是 JSON 形狀 `{"error": "<code>", "message": "<人類可讀>"}`：

| 狀態碼 | code | 何時 |
|--------|------|------|
| 400 | `bad_request` | 請求體格式錯、UUID 無效、缺必填字段 |
| 401 | `unauthorized` | JWT 缺失 / 格式錯 / 過期 / 密鑰不符 |
| 403 | `forbidden` | 路徑 user 跟 JWT user 不匹配，或想讀別人的 session |
| 404 | `not_found` | session / 人格 / 消息 id 不存在 |
| 500 | `internal` | 其餘一切（DB 錯、LLM API 錯等） |

## 源碼

- `crates/eros-engine-server/src/routes/companion.rs`——handler 實現
- `crates/eros-engine-server/src/routes/debug.rs`——好感度 debug 路由
- `crates/eros-engine-server/src/routes/health.rs`——`/healthz`
- `crates/eros-engine-server/src/openapi.rs`——Scalar UI spec 元數據
