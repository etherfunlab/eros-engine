# 自带角色（BYOC）

[English](byoc.md) · [中文](byoc.zh.md)

下游可以让它的终端用户带来一个引擎从不存储的角色。卡片由下游自己保管，存在哪里都行。引擎保存实例、对话产生的一切内容，以及每个实例一把密钥。每个需要角色的请求都带上用该密钥封装的完整卡片；引擎只为这一次请求打开它，不保留任何部分。

导入的卡片永远是一个新角色：一个新实例，不带任何其他实例的历史。引擎不知道卡片从哪里来。

设计文档：[BYOC design](superpowers/specs/2026-10-16-byoc-design.md)。卡片格式：[Eros 角色卡 v1](eros-character-v1.md)。

## 开启方式

| 环境变量 | 含义 |
|---|---|
| `BYOC_CALLER_SECRET` | 开启 BYOC。至少 32 字节；更短的值会导致启动失败。未设置或为空 ⇒ BYOC 关闭：两个密钥路由，以及带有 `character` 的卡片路由请求，都返回 `403 byoc_forbidden`。 |

引擎用终端用户自己的 bearer token 鉴权，因此终端用户可以直接调用引擎。密钥路由和任何带有 `character` 的请求，只有同时带有 `X-Byoc-Caller-Secret: <BYOC_CALLER_SECRET>` 时才会被采纳，比较以常数时间进行。请把这个 secret 留在下游的服务器上，由服务器添加该 header；绝不要发给客户端。在密钥路由上，以及在带有 `character` 的卡片路由请求上，准入检查先于对请求字段的任何检查，所以 secret 缺失或不对时，无论 body 的字段还有什么问题，都是 `403 byoc_forbidden`。缺少 bearer token，或 body 无法反序列化，会在准入之前就被拒绝。

## BYOC 覆盖的范围

BYOC 实例上接受卡片的路由：

- `POST /comp/chat/{session_id}/message/stream`——整轮对话：回复 prompt、PDE 判断器、本轮的 `image_request`（卡片的 `appearance`）、打赏（`tip_personality`）、预设物品，以及轮后的亲密度评估。BYOK 可以与之叠加：卡片提供角色，`byok` 块提供回复模型（见 [byok.zh.md](byok.zh.md)）。
- `POST /persona/{instance_id}/image/compose`——composer 的角色输入。
- `POST /v2/comp/session/{session_id}/message/{message_id}/image/edit`——图片编辑的角色输入。

不需要卡片、也不需要 caller secret 就能在 BYOC 实例上工作的路由，因为它们只读引擎自己拥有的行：带 `instance_id` 的 `POST /comp/chat/start`、历史、会话列表、已读回执、reaction、会话 effects、`GET …/image-request`、角色与用户 insight、角色 profile、物品，以及归档。之后即使撤掉 secret，它们照常工作。`GET /v2/comp/instance/{instance_id}/items` 只列出实例自己的行；预设的 `possessions` 和 `wardrobe` 在卡片里，下游本来就持有。物品的 `POST` 和 `DELETE` 不变。

拒绝 BYOC 实例的路由，在写入任何内容之前返回 `400 byoc_unsupported`，message 里写明路由名：

- `POST /v2/comp/session/{session_id}/message/async`——它的轮次稍后由 worker 生成，而 worker 从不持有卡片。该路由对任何带有 `character` 的请求返回 `400 invalid_payload`。
- `POST /v2/comp/session/{session_id}/open`
- `POST /comp/voice/{session_id}/turn/stream`
- `POST /v2/comp/session/{session_id}/events`（生活事件）
- 群聊：创建含 BYOC 成员的群，或向群里添加 BYOC 成员。

World Memories、World Town 和 World Stories 从不包含 BYOC 实例。

### 客户端看到什么

BYOC 实例没有 genome，所以有两个响应字段没有值可填：

