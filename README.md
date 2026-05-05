# eros-engine

> **An AI companion that learns who you are — and feels like a real person doing it.**
>
> The same intimacy + memory pipeline that powers [Eros](https://eros.etherfun.xyz)'s dating product, open-sourced. Talk to a persona; the engine quietly builds a structured profile of you for matchmaking and runs a six-dimensional affinity model so the companion behaves like a person, not a chatbot.

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

English · [中文](README.zh.md)

## What it does

eros-engine is the conversational layer of a dating platform, carved out as a standalone service. Two things happen in every chat turn:

### 1. Profile-building pipeline (`companion_insights`)

Every user message is mined for facts about the user: city, occupation, interests, MBTI signals, love values, emotional needs, life rhythm, personality traits, matching preferences. These get merged into a single JSONB profile per user with a weighted **training level** that climbs as more dimensions fill in. The profile is structured the way a matchmaker would think — not a vector blob you can't introspect — so it can drive real matchmaking later.

A user who chats freely for a few hours produces a richer dating profile than one who fills out a form, because they're answering the questions they didn't know were being asked.

### 2. Six-dimensional affinity (the "feels like a real person" part)

Most chatbots are stateless. eros-engine isn't. Each chat session carries a six-axis vector that mutates with every turn:

| Axis | Range | What it controls |
|------|------|------|
| **warmth** | −1.0 ↔ 1.0 | Tone and address — cold to affectionate |
| **trust** | 0.0 ↔ 1.0 | Topic depth, willingness to disclose |
| **intrigue** | 0.0 ↔ 1.0 | Curiosity, follow-up questions |
| **intimacy** | 0.0 ↔ 1.0 | Inside jokes, nicknames, callbacks |
| **patience** | 0.0 ↔ 1.0 | Threshold for short or low-effort messages |
| **tension** | 0.0 ↔ 1.0 | Push-pull, playful friction |

Updates use exponential-moving-average smoothing so the persona doesn't lurch, and three axes (intrigue, patience, tension) decay or recover with real time when no one's around. Five relationship labels — `stranger`, `slow_burn`, `friend`, `frenemy`, `romantic` — emerge from the vector by threshold rule, not by being assigned. They're not user-facing; they shape the system prompt the persona generates from.

The vector also drives a deterministic **ghost decision** — when patience and intrigue dip past a threshold, the persona simply doesn't reply. With four protection rules layered on top (no ghosting before message 10, no two ghosts in a row, 1-hour cooldown, raised threshold after a recent ghost) it produces the texture of being slightly absent rather than always available. That single mechanic does more for the "talking to a person" feeling than any prompt-engineering trick.

### Plus a memory layer

Two pgvector tables hold what the persona remembers about you:

- **Profile layer** — cross-session facts (`instance_id IS NULL`), the things any version of any persona could pull up.
- **Relationship layer** — per-session callbacks ("the bookshop you were in that rainy day"), which is what makes someone feel known across weeks of conversation rather than helpfully assistant-shaped.

Embeddings are 512-dimensional via Voyage's `voyage-3-lite`. Retrieval is cosine over IVFFlat.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│ /comp/* HTTP routes  ←  Supabase JWT middleware          │
│         │                                                │
│         ▼                                                │
│   pipeline orchestrator: pre → PDE → handler → chat → post
│                                          │              │
│  ┌───────────────────────────────────────┴────────┐     │
│  │ post-process (background, per turn)            │     │
│  │   • affinity persist (LLM-evaluated 6-dim Δ)   │     │
│  │   • memory   (Voyage embed → pgvector upsert)  │     │
│  │   • insight  (extract facts → companion_insights)
│  └────────────────────────────────────────────────┘     │
└─────────────────────────────────────────────────────────┘
```

Four crates under `crates/`:

| Crate | Role |
|-------|------|
| `eros-engine-core` | Pure-domain logic — affinity vector math, ghost decision, persona decision engine. Zero I/O. |
| `eros-engine-llm` | OpenRouter chat client + Voyage embedding client + TOML model-config loader. |
| `eros-engine-store` | Postgres + pgvector persistence. All tables namespaced under the `engine` schema. |
| `eros-engine-server` | Axum HTTP service + Supabase JWT middleware + pipeline wiring. |

Embed `core + llm + store` as a library to build your own service, or run `eros-engine-server` as a standalone HTTP API.

Deeper docs:
- [Architecture](docs/architecture.md) — crate boundaries, pipeline phases, data flow
- [Affinity model](docs/affinity-model.md) — 6 dimensions, EMA, time decay, relationship labels
- [Ghost mechanics](docs/ghost-mechanics.md) — score formula + protection rules + worked examples
- [Memory layers](docs/memory-layers.md) — profile vs relationship, Voyage, pgvector retrieval
- [Deploying](docs/deploying.md) — Fly.io, Docker compose, bring-your-own-Postgres / IdP
- [API reference](docs/api-reference.md) — every `/comp/*` endpoint

## Quickstart

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env       # fill in: DATABASE_URL, OPENROUTER_API_KEY,
                           #          VOYAGE_API_KEY, SUPABASE_URL,
                           #          SUPABASE_JWT_SECRET
docker compose -f docker/docker-compose.yml up
```

Engine listens on `:8080`. OpenAPI/Scalar reference at `/docs`. Pair with [`eros-engine-web`](https://github.com/etherfunlab/eros-engine-web) for a chat UI that visualises the affinity vector live.

For self-hosters running against an existing Supabase project: tables live under the `engine` Postgres schema, so they coexist cleanly with your other tables.

## API surface

Full reference at `/docs` once running. Highlights:

- `POST /comp/chat/start` — open a session against a persona
- `POST /comp/chat/{session_id}/message` — synchronous chat turn
- `GET  /comp/chat/{session_id}/history` — paginated history
- `GET  /comp/user/{user_id}/profile` — current `companion_insights` + training level
- `GET  /comp/affinity/{session_id}` — live 6-dim vector (env-gated for OSS demo; off in prod-flavoured deploys)

Auth: Bearer Supabase JWT on every `/comp/*` route. The `AuthValidator` trait is pluggable if you bring a different IdP.

## Configuration

| Env var | Required | Notes |
|---------|----------|-------|
| `DATABASE_URL` | yes | Postgres with `pgvector` extension. Engine creates its tables in the `engine` schema. |
| `OPENROUTER_API_KEY` | yes | Chat completions. Routed via `examples/model_config.toml`. |
| `VOYAGE_API_KEY` | yes | Embeddings. Failure modes are loud — empty key fails server boot. |
| `SUPABASE_URL` | yes | Project URL. |
| `SUPABASE_JWT_SECRET` | yes | Project JWT secret. eros-engine validates every incoming token. |
| `EXPOSE_AFFINITY_DEBUG` | no | Set `true` to enable `/comp/affinity/{session_id}`. |
| `EMA_INERTIA` | no | Default `0.8`. |
| `MODEL_CONFIG_PATH` | no | Default `examples/model_config.toml`. |

## What's not here

This repo is the conversational + intimacy core. Things deliberately out of scope:

- **Match-making algorithm** — the multi-stage filter + soft scoring + LLM agent-to-agent simulation lives in the closed-source product. eros-engine builds the *profiles* that feed it, but doesn't pair people up.
- **Full social product UX** — onboarding, video, voice, billing, photos.
- **Companion provenance / lineage** — proprietary.

If you want to build a different product on top — a journaling companion, a language tutor, a coaching agent — the affinity + memory + insight pipeline is the part you'd reuse.

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md). All contributors must accept the [`CLA`](CLA.md) via cla-assistant.io on first PR (one-time, covers all future PRs).

## License

AGPL-3.0. If AGPL doesn't fit your distribution model, commercial licensing is available — `henrylin@etherfun.xyz`.
