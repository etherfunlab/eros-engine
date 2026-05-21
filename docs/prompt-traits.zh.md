# Prompt traits（提示注入层）

一个面向请求的 hook，让调用方在不修改 persona 数据的前提下，往 system
prompt 里塞入自定义段落。

## 请求结构

```jsonc
{
  "content": "...",
  "client_msg_id": "01J3333333333333333333333A",
  "prompt_traits": [
    { "tag": "ascii_identifier", "text": "要注入的文字" }
  ]
}
```

接受于 `POST /comp/chat/{session_id}/message/stream`。

## 引擎会做什么

每轮请求中，所有 trait 的 `text` 会被渲染为 bullet 列表，放在 persona
system prompt 的 `【附加指引】` 段落下，位置在 `【擅长话题】` 与
`【今日情境】` 之间。空列表 → 该段落整个不渲染，输出与旧 prompt
逐字节一致。

## 引擎不会做什么

- **不持久化。** trait 不会写进任何数据库表，每轮请求都由调用方重新提交。
- **不解释内容。** 引擎把 `text` 当不透明字符串处理。内容过滤、白名单、
  用户同意流程都是调用方的责任。
- **不带语义类别。** `tag` 仅供日志/指标用，引擎不会因为 tag 是
  `"nsfw_boost"` 还是 `"politics_open"` 而做任何区别处理。

## 限制

| 字段                  | 限制                                  |
|----------------------|---------------------------------------|
| `prompt_traits` 数量 | ≤ 8                                   |
| `tag`                | 正则 `^[a-z0-9_]{1,32}$`              |
| `text`               | trim 后 1 ≤ 字符数 ≤ 2000             |
| `text` 内容          | 不允许控制字符（含 `\n`）             |

违反规则返回 `400 BadRequest`，且**不会写入任何用户消息行**。

## 可观测性

只要有至少一条 trait 被提交，引擎会输出 `info` 级日志，字段包含
`traits_count` 和 `trait_tags`。`text` 内容**不**进日志。

## 威胁模型

被入侵的客户端可以通过 `text` 尝试 prompt-injection。引擎只是传输层，
不负责防护 persona 内部状态 —— 这一层防御由调用方承担。如需限制 tag
名称白名单，可在部署层加一层 reverse proxy / middleware。

## 为什么不持久化？

引擎的 persona 表是长期契约。trait 故意保持瞬态，让调用方的实验、A/B
测试和按会话的策略不会污染 persona 数据，也不需要 migration。
