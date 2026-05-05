# Deploying

[English](deploying.md) · [中文](deploying.zh.md)

Three supported paths, in order of effort:

1. **Fly.io** — what we run for `erosnx.etherfun.net`. ~10 minutes if you've used Fly before.
2. **Docker compose self-host** — single-host VPS, brings its own Postgres+pgvector.
3. **Embed as a library** — `core + llm + store` into your own service, no HTTP layer.

## Prerequisites in all cases

- Postgres 16+ with the `pgvector` extension (≥ 0.7).
- An OpenRouter account (`OPENROUTER_API_KEY`).
- A Voyage AI account (`VOYAGE_API_KEY`).
- Either a Supabase project (for default JWT auth) or your own JWT issuer (implement `AuthValidator`).

## Path 1: Fly.io

The `fly.toml` in the repo root is the production config we use. App name `eros-engine`, region `nrt`, `shared-cpu-1x` 512MB, scale-to-zero. Custom domain via Fly certs.

```bash
# 1. App
flyctl apps create eros-engine --org personal

# 2. Secrets (paste your real values)
flyctl secrets set --app eros-engine \
  DATABASE_URL='postgres://…@…supabase.co:5432/postgres' \
  OPENROUTER_API_KEY='sk-or-…' \
  VOYAGE_API_KEY='pa-…' \
  SUPABASE_URL='https://…supabase.co' \
  SUPABASE_JWT_SECRET='…' \
  SUPABASE_SERVICE_ROLE_KEY='eyJ…'

# Or import from a .env file:
#   grep -E '^(DATABASE_URL|OPENROUTER_API_KEY|VOYAGE_API_KEY|SUPABASE_URL|SUPABASE_JWT_SECRET|SUPABASE_SERVICE_ROLE_KEY)=' .env \
#     | flyctl secrets import -a eros-engine

# 3. First deploy (Fly's remote builder; ~5–10 min on a clean cache)
flyctl deploy --remote-only -a eros-engine

# 4. Custom domain
flyctl certs create your-domain.example.com -a eros-engine
# Add the A + AAAA records that flyctl prints, at your DNS provider.
flyctl certs check your-domain.example.com -a eros-engine

# 5. (Optional) Seed personas
flyctl ssh console -a eros-engine \
  -C "/usr/local/bin/eros-engine seed-personas /etc/eros-engine/personas"
```

The `release_command = "migrate"` in `fly.toml` runs sqlx migrations automatically before each deploy swaps traffic — you don't run migrations by hand.

### Subcommands

The binary has three modes (dispatched by `argv[1]`):

| Subcommand | Purpose |
|------------|---------|
| `serve` (default) | Run the HTTP server on `BIND_ADDR` |
| `migrate` | Apply pending sqlx migrations and exit |
| `seed-personas <dir>` | Read every `*.toml` in `<dir>` and upsert as a persona genome |

`seed-personas` is idempotent — re-runs update existing rows in place (matched by `name`), preserving UUIDs and FK references in `persona_instances`.

## Path 2: Docker compose self-host

For a single-VPS deployment that runs Postgres+pgvector inside the same compose stack:

```yaml
# docker/docker-compose.yml (sketch — adjust ports, volumes, env)
services:
  postgres:
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: eros_engine
    ports: ["5432:5432"]
    volumes:
      - eros_pg:/var/lib/postgresql/data

  engine:
    build:
      context: ..
      dockerfile: docker/Dockerfile
    depends_on: [postgres]
    environment:
      DATABASE_URL: postgres://postgres:postgres@postgres:5432/eros_engine
      OPENROUTER_API_KEY: ${OPENROUTER_API_KEY}
      VOYAGE_API_KEY: ${VOYAGE_API_KEY}
      SUPABASE_JWT_SECRET: ${SUPABASE_JWT_SECRET}
      EXPOSE_AFFINITY_DEBUG: "true"
    ports: ["8080:8080"]

volumes:
  eros_pg:
```

Run with `docker compose -f docker/docker-compose.yml up`. The first boot will run migrations via the `migrate` subcommand entry; subsequent reboots skip already-applied migrations.

