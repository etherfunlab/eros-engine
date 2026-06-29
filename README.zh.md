# eros-engine

> **一个让 AI 伴侣如真人般鲜活的开源 Rust 引擎：具备持久记忆、持续演变的关系模型，以及让人设历经数千轮对话仍保持一致的决策引擎。**
>
> `eros-engine` 是 [Eros Chat](https://chat.etherfun.xyz) 背后的伴侣对话核心，现已抽离为独立服务。它将对话转化为持久状态——结构化用户画像、双层长期记忆和六维亲密度模型——让用户每次回来时，角色都像始终如一的同一个人。

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

[English](README.md) · **中文** · [日本語](README.ja.md)

## 为什么做这个

大多数 AI 角色应用只是将记忆作为文本附加到 prompt 中，并用一段指令描述关系。这种做法或许足以演示，但在长会话中，行为会逐渐漂移、偏离人设，而且难以调试。`eros-engine` 将这些要素转化为明确、可检查的状态，使伴侣**如真人一般**——记得你，也会根据当前的关系状态作出反应——并在一轮又一轮对话中**始终符合人设**，因为行为是经过*决策*的，而非临场发挥。

这建立在五大支柱之上：

- 🧠 **双层记忆**——画像记忆（稳定的用户事实）与关系记忆（共同经历、前情呼应、未完话题）均存储在 Postgres + pgvector 中，让伴侣能够跨会话、跨人设记住你。→ [记忆分层](docs/memory-layers.zh.md)
- 💞 **六维亲密度 + ghost 机制**——以数值关系向量（warmth、trust、intimacy、intrigue、patience、tension）结合 EMA 平滑与实时衰减；它会逐渐改变语气、深度和行为，甚至可以决定*不回复*。→ [亲密度模型](docs/affinity-model.zh.md) · [ghost 机制](docs/ghost-mechanics.zh.md)
- 🎭 **人设决策引擎（PDE）**——为每轮对话选择行为（回复、ghost 或发送照片）与内在状态——默认基于规则，也可选择启用 LLM judge。它让回复自然、符合人设，而非流于通用助手的腔调；judge 调用会审计到 `companion_decision_events`。→ [模型配置](docs/model-config.zh.md)
- 🧩 **结构化用户洞察**——以 JSONB 画像记录城市、职业、兴趣、MBTI 信号、情感需求、生活节奏和匹配偏好，并附带加权的 `training_level`；下游产品可查询这些数据，用于匹配、用户引导、分析或 gating。→ [API 参考](docs/api-reference.zh.md)
- ⚡ **专为流畅的伴侣对话打造**——逐 token 的 SSE 流式输出；图像理解（用户可发送照片）和伴侣端图像生成（`reply_image` / `reply_text_image`）；按请求指定 `prompt_traits` 与 tier；基于 OpenRouter 的路由，支持按任务选择模型（固定 / 轮询 / 加权，并配有 fallback chain）和完整的调用审计。→ [API 参考](docs/api-reference.zh.md) · [模型配置](docs/model-config.zh.md)

这不是通用 agent 框架，而是一个专注于同一人设跨多个会话与同一用户持续交流的引擎，适用于 AI 伴侣、日记伴侣、教练 agent、语言导师和角色聊天。

## 架构

```txt
┌─────────────────────────────────────────────────────────┐
│ /comp/* HTTP routes  ←  Supabase JWT middleware          │
│         │                                                │
│         ▼                                                │
│ pipeline orchestrator: load → PDE → handler → chat → post│
│                                          │              │
│  ┌───────────────────────────────────────┴────────┐     │
│  │ post-process, spawned after reply              │     │
│  │   • affinity: persist 6D delta + EMA           │     │
│  │   • memory:   Voyage embed → pgvector upsert   │     │
│  │   • insight:  extract facts → JSONB merge      │     │
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

工作区分为四个 crate：

| Crate | 职责 |
|---|---|
| `eros-engine-core` | 纯领域逻辑：亲密度计算、ghost 决策、PDE、人设类型。无 I/O。 |
| `eros-engine-llm` | OpenRouter 聊天客户端、Voyage embedding 客户端、TOML 模型配置加载器。 |
| `eros-engine-store` | Postgres + pgvector 持久化，所有表均位于 `engine` schema 下。 |
| `eros-engine-server` | Axum HTTP 服务、Supabase JWT 中间件、OpenAPI 文档和 pipeline 连接。 |

你可以将 `eros-engine-server` 作为 HTTP API 运行，也可以把 `core + llm + store` 直接嵌入自己的 Rust 服务。有关 crate 边界、pipeline 阶段和数据流，请参阅[架构](docs/architecture.zh.md)。

## 作为库使用

三个库 crate 已发布到 crates.io（[core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)）：

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "0.6"
eros-engine-store = "0.6"   # only if you want the Postgres + pgvector layer
eros-engine-llm   = "0.6"   # only if you want the OpenRouter + Voyage clients
```

`eros-engine-server` 有意不发布到 crates.io——请使用 Docker 镜像运行（见下文）。

## 作为 Docker 镜像运行

每个 `v*` tag 都会将 `eros-engine-server` 的 `linux/amd64` 镜像发布到 GitHub Container Registry（需要 arm64？请使用 `docker/Dockerfile` 自行构建）：

```bash
docker pull ghcr.io/etherfunlab/eros-engine:0.6.8
# or track the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

最简运行方式（需自行提供 Postgres 和 `.env`）：

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:0.6.8 serve
```

构建此镜像使用的正是 `docker/Dockerfile`，可将其部署到任意容器托管平台。请参阅[部署](docs/deploying.zh.md)。

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

前置：`rust-toolchain.toml` 指定的 Rust 工具链、带 `pgvector` 的 Postgres 16+、一个 OpenRouter API key、一个 Voyage API key，以及一个鉴权来源——Supabase JWKS（`SUPABASE_URL`）或旧版 `SUPABASE_JWT_SECRET`（或你自己的 `AuthValidator`）。

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # fill in DATABASE_URL, OPENROUTER_API_KEY, VOYAGE_API_KEY, and one auth source

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

服务默认监听 `0.0.0.0:8080`。Scalar API 文档位于 `/docs`，OpenAPI JSON 位于 `/api-docs/openapi.json`。官方 Eros Chat Web 客户端并未开源——请自行提供 UI，或将这些 crate 嵌入其他服务。

## API 一览

默认所有 `/comp/*` 路由都需要 `Authorization: Bearer <Supabase JWT>`（`AuthValidator` trait 可替换成其他身份提供方）。核心端点：

- `POST /comp/chat/start`——与指定人设开启聊天会话。
- `POST /comp/chat/{session_id}/message/stream`——**核心**对话端点：逐 token 的 Server-Sent Events。每轮可选字段：`tier`、`prompt_traits`、`audit`、`tips_amount_usd`（给角色打赏）、`image_url`（给角色发一张照片）、`image`（请求角色生成一张图片——风格 / 模型 / 画幅 / 脸部参考）。
- `POST /comp/chat/{session_id}/message/{message_id}/image`——回写角色所生成图片在你存储里的 URL。
- `GET /comp/chat/{session_id}/history` · `GET /comp/chat/{user_id}/sessions` · `GET /comp/user/{user_id}/profile`——历史、会话列表、结构化画像。
- `GET /comp/affinity/{session_id}`——仅调试用的实时亲密度向量（`EXPOSE_AFFINITY_DEBUG=true`）。

有关完整的请求 schema、SSE 帧布局（包括 `delta`、`image`、ghost 和 error 帧）以及各字段语义，请参阅 [API 参考](docs/api-reference.zh.md)。

## 配置

必填环境变量：`DATABASE_URL`、`OPENROUTER_API_KEY`、`VOYAGE_API_KEY`，以及**一个**身份验证来源——`SUPABASE_URL` / `SUPABASE_JWKS_URL`（JWKS，Supabase 在 2025 年之后的默认方式）**或** `SUPABASE_JWT_SECRET`（旧版 HS256）。若未设置身份验证来源，服务将拒绝启动。

其余配置均有合理的默认值：模型路由（`MODEL_CONFIG_PATH` → `model_config.toml`）、OpenRouter 归因标头、dreaming-lite / snapshot sweepers、用于调整关系难度的 `EMA_INERTIA`，以及调试开关。完整注释清单见 [`.env.example`](.env.example)；操作指南见[部署](docs/deploying.zh.md)，模型路由见[模型配置](docs/model-config.zh.md)。

## Roadmap

目前不在引擎里，但在计划中：

- [ ] **Agents playground**——多个 AI 人设在同一会话里互相（以及与用户）互动。
- [ ] **语音消息**——角色发出与用户发出的音频轮次。
- [ ] **实时语音对话**——低延迟的语音来回。
- [ ] **视频生成**——角色主动发送的短视频片段，延续图像执行器。

## 明确不在范围内

本仓库提供对话、记忆和关系状态的核心能力，不包含：

- **匹配**——多阶段过滤、软打分、agent 对 agent 的匹配模拟，仍在闭源产品里。
- **完整社交 UX**——引导、视频、语音、计费、相册、审核 UI、移动端。
- **人设来源 / 市场逻辑**——属于商业产品代码，不是引擎的一部分。

如果你在构建其他类型的产品，可复用的部分是亲密度 + 记忆 + 洞察 pipeline。

## 内容提示

`examples/personas/` 下的示例人设是面向成人的角色聊天示例。当关系状态到位时，它们可以调情、表达欲望，同时仍会拒绝不尊重或越界的行为。如果你的产品需要默认 SFW，请在部署前替换这些人设文件。

还可通过消息路由中的 [`prompt_traits`](docs/prompt-traits.zh.md) 字段进一步调整每次请求的行为——引擎将传入文本视为不透明内容，因此 `prompt_traits` 所编码的策略完全由前端 / 中间件定义。

## 贡献

阅读 [`CONTRIBUTING.md`](CONTRIBUTING.md)。所有贡献者在首次 PR 时须通过 cla-assistant.io 接受 [`CLA`](CLA.md)。

## 许可

`eros-engine` 以 AGPL-3.0-only 授权。如果 AGPL 不适合你的分发模式，可提供商业授权：`henrylin@etherfun.xyz`。
