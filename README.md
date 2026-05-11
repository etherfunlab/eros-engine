# eros-engine

> **An open-source Rust engine for AI companions with memory, relationship state, and structured user insight.**
>
> `eros-engine` is the companion-chat core behind [Eros](https://eros.etherfun.xyz), extracted into a standalone service. It turns conversation into three durable signals: a structured user profile, two-layer long-term memory, and a six-dimensional affinity model that changes how a persona behaves over time.

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Crates.io: core](https://img.shields.io/crates/v/eros-engine-core.svg?label=eros-engine-core)](https://crates.io/crates/eros-engine-core)
[![Crates.io: store](https://img.shields.io/crates/v/eros-engine-store.svg?label=eros-engine-store)](https://crates.io/crates/eros-engine-store)
[![Crates.io: llm](https://img.shields.io/crates/v/eros-engine-llm.svg?label=eros-engine-llm)](https://crates.io/crates/eros-engine-llm)
[![GHCR: eros-engine](https://img.shields.io/badge/ghcr.io-etherfunlab%2Feros--engine-blue)](https://github.com/etherfunlab/eros-engine/pkgs/container/eros-engine)

English · [中文](README.zh.md)

## Why this exists

Most AI character apps treat memory as a prompt append and relationship as a paragraph of instructions. That works for a demo, but it drifts in long sessions and is hard to debug.

`eros-engine` moves those concerns into explicit state:

- **Memory** lives in Postgres + pgvector, split into profile memory and relationship memory.
- **Affinity** is a numeric vector updated with EMA smoothing and real-time decay.
- **User insight** is a structured JSONB profile that downstream products can query.
- **Persona behavior** is planned through a rules-based Persona Decision Engine (PDE), then rendered by an LLM.

The result is not a generic agent framework. It is a focused engine for products where a persona talks to the same user across many sessions: AI companions, journaling companions, coaching agents, language tutors, and character chat.

## Key features

### Two-layer memory

`eros-engine` stores memory in two semantic scopes:

| Layer | Scope | Purpose |
|---|---|---|
| Profile memory | `user_id`, with `instance_id IS NULL` | Stable user facts shared across sessions and personas. |
| Relationship memory | `user_id + persona instance` | Callbacks, shared moments, unresolved threads, and relationship-specific context. |

Embeddings use Voyage `voyage-3-lite` with 512-dimensional vectors. Retrieval runs through pgvector IVFFlat cosine search.

### Six-dimensional affinity

Each chat session has a relationship vector:

| Axis | Range | Controls |
|---|---:|---|
| `warmth` | -1.0 to 1.0 | Tone and address, from distant to affectionate. |
| `trust` | 0.0 to 1.0 | Topic depth and willingness to disclose. |
| `intrigue` | 0.0 to 1.0 | Curiosity and follow-up behavior. |
| `intimacy` | 0.0 to 1.0 | Nicknames, inside jokes, and callbacks. |
| `patience` | 0.0 to 1.0 | Tolerance for low-effort or repeated messages. |
| `tension` | 0.0 to 1.0 | Push-pull, friction, and playful resistance. |

Updates are smoothed with exponential moving average (EMA), so the persona does not jump between emotional states. `intrigue`, `patience`, and `tension` also decay or recover with real time.

Relationship labels such as `stranger`, `slow_burn`, `friend`, `frenemy`, and `romantic` emerge from threshold rules. They are internal state, not user-facing badges.

### Deterministic ghost mechanics

The same affinity vector drives a deterministic ghost decision. When patience and intrigue drop far enough, the persona can choose not to reply.

Four protection rules keep this from feeling arbitrary:

- no ghosting before message 10;
- no two ghosts in a row;
- one-hour cooldown after a ghost;
- a higher threshold after a recent ghost.

This is implemented as domain logic in Rust, not as a prompt suggestion.

### Structured user insight

The `companion_insights` table stores a JSONB profile per user: city, occupation, interests, MBTI signals, relationship values, emotional needs, life rhythm, personality traits, and matching preferences.

Each field contributes to a weighted `training_level`. That makes the profile useful outside the chat loop: matchmaking, onboarding completion, coaching logic, analytics, and product gating can all query structured fields instead of parsing free text.

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

You can run `eros-engine-server` as an HTTP API, or embed `core + llm + store` directly in your own Rust service.

## Use as a library

The three library crates are on crates.io ([core](https://crates.io/crates/eros-engine-core) · [store](https://crates.io/crates/eros-engine-store) · [llm](https://crates.io/crates/eros-engine-llm)):

```bash
cargo add eros-engine-core eros-engine-store eros-engine-llm
```

```toml
[dependencies]
eros-engine-core  = "0.1"
eros-engine-store = "0.1"   # only if you want the Postgres + pgvector layer
eros-engine-llm   = "0.1"   # only if you want the OpenRouter + Voyage clients
```

`eros-engine-server` is intentionally not published to crates.io. See the next section to run it as a Docker image.

## Run as a Docker image

Multi-arch (`linux/amd64`, `linux/arm64`) images for `eros-engine-server` are published to GitHub Container Registry on every `v*` tag:

```bash
docker pull ghcr.io/etherfunlab/eros-engine:0.1.0
# or track the latest tagged release
docker pull ghcr.io/etherfunlab/eros-engine:latest
```

Minimal run (you bring Postgres + your own `.env`):

```bash
docker run --rm -p 8080:8080 --env-file .env \
  ghcr.io/etherfunlab/eros-engine:0.1.0 serve
```

The Dockerfile under `docker/` and `fly.toml` in the repo root are the same artifacts used by this image and by the Fly.io example deployment.

## Documentation

- [Architecture](docs/architecture.md) — crate boundaries, pipeline phases, data flow.
- [Affinity model](docs/affinity-model.md) — six dimensions, EMA, time decay, relationship labels.
- [Ghost mechanics](docs/ghost-mechanics.md) — score formula, protection rules, examples.
- [Memory layers](docs/memory-layers.md) — profile vs relationship memory, Voyage, pgvector retrieval.
- [Model config](docs/model-config.md) — `model_config.toml` schema, task names, resolution rules, 0.x stability commitments.
- [Deploying](docs/deploying.md) — Fly.io, Docker, bring-your-own Postgres / IdP.
- [API reference](docs/api-reference.md) — every `/comp/*` endpoint.

## Quickstart

Prerequisites:

- Rust toolchain from `rust-toolchain.toml`.
- Postgres 16+ with the `pgvector` extension.
- OpenRouter API key.
- Voyage API key.
- Supabase JWT secret, or your own `AuthValidator` implementation.

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env
```

Fill in `DATABASE_URL`, `OPENROUTER_API_KEY`, `VOYAGE_API_KEY`, and `SUPABASE_JWT_SECRET`, then run:

```bash
cargo run -p eros-engine-server -- migrate
cargo run -p eros-engine-server -- seed-personas examples/personas
cargo run -p eros-engine-server -- serve
```

The server listens on `0.0.0.0:8080` by default. Scalar API docs are available at `/docs`, and the OpenAPI JSON is available at `/api-docs/openapi.json`.

The official Eros Chat web client is closed-source. `eros-engine` is designed to run standalone; bring your own UI or embed the crates in another service.

## API surface

All `/comp/*` routes require `Authorization: Bearer <Supabase JWT>` by default.

Highlights:

- `GET  /comp/personas` — list active persona genomes.
- `POST /comp/chat/start` — open a chat session against a persona.
- `POST /comp/chat/{session_id}/message` — synchronous chat turn.
- `POST /comp/chat/{session_id}/message_async` — async chat turn with pending-status polling.
- `GET  /comp/chat/{session_id}/pending/{message_id}` — poll async completion.
- `GET  /comp/chat/{session_id}/history` — paginated chat history.
- `GET  /comp/chat/{user_id}/sessions` — list a user's sessions.
- `GET  /comp/user/{user_id}/profile` — current `companion_insights` and `training_level`.
- `POST /comp/chat/{session_id}/event/gift` — apply an out-of-band gift event and affinity delta.
- `GET  /comp/chat/{session_id}/gifts` — list gift events for a session.
- `GET  /comp/affinity/{session_id}` — debug-only live affinity vector, enabled by `EXPOSE_AFFINITY_DEBUG=true`.

The `AuthValidator` trait is pluggable if you use a different identity provider.

## Configuration

| Env var | Required | Notes |
|---|---|---|
| `DATABASE_URL` | yes | Postgres with `pgvector`; tables are created under `engine.*`. |
| `OPENROUTER_API_KEY` | yes | Chat completions, routed by `examples/model_config.toml` unless overridden. |
| `VOYAGE_API_KEY` | yes | Embeddings. Empty keys fail server boot. |
| `SUPABASE_URL` | no | Supabase project URL. Kept in `.env.example` for client/deploy conventions; the server does not read it today. |
| `SUPABASE_JWT_SECRET` | yes | JWT signing secret for default auth. |
| `BIND_ADDR` | no | Defaults to `0.0.0.0:8080`. |
| `EXPOSE_AFFINITY_DEBUG` | no | Set `true` to enable `/comp/affinity/{session_id}`. |
| `EMA_INERTIA` | no | Defaults to `0.8`. |
| `MODEL_CONFIG_PATH` | no | Defaults to `examples/model_config.toml`. |
| `RUST_LOG` | no | Defaults to `info`. |

## What is deliberately out of scope

This repository is the conversation, memory, and relationship-state core. It does not include:

- **Matchmaking** — multi-stage filtering, soft scoring, and agent-to-agent matching simulation remain in the closed-source product.
- **Full social UX** — onboarding, video, voice, billing, photos, moderation UI, and mobile clients.
- **Persona provenance / marketplace logic** — commercial product code, not part of the engine.

If you are building a different product, the reusable part is the affinity + memory + insight pipeline.

## Content note

The example personas under `examples/personas/` are written as adult character-chat examples. They can flirt and express desire when the relationship state reaches that point, while still refusing disrespectful or boundary-crossing behavior. If your product needs a SFW default, replace those persona files before deploying.

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md). All contributors must accept the [`CLA`](CLA.md) through cla-assistant.io on their first PR.

## License

`eros-engine` is licensed under AGPL-3.0-only. If AGPL does not fit your distribution model, commercial licensing is available: `henrylin@etherfun.xyz`.
