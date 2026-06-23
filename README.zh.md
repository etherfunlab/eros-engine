# eros-engine

> **一个开源的 Rust AI 伴侣引擎：让角色"像真人"——持久记忆、会演变的关系模型，以及一个让人设在成千上万轮对话里保持稳定的决策引擎。**
>
> `eros-engine` 是 [Eros Chat](https://chat.etherfun.xyz) 背后的伴侣对话核心，抽取成了独立服务。它把对话沉淀为可持久的状态——结构化用户画像、双层长期记忆、六维亲密度模型——让角色在用户每次回来时都表现得像同一个人。

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

[English](README.md) · 中文

## 为什么做这个

大多数 AI 角色应用把记忆当成往 prompt 里追加文本，把关系写成一段指令。这能撑起一个 demo，但在长会话里会漂移、会崩人设，也很难调试。`eros-engine` 把这些都搬进显式、可检视的状态里——所以角色会"像真人"（记得你、并对关系所处的位置作出反应），并且"人设稳定"（行为是被*决策*出来的，而不是即兴发挥）。

五大支柱支撑这一点：

- **双层记忆**——画像记忆（稳定的用户事实）与关系记忆（共同时刻、回扣、未了的话头），都存在 Postgres + pgvector 里，让角色跨会话、跨人设地记得你。→ [记忆分层](docs/memory-layers.zh.md)
- **六维亲密度 + ghost 机制**——一个数值化的关系向量（warmth、trust、intimacy、intrigue、patience、tension），用 EMA 平滑、随真实时间衰减；它会随时间重塑语气、深度与行为，甚至可以决定*不回复*。→ [亲密度模型](docs/affinity-model.zh.md) · [ghost 机制](docs/ghost-mechanics.zh.md)
- **人设决策引擎（PDE）**——为每一轮挑选行为（回复 / ghost / 发一张照片）和内在状态——默认基于规则，可选开启 LLM 裁判层。这正是让回复"像真人"且不崩人设、而不是变成通用助手腔的关键；裁判调用会审计到 `companion_decision_events`。→ [模型配置](docs/model-config.zh.md)
- **结构化用户画像**——每个用户一份 JSONB 画像（城市、职业、兴趣、MBTI 信号、情感需求、生活节奏、匹配偏好），带一个加权的 `training_level`，下游产品可直接查询用于匹配、引导、分析或灰度。→ [API 参考](docs/api-reference.zh.md)
- **为流畅的伴侣对话而生**——逐 token 的 SSE 流式输出；图像理解（用户可以发照片）与角色主动生成图片（`reply_image` / `reply_text_image`）；按请求注入的 prompt traits 与 tier；OpenRouter 路由，按任务选模型（固定 / 轮询 / 加权，外加 fallback 链）并全程审计调用。→ [API 参考](docs/api-reference.zh.md) · [模型配置](docs/model-config.zh.md)

这不是一个通用 agent 框架。它是一个聚焦的引擎，专为"同一个角色跨多次会话与同一个用户对话"的产品而做：AI 伴侣、日记陪伴、教练 agent、语言陪练、角色聊天。

## 架构

```txt
┌─────────────────────────────────────────────────────────┐
│ /comp/* HTTP 路由  ←  Supabase JWT 中间件                │
│         │                                                │
│         ▼                                                │
│ pipeline 编排：load → PDE → handler → chat → post        │
│                                          │              │
│  ┌───────────────────────────────────────┴────────┐     │
│  │ post-process，回复后异步 spawn                  │     │
│  │   • 亲密度：持久化 6 维 delta + EMA            │     │
│  │   • 记忆：  Voyage embed → pgvector upsert     │     │
│  │   • 画像：  抽取事实 → JSONB 合并              │     │
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

工作区拆成四个 crate：

| Crate | 职责 |
|---|---|
| `eros-engine-core` | 纯领域逻辑：亲密度计算、ghost 决策、PDE、人设类型。零 I/O。 |
| `eros-engine-llm` | OpenRouter 聊天客户端、Voyage 向量客户端、TOML 模型配置加载。 |
| `eros-engine-store` | Postgres + pgvector 持久层，所有表都在 `engine` schema 下。 |
| `eros-engine-server` | Axum HTTP 服务、Supabase JWT 中间件、OpenAPI 文档、pipeline 接线。 |

你可以把 `eros-engine-server` 当 HTTP API 跑，也可以把 `core + llm + store` 直接嵌进你自己的 Rust 服务。crate 边界、pipeline 阶段、数据流见 [架构](docs/architecture.zh.md)。

## 作为库使用

三个库 crate 已发布到 crates.io（[core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)）：

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "0.6"
eros-engine-store = "0.6"   # 需要 Postgres + pgvector 层时
eros-engine-llm   = "0.6"   # 需要 OpenRouter + Voyage 客户端时
```

`eros-engine-server` 刻意不发布到 crates.io——它以 Docker 镜像方式运行（见下）。

## 作为 Docker 镜像运行

每个 `v*` tag 都会把 `eros-engine-server` 的 `linux/amd64` 镜像发布到 GitHub Container Registry（需要 arm64？用 `docker/Dockerfile` 自己构建）：

```bash
docker pull ghcr.io/etherfunlab/eros-engine:0.6.2
# 或跟踪最新 release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

最小运行（自带 Postgres 与 `.env`）：

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:0.6.2 serve
```

`docker/Dockerfile` 就是构建该镜像的同一份产物，可部署到任意容器平台。见 [部署](docs/deploying.zh.md)。

## 文档

- [架构](docs/architecture.zh.md)——crate 边界、pipeline 阶段、数据流。
- [亲密度模型](docs/affinity-model.zh.md)——六个维度、EMA、时间衰减、关系标签。
- [ghost 机制](docs/ghost-mechanics.zh.md)——评分公式、保护规则、示例。
- [记忆分层](docs/memory-layers.zh.md)——画像记忆 vs 关系记忆、Voyage、pgvector 检索。
- [模型配置](docs/model-config.zh.md)——`model_config.toml` schema、各任务（chat、vision、图像生成、PDE、过滤器、抽取）、选模型规则、0.x 稳定性承诺。
- [Prompt traits](docs/prompt-traits.zh.md)——按请求注入系统 prompt 与 tier 白名单。
- [LLM / OpenRouter 审计](docs/llm-audit.zh.md)——按用户 / 按会话的归因透传。
- [部署](docs/deploying.zh.md)——Docker、自带 Postgres / IdP、运行期环境变量。
- [API 参考](docs/api-reference.zh.md)——每个 `/comp/*` 端点、请求字段、SSE 帧布局。

## 快速开始

前置：`rust-toolchain.toml` 指定的 Rust 工具链、带 `pgvector` 的 Postgres 16+、一个 OpenRouter API key、一个 Voyage API key，以及一个 Supabase JWT secret（或你自己的 `AuthValidator`）。

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # 填入 DATABASE_URL、OPENROUTER_API_KEY、VOYAGE_API_KEY，以及一个鉴权来源

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

服务默认监听 `0.0.0.0:8080`。Scalar API 文档在 `/docs`，OpenAPI JSON 在 `/api-docs/openapi.json`。官方 Eros Chat 网页客户端是闭源的——自带 UI，或把这些 crate 嵌进别的服务。

## API 一览

默认所有 `/comp/*` 路由都需要 `Authorization: Bearer <Supabase JWT>`（`AuthValidator` trait 可替换成其他身份提供方）。核心端点：

- `POST /comp/chat/start`——对某个人设开一个会话。
- `POST /comp/chat/{session_id}/message/stream`——**核心**对话端点：逐 token 的 Server-Sent Events。每轮可选字段：`tier`、`prompt_traits`、`audit`、`tips_amount_usd`（给角色打赏）、`image_url`（给角色发一张照片）、`image`（请求角色生成一张图片——风格 / 模型 / 画幅 / 脸部参考）。
- `POST /comp/chat/{session_id}/message/{message_id}/image`——回写角色所生成图片在你存储里的 URL。
- `GET /comp/chat/{session_id}/history` · `GET /comp/chat/{user_id}/sessions` · `GET /comp/user/{user_id}/profile`——历史、会话列表、结构化画像。
- `GET /comp/affinity/{session_id}`——仅调试用的实时亲密度向量（`EXPOSE_AFFINITY_DEBUG=true`）。

阻塞式同步 `/message` 端点已在 0.3 移除——SSE 是唯一的对话路径。完整请求 schema、SSE 帧布局（含 `delta`、`image`、ghost、error 帧）与逐字段语义见 [API 参考](docs/api-reference.zh.md)。

## 配置

必填环境变量：`DATABASE_URL`、`OPENROUTER_API_KEY`、`VOYAGE_API_KEY`，以及**一个**鉴权来源——`SUPABASE_URL` / `SUPABASE_JWKS_URL`（JWKS，2025 年后 Supabase 默认）**或** `SUPABASE_JWT_SECRET`（旧版 HS256）。一个都不设，服务会 fail-closed 拒绝启动。

其余都有合理默认值：模型路由（`MODEL_CONFIG_PATH` → `model_config.toml`）、OpenRouter 归因头、dreaming-lite / snapshot 后台任务、`EMA_INERTIA`（关系难度旋钮）、调试开关等。完整带注释的清单见 [`.env.example`](.env.example)；运行期指引见 [部署](docs/deploying.zh.md)，模型路由见 [模型配置](docs/model-config.zh.md)。

## Roadmap

目前不在引擎里，但在计划中：

- **Agents playground**——多个 AI 人设在同一会话里互相（以及与用户）互动。
- **语音消息**——角色发出与用户发出的音频轮次。
- **实时语音对话**——低延迟的语音来回。
- **视频生成**——角色主动发送的短视频片段，延续图像执行器。

## 明确不在范围内

本仓库是对话、记忆、关系状态的核心，不包含：

- **匹配**——多阶段过滤、软打分、agent 对 agent 的匹配模拟，仍在闭源产品里。
- **完整社交 UX**——引导、视频、语音、计费、相册、审核 UI、移动端。
- **人设来源 / 市场逻辑**——属于商业产品代码，不是引擎的一部分。

如果你在做不同的产品，可复用的部分是亲密度 + 记忆 + 画像这条 pipeline。

## 内容提示

`examples/personas/` 下的示例人设是面向成人的角色聊天示例。当关系状态到位时，它们可以调情、表达欲望，同时仍会拒绝不尊重或越界的行为。如果你的产品需要默认 SFW，请在部署前替换这些人设文件。

每轮行为还可以通过消息路由上的 [`prompt_traits`](docs/prompt-traits.zh.md) 字段进一步调节——引擎把传入文本当作不透明内容，所以"这些 trait 编码了什么策略"完全由你的前端 / 中间层决定。

## 贡献

阅读 [`CONTRIBUTING.md`](CONTRIBUTING.md)。所有贡献者在首次 PR 时须通过 cla-assistant.io 接受 [`CLA`](CLA.md)。

## 许可

`eros-engine` 以 AGPL-3.0-only 授权。如果 AGPL 不适合你的分发模式，可提供商业授权：`henrylin@etherfun.xyz`。
