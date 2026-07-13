# eros-engine

> **An open-source Rust engine for AI companions that feel real: persistent memory, an evolving relationship model, and a decision engine that keeps a persona in character across thousands of turns.**
>
> `eros-engine` is the companion-chat core behind [Eros Chat](https://chat.etherfun.xyz), extracted into a standalone service. It turns conversation into durable state — a structured user profile, two-layer long-term memory, and a six-dimensional affinity model — so a persona behaves like the same person each time a user comes back.

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

**English** · [中文](README.zh.md) · [日本語](README.ja.md)

## Why this exists

Most AI character apps treat memory as text appended to a prompt and relationship as a paragraph of instructions. That can work in a demo, but behavior drifts over long sessions, breaks character, and becomes hard to debug. `eros-engine` moves those concerns into explicit, inspectable state, so a companion feels like **a real person** — remembering you and responding to the state of the relationship — and **stays in character** turn after turn, because behavior is *decided*, not improvised.

Five pillars support this:

- 🧠 **Two-layer memory** — profile memory (stable user facts) and relationship memory (shared moments, callbacks, open threads) in Postgres + pgvector, so the companion remembers you across sessions and personas. → [Memory layers](docs/memory-layers.md)
- 💞 **Six-axis affinity + ghost mechanics** — a numeric relationship vector (warmth, trust, intimacy, intrigue, patience, tension) updated with EMA smoothing and real-time decay; it reshapes tone, depth, and behavior over time, and can even decide *not* to reply. → [Affinity model](docs/affinity-model.md) · [Ghost mechanics](docs/ghost-mechanics.md)
- 🎭 **Persona Decision Engine (PDE)** — selects each turn's action (reply, ghost, or send a photo) and inner state — rules-based by default, with an opt-in LLM judge. This keeps replies human and in character instead of sounding like a generic assistant; judge calls are audited to `companion_decision_events`. → [Model config](docs/model-config.md)
- 🧩 **Structured user insight** — a JSONB profile (city, occupation, interests, MBTI signals, emotional needs, life rhythm, matching preferences) with a weighted `training_level`, queryable by downstream products for matchmaking, onboarding, analytics, or gating. → [API reference](docs/api-reference.md)
- ⚡ **Built for fluent companion chat** — token-by-token SSE streaming; image understanding (the user can send a photo) and companion-sent image generation (`reply_image` / `reply_text_image`); per-request prompt traits and tiers; OpenRouter-backed routing with per-task model selection (fixed / round-robin / weighted, plus a fallback chain) and full call auditing. → [API reference](docs/api-reference.md) · [Model config](docs/model-config.md)

This is not a generic agent framework. It is a focused engine for products where one persona talks to the same user across many sessions: AI companions, journaling companions, coaching agents, language tutors, and character chat.

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
│  │   • affinity: persist 6D delta + EMA           │     │
│  │   • memory:   Voyage embed → pgvector upsert   │     │
│  │   • insight:  extract facts → JSONB merge      │     │
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

The workspace is split into four crates:

| Crate | Role |
|---|---|
| `eros-engine-core` | Pure domain logic: affinity math, ghost decision, PDE, persona types. Zero I/O. |
| `eros-engine-llm` | OpenRouter chat client, Voyage embedding client, TOML model-config loader. |
| `eros-engine-store` | Postgres + pgvector persistence, with all tables under the `engine` schema. |
| `eros-engine-server` | Axum HTTP service, Supabase JWT middleware, OpenAPI docs, and pipeline wiring. |

You can run `eros-engine-server` as an HTTP API, or embed `core + llm + store` directly in your own Rust service. See [Architecture](docs/architecture.md) for crate boundaries, pipeline phases, and data flow.

## Use as a library

The three library crates are on crates.io ([core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)):

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "0.7"
eros-engine-store = "0.7"   # only if you want the Postgres + pgvector layer
eros-engine-llm   = "0.7"   # only if you want the OpenRouter + Voyage clients
```

`eros-engine-server` is intentionally not published to crates.io — run it as a Docker image (below).

## Run as a Docker image

`linux/amd64` images for `eros-engine-server` are published to GitHub Container Registry for every `v*` tag (need arm64? Build it yourself from `docker/Dockerfile`):

```bash
docker pull ghcr.io/etherfunlab/eros-engine:0.7.3
# or track the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

Minimal run (you bring Postgres + your own `.env`):

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:0.7.3 serve
```

The `docker/Dockerfile` is the same artifact used to build this image. Deploy it on any container host. See [Deploying](docs/deploying.md).

## Documentation

- [Architecture](docs/architecture.md) — crate boundaries, pipeline phases, data flow.
- [Affinity model](docs/affinity-model.md) — six dimensions, EMA, time decay, relationship labels.
- [Ghost mechanics](docs/ghost-mechanics.md) — score formula, protection rules, examples.
- [Memory layers](docs/memory-layers.md) — profile vs relationship memory, Voyage, pgvector retrieval.
- [Model config](docs/model-config.md) — `model_config.toml` schema, every task (chat, vision, image generation, PDE, filters, extraction), model selection, 0.x stability commitments.
- [Prompt traits](docs/prompt-traits.md) — per-request system-prompt injection and tier allow-lists.
- [LLM / OpenRouter audit](docs/llm-audit.md) — per-user / per-session attribution passthrough.
- [Deploying](docs/deploying.md) — Docker, bring-your-own Postgres / IdP, operational env vars.
- [API reference](docs/api-reference.md) — every `/comp/*` endpoint, request fields, and SSE frame layout.

## Quickstart

Prerequisites: a Rust toolchain (`rust-toolchain.toml`), Postgres 16+ with `pgvector`, an OpenRouter API key, a Voyage API key, and one auth source — Supabase JWKS (`SUPABASE_URL`) or a legacy `SUPABASE_JWT_SECRET` (or your own `AuthValidator`).

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env   # fill in DATABASE_URL, OPENROUTER_API_KEY, VOYAGE_API_KEY, and one auth source

cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

The server listens on `0.0.0.0:8080` by default. Scalar API docs are available at `/docs`; the OpenAPI JSON is at `/api-docs/openapi.json`. The official Eros Chat web client is closed-source — bring your own UI or embed the crates in another service.

## API surface

All `/comp/*` routes require `Authorization: Bearer <Supabase JWT>` by default (the `AuthValidator` trait is pluggable for other identity providers). Key endpoints:

- `POST /comp/chat/start` — open a chat session against a persona.
- `POST /comp/chat/{session_id}/message/stream` — **the** chat turn endpoint: token-by-token Server-Sent Events. Optional per-turn fields include `tier`, `prompt_traits`, `audit`, `tips_amount_usd` (tip the companion), `image_url` (send the companion a photo), and `image` (request a companion-generated image — style / aspect ratio). For an image turn the engine composes the prompt and emits an `image_request` frame; it does not draw on the chat stream.
- `POST /comp/chat/{session_id}/image/stream` — opt-in: on receiving an `image_request`, have the engine draw the composed prompt and stream the image back (`image_pending` → `image_attempt*` → `image` / `image_failed`). Requires `[tasks.chat_image_generation]`; absent, it returns `501` and the consumer draws the prompt itself.
- `GET /comp/chat/{session_id}/history` · `GET /comp/chat/{user_id}/sessions` · `GET /comp/user/{user_id}/profile` — history, session list, and the structured insight profile.
- `GET /comp/affinity/{session_id}` — debug-only live affinity vector (`EXPOSE_AFFINITY_DEBUG=true`).

For the full request schema, SSE frame layout (including `delta`, `image_request`, ghost, and error frames on the chat stream, and the draw endpoint's `image` frames), and per-field semantics, see the [API reference](docs/api-reference.md).

## Configuration

Required env vars: `DATABASE_URL`, `OPENROUTER_API_KEY`, `VOYAGE_API_KEY`, and **one** auth source — `SUPABASE_URL` / `SUPABASE_JWKS_URL` (JWKS, the post-2025 Supabase default) **or** `SUPABASE_JWT_SECRET` (legacy HS256). The server refuses to boot if no auth source is set.

Everything else has sane defaults: model routing (`MODEL_CONFIG_PATH` → `model_config.toml`), OpenRouter attribution headers, the dreaming-lite / snapshot sweepers, the `EMA_INERTIA` relationship-difficulty dial, and debug toggles. The full annotated list lives in [`.env.example`](.env.example); operational guidance is in [Deploying](docs/deploying.md), and model routing in [Model config](docs/model-config.md).

## Roadmap

Not part of the engine today, but on the radar:

- [ ] **Agents playground** — multiple AI personas interacting with each other (and the user) in one session.
- [ ] **Voice messages** — companion-sent and user-sent audio turns.
- [ ] **Real-time voice conversation** — low-latency spoken back-and-forth.
- [ ] **Video generation** — short companion-sent video clips, extending the image executor.

## What is deliberately out of scope

This repository is the conversation, memory, and relationship-state core. It does not include:

- **Matchmaking** — multi-stage filtering, soft scoring, and agent-to-agent matching simulation remain in the closed-source product.
- **Full social UX** — onboarding, video, voice, billing, photos, moderation UI, and mobile clients.
- **Persona provenance / marketplace logic** — commercial product code, not part of the engine.

If you are building a different product, the reusable part is the affinity + memory + insight pipeline.

## Content note

The example personas under `examples/personas/` are written as adult character-chat examples. They can flirt and express desire when the relationship state reaches that point, while still refusing disrespectful or boundary-crossing behavior. If your product needs a SFW default, replace those persona files before deploying.

Per-request behavior can be further adjusted via the [`prompt_traits`](docs/prompt-traits.md) field on the message routes — the engine treats the supplied text as opaque, so the policy defining what those traits encode lives entirely in your frontend / middleware.

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md). All contributors must accept the [`CLA`](CLA.md) through cla-assistant.io on their first PR.

## License

`eros-engine` is licensed under AGPL-3.0-only. If AGPL does not fit your distribution model, commercial licensing is available: `henrylin@etherfun.xyz`.
