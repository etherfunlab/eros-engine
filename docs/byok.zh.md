# 自带密钥（BYOK）

[English](byok.md) · [中文](byok.zh.md)

下游可以让它的终端用户自带兼容 OpenAI 的聊天端点和 API key。带有 `byok` 块的一轮对话，伴侣的回复由终端用户自己的账户生成。引擎不保留该块的任何部分：URL 和 key 只存在于使用它们的那一轮的内存里。

设计文档：[BYOK chat spec](superpowers/specs/2026-10-08-byok-chat-design.md)。

## 开启方式

| 环境变量 | 含义 |
|---|---|
| `BYOK_CALLER_SECRET` | 开启 BYOK。至少 32 字节；更短的值会导致启动失败。未设置 ⇒ 所有 `byok` 块都返回 `403 byok_forbidden`。 |
| `BYOK_ALLOW_PRIVATE_NETWORK` | `true` 时解除地址防护并允许 `http` 端点。用于自托管网络和测试。 |

引擎用终端用户自己的 bearer token 鉴权，因此终端用户可以直接调用引擎。只有请求同时带有 `X-Byok-Caller-Secret: <BYOK_CALLER_SECRET>` 时，`byok` 块才会被采纳。请把这个 secret 留在下游的服务器上，由服务器添加该 header；绝不要发给客户端。

## BYOK 覆盖的范围

只覆盖 `POST /comp/chat/{session_id}/message/stream` 上的回复，包括打赏轮和 `reply_text_image` 的文字部分。PDE 判断器、输入与输出 filter、vision、图片 prompt 合成、产品问答、亲密度、insight 与记忆提取，以及 embedding，仍然运行在部署自己的链路和 key 上。

异步路由收到 `byok` 块会返回 `400 invalid_payload`。`/open` 开场白、语音轮次和图片编辑都不接受该块。

## `byok` 块

```json
"byok": {
  "providers": {
    "mine": {
      "chat": "https://api.example.com/v1/chat/completions",
      "api_key": "sk-…",
      "headers": { "X-Org": "…" }
    }
  },
  "model": "gpt-x@mine",
  "fallback": ["other-model@mine"],
  "retry_depth": 1,
  "output_regex": [{ "pattern": "\\s*\\[note:[^\\]]*\\]\\s*$" }],
  "fallback_to_platform": false
}
```

| 字段 | 类型 | 必填 | 含义 |
|---|---|---|---|
| `providers` | map 名称 → provider | 是 | 终端用户的端点。 |
| `providers.<name>.chat` | string | 是 | 完整的 chat-completions URL，原样 POST。 |
| `providers.<name>.api_key` | string | 是 | 以 `Authorization: Bearer <api_key>` 发送。 |
| `providers.<name>.headers` | map | 否 | 对该 provider 的每个请求原样附带。 |
| `model` | string \| array \| map | 是 | 固定、轮询或加权随机——与 `[tasks.*].model` 的形式相同。 |
| `fallback` | string \| array | 否 | 顺序 fallback 链。 |
| `retry_depth` | integer | 否 | primary 之后尝试的 fallback 数。默认 `2`。 |
| `output_regex` | array | 否 | 本链路回复的剥除规则；省略 `models` ⇒ 作用于链上所有模型。 |
| `fallback_to_platform` | bool | 否 | 默认 `false`。 |

`model` 和 `fallback` 里的每个 slug 都必须以 `@<name>` 结尾，且 `<name>` 是 `providers` 中的一个。裸 slug、`@openrouter` 后缀，或部署 `[providers]` 里的名字，一律拒绝，因此 BYOK 轮次永远不会花部署的 key。`allow_traits`、显示覆盖、`temperature`、`max_tokens`、采样参数、`output_filter` 和 body 参数不接受出现在该块里；它们仍然来自 `[tasks.chat_companion]` 和请求的 `tier`。

## 校验

所有检查都在写入用户行之前完成。失败返回 `400 invalid_payload`，其 message 只指出字段名，不含 key、header 值或 URL。