Place a real Caddy / Traefik / Cloudflare in front for HTTPS termination.

## Path 3: Embed as a library

If you don't need the HTTP layer — say you're building a different product on top of the affinity + memory pipeline — skip `eros-engine-server` entirely:

```toml
[dependencies]
eros-engine-core  = { git = "https://github.com/etherfunlab/eros-engine", branch = "main" }
eros-engine-llm   = { git = "https://github.com/etherfunlab/eros-engine", branch = "main" }
eros-engine-store = { git = "https://github.com/etherfunlab/eros-engine", branch = "main" }
```

Then construct a pool, repos, LLM clients, and write your own dispatch layer:

```rust
let pool = eros_engine_store::pool::build(&database_url).await?;
let openrouter = eros_engine_llm::openrouter::OpenRouterClient::new(or_key);
let voyage = eros_engine_llm::voyage::VoyageClient::new(voyage_key);

let affinity_repo = eros_engine_store::affinity::AffinityRepo { pool: &pool };
let mut affinity = affinity_repo
    .load_or_create(session_id, user_id, instance_id)
    .await?;

let signals = eros_engine_core::ghost::GhostSignals { … };
match eros_engine_core::ghost::decide(&affinity, signals) {
    GhostDecision::Reply  => { /* run chat */ }
    GhostDecision::Ghost => { /* stay silent */ }
}
```

The migrations file `crates/eros-engine-store/migrations/` ships with the crate; run `sqlx::migrate!()` against your pool the same way `eros-engine-server` does.

## Bring-your-own auth

The default JWT validator is Supabase HS256. Plug another IdP by implementing the trait:

```rust
use async_trait::async_trait;
use eros_engine_server::auth::{AuthError, AuthValidator};
use uuid::Uuid;

pub struct MyValidator { /* … */ }

#[async_trait]
impl AuthValidator for MyValidator {
    async fn validate(&self, bearer: &str) -> Result<Uuid, AuthError> {
        // verify your token here, return the user_id
    }
}
```

Then inject your impl into `AppState.auth: Arc<dyn AuthValidator>`. The middleware (`auth::middleware::require_auth`) is generic over whatever validator you provide.

## Bring-your-own Postgres

Anything compatible with the sqlx Postgres driver works — Supabase, Neon, RDS, Crunchy Bridge, plain self-hosted. Hard requirement: pgvector extension installed (`CREATE EXTENSION vector;`). The engine creates its own schema (`CREATE SCHEMA IF NOT EXISTS engine;` in migration `0000_schema.sql`) so it coexists cleanly with whatever else is in the database.

If you're sharing a database with another service, the engine's tables stay in `engine.*` and never write to `public.*` — collision-free.

## Operational notes

- **Health probe:** `GET /healthz` returns 200 with `{ status: "ok", service, version, timestamp }`. Wire this into your platform's health check.
- **OpenAPI / Scalar:** `GET /docs` serves a live Scalar reference. The OpenAPI JSON is at `/api-docs/openapi.json`.
- **Affinity debug:** `GET /comp/affinity/{session_id}` is gated by `EXPOSE_AFFINITY_DEBUG=true`. Production deploys typically leave it off; the OSS demo turns it on so the radar visualisation in `eros-engine-web` works.
- **Logs:** `RUST_LOG=info` is the default. Set `RUST_LOG=debug,sqlx=warn` to see everything except SQLx query churn.
- **Cost:** the OSS deployment defaults to grok-4-fast (cheap) for chat and grok-4-mini for insight extraction. A typical chat turn costs ≪ $0.001 in token spend plus a Voyage embedding call (~$0.000003 for a memory-worthy fact). 10k chat turns costs single-digit dollars.

## Source

- `fly.toml` — the production config we run
- `docker/Dockerfile` — multi-stage build (Rust 1.88 builder → debian:bookworm-slim runtime)
- `docker/docker-compose.yml` — self-host stack
- `crates/eros-engine-server/src/main.rs` — the three subcommands
