# Deploying

[English](deploying.md) · [中文](deploying.zh.md)

Two supported paths, in order of effort:

1. **Docker compose self-host** — single-host VPS, brings its own Postgres+pgvector.
2. **Embed as a library** — `core + llm + store` into your own service, no HTTP layer.

## Prerequisites in all cases

- Postgres 16+ with the `pgvector` extension (≥ 0.7).
- An OpenRouter account (`OPENROUTER_API_KEY`).
- A Voyage AI account (`VOYAGE_API_KEY`).
- Either a Supabase project (for default JWT auth) or your own JWT issuer (implement `AuthValidator`).

## Subcommands

The binary has three modes (dispatched by `argv[1]`):

| Subcommand | Purpose |
|------------|---------|
| `serve` (default) | Run the HTTP server on `BIND_ADDR` |
| `migrate` | Apply pending sqlx migrations and exit |
| `seed-personas <dir>` | Read every `*.toml` in `<dir>` and upsert as a persona genome |

`seed-personas` is idempotent — re-runs update existing rows in place (matched by `name`), preserving UUIDs and FK references in `persona_instances`.

## Path 1: Docker compose self-host

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

This compose file wires only the legacy `SUPABASE_JWT_SECRET`, so export a non-empty value before `up`. An empty or unset secret with no JWKS source makes the engine refuse to boot — by design, so a misconfigured deploy fails loudly instead of silently rejecting every request. For asymmetric JWKS validation instead, add `SUPABASE_URL` (or `SUPABASE_JWKS_URL`) to the `environment:` block.

Place a real Caddy / Traefik / Cloudflare in front for HTTPS termination.

## Path 2: Embed as a library

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

### Supabase deployments — schema-exposure footgun

If your Postgres provider is Supabase **and** you've added `engine` to the project's Exposed Schemas list (Studio → Settings → API → Exposed schemas) so a co-deployed web app can read `engine.*` through `@supabase/supabase-js`, you've also potentially exposed every `engine.*` table to the publishable `anon` key — depending on which roles Studio's Permissions panel granted SELECT/INSERT/etc to.

The hazard: a holder of the publishable anon key (which ships in every browser bundle by design) can issue:

```bash
curl "https://<project>.supabase.co/rest/v1/chat_messages?select=*&limit=5" \
  -H "apikey: <publishable-anon-key>"
```

…and read every user's chat history if `anon` was ever granted SELECT on `engine.chat_messages`.

Migration `0013_supabase_lockdown.sql` (shipped with eros-engine 0.2+) closes this by:

1. `REVOKE ALL` on every `engine.*` table from `anon` and `authenticated`
2. `REVOKE USAGE ON SCHEMA engine` from `anon` and `authenticated`
3. `ENABLE ROW LEVEL SECURITY` on every `engine.*` table (no policies — defense in depth; the `postgres` owner and `service_role` bypass RLS, which covers the engine binary and any server-side Supabase client)

The migration is guarded by `IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon')`, so non-Supabase Postgres deployments (Neon, RDS, plain self-hosted) skip the REVOKEs silently and only inherit the harmless RLS enable.

**If you upgraded from a pre-0.2 release on Supabase, run `eros-engine migrate` once to apply this — it's idempotent.**

To audit your project independently of this migration, run as the `postgres` role:

```sql
-- Which tables in engine.* are missing RLS?
SELECT relname FROM pg_class
 WHERE relnamespace = 'engine'::regnamespace
   AND relkind = 'r' AND NOT relrowsecurity;

-- Which engine.* tables expose anything to anon / authenticated?
SELECT grantee, table_name, privilege_type
  FROM information_schema.role_table_grants
 WHERE table_schema = 'engine'
   AND grantee IN ('anon', 'authenticated');
```

Both queries should return zero rows after the migration applies.

## Operational notes

- **Health probe:** `GET /healthz` returns 200 with `{ status: "ok", service, version, timestamp }`. Wire this into your platform's health check.
- **OpenAPI / Scalar:** `GET /docs` serves a live Scalar reference. The OpenAPI JSON is at `/api-docs/openapi.json`.
- **Affinity debug:** `GET /comp/affinity/{session_id}` is gated by `EXPOSE_AFFINITY_DEBUG=true`. Production deploys typically leave it off; turn it on if your frontend renders a live radar of the affinity vector.
- **Logs:** `RUST_LOG=info` is the default. Set `RUST_LOG=debug,sqlx=warn` to see everything except SQLx query churn.
- **Cost:** the OSS deployment defaults to a fast, cheap model for chat and a capable extraction model for insight extraction (see `examples/model_config.toml` for current defaults). A typical chat turn costs ≪ $0.001 in token spend plus a Voyage embedding call (~$0.000003 for a memory-worthy fact). 10k chat turns costs single-digit dollars.

## Source

- `docker/Dockerfile` — multi-stage build (Rust 1.88 builder → debian:bookworm-slim runtime)
- `docker/docker-compose.yml` — self-host stack
- `crates/eros-engine-server/src/main.rs` — the three subcommands
