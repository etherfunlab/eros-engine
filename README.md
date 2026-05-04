# eros-engine

> **Open-source AI companion engine** — chat + 6-dimensional intimacy/affinity model + ghost mechanics + pgvector memory. The intimacy-modeling backbone of the [Eros](https://eros.ai) dating platform.

[![CI](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/etherfunlab/eros-engine/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

## What it does

eros-engine takes user messages, runs them through a Persona Decision Engine (PDE) backed by a 6-dim affinity vector (warmth / trust / intrigue / intimacy / patience / tension), and decides whether the AI replies, ghosts, sends an unprompted message, or reacts to a gift event. After every interaction it writes deltas to the affinity vector with EMA smoothing, embeds the message into pgvector for long-term memory, and extracts structured facts about the user into `companion_insights`.

The 6-dim vector is the open educational hook: most chatbots are stateless. This one isn't, and you can watch the state move.

## Quickstart

```bash
git clone https://github.com/etherfunlab/eros-engine
cd eros-engine
cp .env.example .env  # fill in OPENROUTER_API_KEY, VOYAGE_API_KEY, SUPABASE_JWT_SECRET
docker compose -f docker/docker-compose.yml up
```

Engine listens on `:8080`. Pair with [`eros-engine-web`](https://github.com/etherfunlab/eros-engine-web) for a chat UI with a live affinity radar.

## Architecture

See [`docs/architecture.md`](docs/architecture.md). Four crates: `core` (pure logic), `llm` (OpenRouter + Voyage), `store` (Postgres+pgvector), `server` (Axum). Embed as a library or run as a service.

## Contributing

Read [`CONTRIBUTING.md`](CONTRIBUTING.md). All contributors must accept the [`CLA`](CLA.md) via cla-assistant.io on first PR.

## License

AGPL-3.0. Commercial licensing available — contact `oss@etherfunlab.dev`.