| 项目 | 规则 |
|---|---|
| `providers` | 1–4 项；名称匹配 `[a-z0-9_]{1,32}`；`openrouter` 为保留字 |
| `chat` | ≤ 2048 字节；https（仅在 `BYOK_ALLOW_PRIVATE_NETWORK` 下允许 http）；URL 中不得带凭据；字面 IP 主机必须通过地址防护 |
| `api_key` | 1–1024 字节的合法 header 文本 |
| `headers` | ≤ 8 项，值 ≤ 1024 字节；拒绝 `Authorization` 和 `Content-Type` |
| `model`、`fallback` | 各 ≤ 8 项；权重为有限数且 > 0，总和也为有限数 |
| `output_regex` | ≤ 16 条规则；pattern 1–512 字符；replacement ≤ 512 字符；编译时受 1 MiB 体积限制 |

## 链路如何运行

- **Primary。** 与配置里一样：固定、轮询或加权。轮询游标按终端用户、按进程保存；重启后重置，每个副本各自计数。
- **Fallback。** 选中的 primary 会从 `fallback` 中剔除，再按 `retry_depth` 截断。
- **`fallback_to_platform: true`。** 所有 BYOK hop 都失败后，继续走部署中对应请求 `tier` 的链路。
- **耗尽。** 纯 BYOK 链路全部失败时，以 `error` frame 结束，携带最后一个 hop 的 `upstream_status` 和 `provider_code`。它绝不会给出部署的兜底话术，因为那会向终端用户隐瞒其 key 或端点出了问题。这一轮不会重试；每个失败的 hop 都会在历史里留下一个被截断的气泡，队列行落为 `done`，与任何所有 hop 都失败的实时链路相同。
- **`meta.model`。** BYOK hop 始终显示真实的模型 id，实时流与重放都一样；`model_name_display_override` 只作用于部署自己的 hop。
- **剥除规则。** BYOK hop 使用该块的 `output_regex`；部署的 hop 使用配置里的。
- **参数。** BYOK hop 在严格的 OpenAI 兼容 wire 上，收到部署 `chat_companion` 的 `temperature`、`max_tokens` 和采样参数。若 provider 拒绝其中某项，该 hop 以 provider 自己的 400 失败。

## 引擎连接到哪里

BYOK 请求通过专用客户端发出：不跟随重定向，不使用代理，只允许 https。先解析主机名；loopback、私有、link-local、CGNAT、unique-local、未指定和多播地址（包括 IPv4-mapped 和 NAT64 形式）一律拒绝，连接只会发往通过检查的地址。被拒绝的主机会让该 hop 以 `transport` 网关错误失败。`BYOK_ALLOW_PRIVATE_NETWORK` 会解除以上全部限制。

## 记录什么

- BYOK hop 在 `engine.llm_generations.model`、`llm_attempts[].model`、`gateway_errors[].model` 以及 prompt log 里记为 `<id>@byok`。终端用户选的 provider 名称不会被记录。`byok` 是保留的 `[providers]` 名称，因此这个标签永远表示 BYOK hop。
- 某一轮请求过 BYOK 这件事只在它的队列行上记录一次：`engine.chat_turn_queue.params->'byok'` = `{"fallback_to_platform": …}`。
- URL、key 和 header 值从不记录：不进表、不进队列 params、不进日志、不进 prompt log。回显了 key 的 provider 错误，存储时 key 会被替换为 `<redacted>`。

如果一次部署打断了某个 BYOK 轮次，接手恢复的 worker 已经没有该块：它会让这一轮失败（`last_error = 'byok_unavailable'`），或者在设了 `fallback_to_platform` 时，用部署自己的链路来回答。

## 安全

BYOK 轮次会把完整的回复 prompt——角色定义、召回的记忆、insight、世界状态——发给终端用户控制的端点，终端用户可以读到全部内容。哪些角色、tier 和终端用户可以使用 BYOK，由下游决定。
