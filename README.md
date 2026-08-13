# eros-engine

**An open-source Rust engine for AI companions that feel real: persistent memory, an evolving relationship model, and a decision engine that keeps a persona in character across thousands of turns.**

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

**English** · [中文](README.zh.md) · [日本語](README.ja.md)

## Highlights

Most AI character apps eventually forget who you are. Their relationships reset to whatever fits in a prompt, and their personalities drift as the conversation grows. `eros-engine` makes those parts durable: a companion remembers you across sessions, the relationship changes through interaction, and each reply is chosen in character rather than improvised by a generic assistant.

The engine has six foundations:

- 🧠 **Two-layer memory** — stable facts about the user live alongside shared moments, callbacks, and unfinished threads. → [Memory layers](docs/memory-layers.md)
- 💞 **Evolving affinity** — six relationship dimensions change smoothly and decay with time, shaping tone, depth, and even whether the companion replies. → [Affinity model](docs/affinity-model.md) · [Ghost mechanics](docs/ghost-mechanics.md)
- 🎭 **Persona Decision Engine (PDE)** — before generation, the engine chooses an action and inner state. Rules work out of the box; an LLM judge is optional. → [Model config](docs/model-config.md)
- 🧩 **Structured user insight** — the engine builds a queryable profile that downstream products can use for onboarding, personalization, and analysis. → [API reference](docs/api-reference.md)
- ⚡ **A complete chat path** — SSE streaming, image understanding and generation requests, prompt traits, per-task model selection, fallbacks, and call auditing. OpenRouter is the default; additional OpenAI-compatible chat and embedding providers can be configured through `[providers]`. → [API reference](docs/api-reference.md) · [Model config](docs/model-config.md)
- 🎙️ **A voice turn path built for interruption** — a lean, low-latency turn endpoint on its own channel, with barge-in: the client stops playback and reports what was actually spoken, so history records what the user *heard* rather than what the model produced. A turn lost to a dropped connection can be regenerated instead of stranded. → [API reference](docs/api-reference.md#post-compvoicesession_idturnstream)

This is not a generic agent framework. It is the stateful core for products where one persona gets to know the same person over time: companions, journals, coaches, tutors, and character chat.

## Architecture

```txt
┌─────────────────────────────────────────────────────────┐
│ /comp/* HTTP routes  ←  Supabase JWT middleware          │
│         │                                                │
│         ▼                                                │
│ pipeline orchestrator: load → PDE → handler → chat → post│
│                                          │              │
│  ┌───────────────────────────────────────┴────────┐     │
│  │ post-process, spawned after reply              │     │
│  │   • affinity: persist 6D graded turn           │     │
│  │   • memory:   Voyage embed → pgvector upsert   │     │
│  │   • insight:  extract facts → JSONB merge      │     │
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

Four crates keep domain logic, model access, persistence, and the HTTP service separate. Run `eros-engine-server` as an API, or embed `core + llm + store` in your own Rust service. See [Architecture](docs/architecture.md) for the boundaries and data flow.

## Library use

The three library crates are on crates.io ([core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)):

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "1.0"
eros-engine-store = "1.0"   # optional: Postgres + pgvector persistence
eros-engine-llm   = "1.0"   # optional: model and embedding clients
```

`eros-engine-server` is not published to crates.io; run it as a Docker image instead.

## Docker image

Multi-architecture images are published to GitHub Container Registry for every `v*` tag:

```bash
docker pull ghcr.io/etherfunlab/eros-engine:1.1.0
# Or follow the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:1.1.0 serve
```

Bring your own Postgres and `.env`; the same `docker/Dockerfile` can be deployed to any container host. See [Deploying](docs/deploying.md).

## Documentation

- [Architecture](docs/architecture.md) — crate boundaries, pipeline phases, and data flow.
- [Affinity model](docs/affinity-model.md) — relationship dimensions, graded scoring, decay, and labels.
- [Ghost mechanics](docs/ghost-mechanics.md) — when and why a companion may stay silent.
- [Memory layers](docs/memory-layers.md) — profile and relationship memory, embeddings, and retrieval.
- [World system](docs/world-system.md) — experimental World Memories, World Town, and World Stories simulations.
- [Model config](docs/model-config.md) — tasks, selection, fallbacks, and multi-provider routing through `[providers]`.
- [Prompt traits](docs/prompt-traits.md) — per-request prompt behavior and tier allow-lists.
- [LLM / OpenRouter audit](docs/llm-audit.md) — user and session attribution.
- [Deploying](docs/deploying.md) — Docker, Postgres, identity, and operations.
- [API reference](docs/api-reference.md) — routes, request schemas, and SSE frames.

## Quickstart

You need Rust, Postgres 16+ with `pgvector`, an OpenRouter API key, and one auth source. Voyage powers the default embedding route; embeddings can be routed to other providers instead, and once both embedding reads and writes leave Voyage, `VOYAGE_API_KEY` is no longer needed.

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # Set DATABASE_URL, OPENROUTER_API_KEY, VOYAGE_API_KEY, and one auth source

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

The server listens on `0.0.0.0:8080` by default, with Scalar API docs at `/docs`. The official Eros Chat web client is closed-source, so bring your own UI or embed the crates in another service.

## API surface

The main flow is simple: start a persona session, then send turns to the SSE streaming endpoint. Voice runs on its own channel — start the session with `channel: "voice"` and use the voice turn endpoint, which carries a leaner prompt and a barge-in call; the two channels never write into each other's sessions. The engine also exposes history, session, profile, and optional affinity-debug routes. Authentication uses Supabase JWTs by default and can be replaced through `AuthValidator`. See the [API reference](docs/api-reference.md) for paths, payloads, and stream frames.

## Configuration

At minimum, configure `DATABASE_URL`, one authentication source, and `OPENROUTER_API_KEY` — OpenRouter is the built-in default and its key is always required at boot. `[providers]` adds OpenAI-compatible endpoints for chat and embeddings, each with its own key; the default Voyage embedding setup requires `VOYAGE_API_KEY`, unless `[tasks.embedding]` routes both reads and writes elsewhere.

The complete environment list is in [`.env.example`](.env.example), operational guidance in [Deploying](docs/deploying.md), and routing details in [Model config](docs/model-config.md).

## Roadmap

- [ ] **Agents playground** — multiple personas sharing a session with each other and the user.
- [ ] **Native audio I/O** — the low-latency voice-turn API and barge-in already ship; STT/TTS currently stay on the caller's side.
- [ ] **Video generation** for companion-sent clips.

## Non-goals

This repository is the conversation, memory, and relationship-state core. Matchmaking, the full social product experience, and persona marketplace or provenance logic remain outside the engine. The reusable center is the affinity, memory, and insight pipeline.

## Content note

The personas in `examples/personas/` are adult character-chat examples. They may flirt or express desire as a relationship develops, while refusing disrespectful or boundary-crossing behavior. Replace them before deployment if your product needs a SFW default.

Per-request behavior can also be adjusted through [`prompt_traits`](docs/prompt-traits.md). The engine treats this text as opaque; your frontend or middleware owns its policy.

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md). Contributors accept the [`CLA`](CLA.md) through cla-assistant.io on their first PR.

## License

`eros-engine` is licensed under AGPL-3.0-only. For commercial licensing, contact `henrylin@etherfun.xyz`.
