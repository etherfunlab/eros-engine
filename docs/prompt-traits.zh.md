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
system prompt 的 `[additional_guidance]` 段落下，位置在 `[topics]` 与
`[turn_style]` 之间。空列表 → 该段落整个不渲染，输出与旧 prompt
逐字节一致。

## 引擎不会做什么

- **不持久化。** trait 不会写进任何数据库表，每轮请求都由调用方重新提交。
- **不解释内容。** 引擎把 `text` 当不透明字符串处理。内容过滤、
  用户同意流程都是调用方的责任。
- **不带语义类别。** `tag` 仅供日志/指标用，引擎不会因为 tag 是
  `"nsfw_boost"` 还是 `"politics_open"` 而做任何区别处理。
- **引擎侧白名单（纵深防御）。** 引擎现在会丢弃不在当前 tier 的
  `allow_traits` 列表中的 trait tag —— 这些 tag 不会注入 system prompt，
  但回复仍然正常生成。调用方侧的拦截仍然是主要防线；引擎侧过滤是纵深防御。

## Tier 过滤

不在请求 tier 的 `allow_traits` 列表中的 tag 不会被注入 system prompt。具体来说:

- 被丢弃的 tag 会被记录日志（只记 tag 名，绝不记 `text` 正文）。
- 即使所有 trait 都被丢弃，回复也会正常生成。
- 在 `model_config.toml` 里按 tier 配置白名单:
  ```toml
  [tasks.chat_companion.tiers.gold]
  allow_traits = ["allow_nsfw", "allow_politics"]
  ```
- **三态语义:**
  - `allow_traits` 缺省 —— 不过滤，所有 trait 注入。
  - `allow_traits = []` —— 丢弃所有 trait。
  - `allow_traits = ["a", "b"]` —— 白名单，只注入列表中的 tag。
- 请求的 `tier` 字段选择 tier 块（见
  [api-reference.zh.md](api-reference.zh.md)）。
  tier 未知或缺省时使用任务默认块的 `allow_traits`。

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

被入侵的客户端可以通过 `text` 尝试 prompt-injection。引擎现在通过
`allow_traits` 做服务端 tag 过滤（见上方"Tier 过滤"），但防护 persona
内部状态最终是调用方的责任。部署层的 tag 白名单是主要防线；引擎侧过滤是纵深防御。

## 为什么不持久化？

引擎的 persona 表是长期契约。trait 故意保持瞬态，让调用方的实验、A/B
测试和按会话的策略不会污染 persona 数据，也不需要 migration。
