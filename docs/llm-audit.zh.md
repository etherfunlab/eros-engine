# LLM audit 透传

eros-engine 在 chat 消息路由上暴露一个不透明的 OpenRouter 透传层。
三个 caller 提供的字段原样发给
`openrouter.ai/api/v1/chat/completions`，三个 OpenRouter 的 wire 回显
在 sync 响应里带回来，两个 deployer 设的环境变量会给每次出站调用都
带上 app-attribution headers。

引擎不解读内容。PII 脱敏、hash、metadata 语义都是 caller 的责任。

## Inbound：请求体的 `audit` 字段

`POST /comp/chat/{session_id}/message` 和
`POST /comp/chat/{session_id}/message_async` 都接受可选的 `audit`
对象：

```jsonc
{
  "message": "...",
  "audit": {
    "user": "u_<hash-of-internal-id>",     // 可选
    "session_id": "conv_xyz",               // 可选，与 URL 上的 session UUID 不同
    "metadata": {                           // 可选
      "feature": "chat",
      "plan": "pro"
    }
  }
}
```

引擎在转发前强制的上限：

| 字段                    | 上限                                                  |
|-------------------------|-------------------------------------------------------|
| `audit.user`            | `chars ≤ 256`                                         |
| `audit.session_id`      | `chars ≤ 256`                                         |
| `audit.metadata` key 数 | `≤ 16`                                                |
| `audit.metadata` key    | 正则 `^[A-Za-z0-9_.-]{1,64}$`                         |
| `audit.metadata` value  | JSON string，`chars ≤ 512`                            |

违反返回 `400 BadRequest`，且不会写任何 user message 行。

**隐私：**不要把原始 email / 钱包地址 / 真实姓名放进 `user` ——
要送 hash。OpenRouter 默认保留 request metadata（token 数、延迟），
但不保留 prompt / response 内容。

## Outbound：sync 响应里的 `usage` 回显

`POST /comp/chat/{session_id}/message` 在 200 响应体里增加三个可选
字段：

| 字段            | 类型      | 含义                                                                                       |
|-----------------|-----------|--------------------------------------------------------------------------------------------|
| `usage`         | `object?` | OpenRouter 的 `usage` 块原样（tokens / cost / cached / reasoning）。引擎不展平。           |
| `generation_id` | `string?` | OpenRouter `response.id`。之后可以用它查 `/api/v1/generation` 拿完整 metadata。          |
| `model`         | `string?` | OpenRouter 实际服务的模型。`fallback_model` 命中时，这里是 fallback。                    |

异步路由（`/message_async`）和轮询路由（`/pending/{message_id}`）
**不带**这三个字段。需要 per-turn audit 数据请走 sync。

### 从响应里剔除字段

Deployer 可以在服务器上设
`OPENROUTER_USAGE_HIDDEN_KEYS`（逗号分隔）来把 `usage` 回显里指定的顶层
key 剔除掉。典型用途：把批发的 `cost` / `cost_details` 对下游客户隐藏，
同时不影响运维侧可见性。

```bash
OPENROUTER_USAGE_HIDDEN_KEYS=cost,cost_details
```

行为：

- 只作用于 sync `/comp/chat/{id}/message` 的响应。
- **不**影响 `tracing::info!` 输出 —— 运维可见性照旧。
- 异步路由（`/message_async`）、轮询路由、后台路径（dreaming /
  post_process）本来就不把 `usage` 返回给 client，env 设了也没区别。
- 只剔除顶层 key；要把整个子树抹掉就列父 key（`cost_details` 会把整个
  对象删掉，而不是只删它内部的字段）。
- 未设或空 → 维持现状（完整透传）。

后台路径（`pipeline::dreaming`、`pipeline::post_process`）的 usage
只通过 `tracing::info!` 字段输出：

```
openrouter: call completed session=… generation_id=… model=…
prompt_tokens=… completion_tokens=… total_tokens=… cost=…
```

## App-attribution headers

两个可选环境变量给每次出站 OpenRouter 调用加 header：

| Env                       | Header         | 用途                                          |
|---------------------------|----------------|-----------------------------------------------|
| `OPENROUTER_APP_REFERER`  | `HTTP-Referer` | OpenRouter 仪表盘上的 app 标识                |
| `OPENROUTER_APP_TITLE`    | `X-Title`      | OpenRouter app analytics 里显示的名字         |

两个都不设 → 维持现状（不发任何 attribution header）。它们是
deployment 级别的设置，不是 per-request —— App-Attribution 的目的是
app-level 聚合。Per-user 维度走 `audit.user`。

非法值（控制字符、非 ASCII 之类）在构造时被丢弃并打一条
`tracing::warn!`，client 仍然可用。

## 引擎不做的事

- **不持久化。**没有任何 DB 列保存 `audit` / `usage` / attribution。
  只有 surface 字段。
- **不 hash。**引擎不会变换 `user` —— caller 负责送 hash。
- **不消毒。**`metadata` 的 key / value 只检查 size / shape，不查内容。
- **不解读。**引擎不会按 audit 字段分组、聚合、报警。Caller 自己接。

## 可观测性

收到 `audit` 时，引擎会打 info 级别日志，带
`audit_user_present` / `audit_session_present` / `audit_metadata_keys`
（只有 key，永远不带 value）。每次成功的 OpenRouter 调用也会日志
`generation_id` / `model` 以及 best-effort 解析出来的 token / cost。

## 为什么不持久化？

引擎的 persona / chat / affinity 表是长期契约。Audit 上下文有意做成
短暂的，让 caller 可以自由实验 `user` hash、metadata schema、不同
deployment 的 analytics，不污染引擎表也不需要 migration。