- `POST /comp/chat/start` 和 `POST /bff/v1/comp/chat/start` 的 `persona_name` 为 `null`。名字在卡片里。
- `GET /bff/v1/comp/affinities/{user_id}` 每个条目的 `genome_id` 为 `null`。每个条目都带有 `instance_id`，客户端可以用它为两种关系建键。

## 创建实例

两次调用，都带用户的 bearer token 和 caller secret。第一次返回密钥；下游存好密钥之后，第二次把它变成实例。

```bash
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  http://localhost:8080/v2/comp/byoc/init
```

```json
{ "key": "<base64url, 32 bytes>", "ticket": "<base64url, 32 bytes>" }
```

init 不写任何内容。先保存 `key`，再调用 create：

```bash
curl -X POST -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  -H "Content-Type: application/json" \
  -d '{"key":"<key>","ticket":"<ticket>"}' \
  http://localhost:8080/v2/comp/byoc/create
```

```json
{ "instance_id": "8a1f0c2e-4b6d-4f8a-9c31-2d5e7f0a1b3c" }
```

- ticket 是针对 bearer 所属用户计算的 `HMAC-SHA256(BYOC_CALLER_SECRET, "eros-byoc-init-v1" ‖ user_id ‖ key)`，其中 `user_id` 以其 16 个原始字节进入 HMAC。create 会重新计算它，所以引擎没有签发给该用户的 key 无法创建实例：`403 byoc_forbidden`。
- `key` 和 `ticket` 是不带 padding 的 base64url；输入时接受 padding。解码后不是 32 字节的一律 `400 invalid_payload`。
- create 对每个 key 幂等。重发同一组 `key` 和 `ticket` 会返回同一个 `instance_id`，所以丢了响应的下游可以放心重发。重新 create 一个已归档的 BYOC 实例会把它重新激活并返回同一个 id，因此归档之后，已保存的 key 依然有效。
- 一个用户可以持有任意多个 BYOC 实例。
- 引擎不会再次展示这把 key，v1 也没有轮换和找回。丢了 key 的下游无法再对该实例使用卡片路由；它只能创建一个新实例，也就是一个没有历史的新角色。

create 之后，用 `POST /comp/chat/start` 加上新的 `instance_id` 开会话，与任何显式指定的实例一样。

## 封装卡片

明文是 [Eros 角色卡 v1](eros-character-v1.md) 的 UTF-8 JSON，最多 64 KiB。第三方格式由下游在封装之前转换。

```
character = base64url( nonce[12] ‖ AES-256-GCM(key, nonce, plaintext) ‖ tag[16] )
```

- `key` 是 init 返回的 32 字节。
- 不带关联数据。每次封装都用新的随机 nonce。
- 编码时不带 padding；引擎在输入时接受 padding。
- 封装后的字符串最长 96 KiB，解码后至少 28 字节。

WebCrypto 的 AES-GCM 返回的是密文后面附上 16 字节 tag，正好是上面的布局：

```js
const key = await crypto.subtle.importKey("raw", fromB64url(keyB64), "AES-GCM", false, ["encrypt"]);
const nonce = crypto.getRandomValues(new Uint8Array(12));
const ct = new Uint8Array(await crypto.subtle.encrypt({ name: "AES-GCM", iv: nonce }, key,
  new TextEncoder().encode(JSON.stringify(card))));          // ciphertext ‖ 16-byte tag
const character = toB64url(new Uint8Array([...nonce, ...ct]));
```

`fromB64url` 和 `toB64url` 是 base64url 编解码函数，任何实现都行。

把结果作为 `character` 发给卡片路由，并带上 caller secret：

```bash
curl -N -X POST \
  -H "Authorization: Bearer $JWT" \
  -H "X-Byoc-Caller-Secret: $BYOC_CALLER_SECRET" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"content":"hi","client_msg_id":"01J3333333333333333333333A","character":"<sealed card>"}' \
  http://localhost:8080/comp/chat/<session_id>/message/stream
```

