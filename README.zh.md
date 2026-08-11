# eros-engine

**一个让 AI 伴侣如真人般鲜活的开源 Rust 引擎：具备持久记忆、持续演变的关系模型，以及让人设历经数千轮对话仍保持一致的决策引擎。**

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

[English](README.md) · **中文** · [日本語](README.ja.md)

## 亮点

大多数 AI 角色应用用不了多久就会忘记你。关系被压缩成 prompt 里的一段文字，聊得越久，人设越容易漂移。`eros-engine` 把这些真正重要的部分变成持久状态：伴侣能跨会话记住你，关系会随着相处而变化，每次回复也会依照人设作出决定，而不是让一个通用助手临场发挥。

引擎建立在五项基础能力之上：

- 🧠 **双层记忆**——稳定的用户事实与共同经历、前情呼应、未完话题各有其位。→ [记忆分层](docs/memory-layers.zh.md)
- 💞 **演变的亲密度**——六个关系维度平滑变化，也会随时间衰减，逐渐影响语气、深度，甚至是否回复。→ [亲密度模型](docs/affinity-model.zh.md) · [ghost 机制](docs/ghost-mechanics.zh.md)
- 🎭 **人设决策引擎（PDE）**——生成回复前，先选择行为与内在状态。默认规则即可运行，也可启用 LLM 评判。→ [模型配置](docs/model-config.zh.md)
- 🧩 **结构化用户洞察**——持续形成可查询的用户画像，供下游产品用于引导、个性化和分析。→ [API 参考](docs/api-reference.zh.md)
- ⚡ **完整的聊天链路**——SSE 流式输出、图像理解与生成请求、prompt traits、按任务选模型、故障回退及调用审计。OpenRouter 是默认提供方，也可通过 `[providers]` 接入其他兼容 OpenAI 的聊天和 embedding 服务。→ [API 参考](docs/api-reference.zh.md) · [模型配置](docs/model-config.zh.md)

它不是通用 agent 框架，而是为同一人设与同一用户长期相处而做的有状态核心，适合 AI 伴侣、日记伙伴、教练、语言导师和角色聊天。

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

四个 crate 分别承载领域逻辑、模型访问、持久化和 HTTP 服务。你可以将 `eros-engine-server` 作为 API 运行，也可以把 `core + llm + store` 嵌入自己的 Rust 服务。边界与数据流详见[架构](docs/architecture.zh.md)。

## 库集成

三个库 crate 均已发布到 crates.io（[core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)）：

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "1.0"
eros-engine-store = "1.0"   # optional: Postgres + pgvector persistence
eros-engine-llm   = "1.0"   # optional: model and embedding clients
```

`eros-engine-server` 不发布到 crates.io，请使用 Docker 镜像运行。

## Docker 镜像

每个 `v*` tag 都会向 GitHub Container Registry 发布多架构镜像：

```bash
docker pull ghcr.io/etherfunlab/eros-engine:1.0.4
# Or follow the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:1.0.4 serve
```

你需要自行提供 Postgres 和 `.env`；同一个 `docker/Dockerfile` 可部署到任意容器托管平台。详见[部署](docs/deploying.zh.md)。

## 文档

- [架构](docs/architecture.zh.md)——crate 边界、pipeline 阶段和数据流。
- [亲密度模型](docs/affinity-model.zh.md)——关系维度、平滑、衰减和标签。
- [ghost 机制](docs/ghost-mechanics.zh.md)——伴侣何时以及为何保持沉默。
- [记忆分层](docs/memory-layers.zh.md)——画像与关系记忆、embedding 和检索。
- [世界系统](docs/world-system.zh.md)——实验性的 World Memories、World Town 与 World Stories 模拟。
- [模型配置](docs/model-config.zh.md)——任务、模型选择、故障回退及通过 `[providers]` 实现的多提供方路由。
- [Prompt traits](docs/prompt-traits.zh.md)——按请求调整 prompt 行为及 tier 白名单。
- [LLM / OpenRouter 审计](docs/llm-audit.zh.md)——用户与会话归因。
- [部署](docs/deploying.zh.md)——Docker、Postgres、身份认证和运维。
- [API 参考](docs/api-reference.zh.md)——路由、请求结构和 SSE 帧。

## 快速开始

你需要 Rust、带 `pgvector` 的 Postgres 16+、一个 OpenRouter API key，以及一个鉴权来源。默认 embedding 路由使用 Voyage；embedding 也可路由到其他提供方，只有在读取和写入均不再使用 Voyage 时，才无需 `VOYAGE_API_KEY`。

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # Set DATABASE_URL, OPENROUTER_API_KEY, VOYAGE_API_KEY, and one auth source

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

服务默认监听 `0.0.0.0:8080`，Scalar API 文档位于 `/docs`。官方 Eros Chat Web 客户端并未开源，请自行提供 UI，或将这些 crate 嵌入其他服务。

## API 一览

核心流程很简单：创建人设会话，然后向 SSE 流式端点发送对话。引擎还提供历史记录、会话、画像及可选的亲密度调试路由。默认使用 Supabase JWT 鉴权，也可通过 `AuthValidator` 替换。具体路径、payload 与流式帧详见 [API 参考](docs/api-reference.zh.md)。

## 配置

至少需要设置 `DATABASE_URL`、一个身份验证来源，以及 `OPENROUTER_API_KEY`——OpenRouter 是内置默认提供方，启动时始终需要它的 key。`[providers]` 可加入兼容 OpenAI 的聊天和 embedding 端点，各用各的 key；默认 Voyage embedding 配置需要 `VOYAGE_API_KEY`，除非 `[tasks.embedding]` 将读取和写入都路由到其他提供方。

完整环境变量见 [`.env.example`](.env.example)，运维说明见[部署](docs/deploying.zh.md)，路由细节见[模型配置](docs/model-config.zh.md)。

## 路线图

- [ ] **多角色实验场**——多个 AI 人设在同一会话中彼此互动，也与用户互动。
- [ ] **语音消息**与**原生音频 I/O**——低延迟的语音回合 API 已经就位，STT/TTS 目前由调用方负责。
- [ ] **视频生成**——由伴侣发送短视频片段。

## 非目标

本仓库只提供对话、记忆和关系状态的核心。匹配、完整的社交产品体验，以及人设市场或来源逻辑不属于本引擎。可复用的中心是亲密度、记忆与洞察 pipeline。

## 内容提示

`examples/personas/` 下是面向成人的角色聊天示例。关系发展到相应阶段时，它们可能调情或表达欲望，同时会拒绝不尊重或越界的行为。如果产品需要默认 SFW，请在部署前替换这些人设。

也可通过 [`prompt_traits`](docs/prompt-traits.zh.md) 调整每次请求的行为。引擎将其文本视为不透明内容，具体策略由你的前端或中间件负责。

## 贡献

请阅读 [`CONTRIBUTING.md`](CONTRIBUTING.md)。贡献者须在首次 PR 时通过 cla-assistant.io 接受 [`CLA`](CLA.md)。

## 许可

`eros-engine` 以 AGPL-3.0-only 授权。如需商业授权，请联系 `henrylin@etherfun.xyz`。