每个需要角色的请求都要带上它。key 是卡片唯一的身份：能在实例的 key 下打开的，就是这一次请求里该实例的角色，引擎从不拿一张卡片与更早的卡片做比较。两次请求之间换卡片是下游自己的事；在更早那张卡片下积累的记忆、insight 和亲密度原样延续。

卡片在会话及其实例解析完成、归属确认之后、写入任何行之前检查：

| 情形 | 结果 |
|---|---|
| BYOC 实例，没有 `character` | `400 invalid_payload` |
| genome 实例，带有 `character` | `400 invalid_payload` |
| `character` 不是 base64url、解码后不足 28 字节，或超过 96 KiB | `400 invalid_payload` |
| 明文超过 64 KiB | `400 invalid_payload` |
| 认证 tag 校验失败（key 不对，或字节被改动） | `403 byoc_card_rejected` |
| 明文不是合法的卡片 | `400 invalid_payload`，说明哪条规则未通过 |

## 错误

每个 BYOC 拒绝都使用带 code 的响应体 `{code, message, user_message}`，在任何路由上都一样。任何错误响应都不会引用封装字符串、key 或卡片的任何部分。

| 状态 | Code | 何时 |
|---|---|---|
| 403 | `byoc_forbidden` | BYOC 关闭；caller secret header 缺失或不对（密钥路由，或带有 `character` 的请求）；用引擎没有签发给该用户的 key 和 ticket 调用 create |
| 403 | `byoc_card_rejected` | 卡片无法在实例的 key 下打开 |
| 400 | `invalid_payload` | create：`key` 或 `ticket` 不是 32 字节的 base64url；卡片路由：`character` 缺失、不该出现、格式错误或不是合法卡片（见上表）；async 路由：任何 `character` |
| 400 | `byoc_unsupported` | 上面列出的拒绝 BYOC 实例的路由 |

BYOC 实例上的 stream 轮次如果被一次部署打断，就无法再被驱动：卡片随进程一起消失了，部署也没有可以兜底的角色。它的队列行变为 `failed`，`last_error = 'byoc_unavailable'`，并照常写入 `system_error` 行。请用新的 `client_msg_id` 和卡片重发。

## 记录什么

引擎用来驱动请求而回读的行里，没有任何一行含有卡片的任何部分：`chat_messages`、`chat_turn_queue`（包括 `params` 和 `last_error`）、`persona_instances`、`persona_instance_items` 都没有。卡片只存在于携带它的那次请求的内存里，包括同一进程中回复之后运行的工作。每个实例，引擎只存 32 字节的 key，不存角色的其他任何内容。

- 对话产生的内容与任何一轮一样照常存储，即使其中引用了卡片：回复正文、记忆、insight、PDE 判定、亲密度。
- 审计表和日志对 BYOC 请求的记录与任何其他请求一样。它们是引擎从不回读来驱动生成的备份。
  - prompt log（`PROMPT_LOG_DIR`，不设置则关闭）会写入 BYOC 轮次的完整条目，包括卡片。
  - `engine.chat_images_events` 在 `composed_prompt` 里保留最终的图片 prompt——风格预设、原样的卡片 `appearance` 和 composer 的主体——并在它的 `inputs` 快照里再记一次 `appearance`，stream、compose 和图片编辑三条路由都是如此。
- 审计表从不参与生成。`GET …/image-request` 读取 `chat_images_events`，只是把已存的 prompt 原样交还给调用方。想让角色的 `appearance` 驱动生成的功能，要从请求的卡片里取，绝不能从这张表里取。

## 安全

- init 只依赖 TLS。key 就在它的响应里传递；卡片的加密保护的是 create 之后发送的卡片，不保护 key 的送达。
- caller secret 绝不离开下游的服务器。只有它能让请求带上卡片，所以直接调用引擎的终端用户做不到。
- key 按原样存储。能读数据库的人会读到 key，而卡片只会读到图片审计保留的 `appearance`。
- BYOC 轮次会把完整的回复 prompt（包括卡片）发给模型提供方，与任何角色一样。
- 卡片从哪里来、谁可以带来卡片、什么内容可以接受，都由下游决定。
