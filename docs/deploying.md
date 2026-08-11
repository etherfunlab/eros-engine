# Deploying

[English](deploying.md) · [中文](deploying.zh.md)

Two supported paths, in order of effort:

1. **Docker self-host** — pull the prebuilt GHCR image (or build from `docker/Dockerfile`); single-host VPS brings its own Postgres+pgvector.
2. **Embed as a library** — `core + llm + store` from crates.io into your own service, no HTTP layer.

## Prerequisites in all cases

- Postgres 16+ with the `pgvector` extension (≥ 0.7).
- An OpenRouter account (`OPENROUTER_API_KEY`).
- A Voyage AI account (`VOYAGE_API_KEY`) — required unless `[tasks.embedding]` routes both read and write off Voyage (see [Model config](model-config.md)); the default (no `[tasks.embedding]` block) still needs it.
- Either a Supabase project (for default JWT auth) or your own JWT issuer (implement `AuthValidator`).

## Subcommands

The binary has four modes (dispatched by `argv[1]`):

| Subcommand | Purpose |
|------------|---------|
| `serve` (default) | Run the HTTP server on `BIND_ADDR` |
| `migrate` | Apply pending sqlx migrations and exit |
| `seed-personas [dir]` | Read every `*.toml` in `[dir]` (default `/etc/eros-engine/personas` — the examples baked into the Docker image) and upsert as a persona genome |
| `print-openapi` | Dump the OpenAPI spec to stdout and exit (no DB, no env; used by the CI drift check) |

`seed-personas` is idempotent — re-runs update existing rows in place (matched by `name`), preserving UUIDs and FK references in `persona_instances`.

## Path 1: Docker self-host

Multi-arch (`linux/amd64` + `linux/arm64`) images of `eros-engine-server` are published to GitHub Container Registry for every `v*` tag (`docker/Dockerfile` builds the same artifact if you'd rather build your own):

```bash
docker pull ghcr.io/etherfunlab/eros-engine:latest   # or pin a version tag
```

For a single-VPS deployment that runs Postgres+pgvector next to the engine, a compose file along these lines works (the repo ships no compose file — write your own; adjust ports, volumes, env):

```yaml
# compose.yml (sketch)
services:
  postgres:
    image: pgvector/pgvector:pg16
    environment:
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: eros_engine
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres"]
      interval: 2s
    volumes:
      - eros_pg:/var/lib/postgresql/data

  engine:
    image: ghcr.io/etherfunlab/eros-engine:latest  # pin a version tag in production
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      DATABASE_URL: postgres://postgres:postgres@postgres:5432/eros_engine
      OPENROUTER_API_KEY: ${OPENROUTER_API_KEY}
      VOYAGE_API_KEY: ${VOYAGE_API_KEY}
      SUPABASE_URL: ${SUPABASE_URL}   # JWKS auth — the post-2025 Supabase default
      # SUPABASE_JWT_SECRET: ${SUPABASE_JWT_SECRET}   # legacy HS256 alternative
    ports: ["8080:8080"]

volumes:
  eros_pg:
```

Nothing migrates automatically — run the `migrate` subcommand before the first `serve` and again after every image upgrade:

```bash
docker compose up -d postgres
docker compose run --rm engine migrate
docker compose run --rm engine seed-personas /etc/eros-engine/personas  # optional: example personas (manual step — upserts by name)
docker compose up -d engine
```

At least one auth source must be wired — `SUPABASE_URL` (or `SUPABASE_JWKS_URL`) for asymmetric JWKS validation, or a non-empty legacy `SUPABASE_JWT_SECRET`. With neither, the engine refuses to boot — by design, so a misconfigured deploy fails loudly instead of silently rejecting every request.

**Model config:** the image bakes the sanitized `examples/model_config.toml` at `/etc/eros-engine/model_config.toml` and presets `MODEL_CONFIG_PATH` to it, so the container boots as-is. Note the baked example includes live `[tasks.world_*]` sections — harmless with zero world enrollments (no LLM calls), but the world sweepers do run; set `WORLD_DISABLED=true` if you want the [World system](world-system.md) fully inert.

Conversely, `[tasks.chat_image_prompt_compose]` is commented out in the baked example — and the image-prompt composer is required for image capability, so out of the box image turns stay unavailable and image actions silently degrade to text until you configure that block. See [Model config → image-prompt composer](model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns).

For a real deployment, mount your own config and point `MODEL_CONFIG_PATH` at it — or set `MODEL_CONFIG_DIR` to a mounted directory of `.toml` fragments merged at boot. The two are mutually exclusive, and the image presets `MODEL_CONFIG_PATH`, so going the directory route means clearing it explicitly (`MODEL_CONFIG_PATH=` — empty counts as unset). See [Model config](model-config.md).

Place a real Caddy / Traefik / Cloudflare in front for HTTPS termination.

## Path 2: Embed as a library

If you don't need the HTTP layer — say you're building a different product on top of the affinity + memory pipeline — skip `eros-engine-server` entirely. The three library crates are published on crates.io:

```toml
[dependencies]
eros-engine-core  = "1.0"
eros-engine-llm   = "1.0"
eros-engine-store = "1.0"
```

(To track unreleased work, use `{ git = "https://github.com/etherfunlab/eros-engine", branch = "main" }` instead.)

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

The migration files ship with the `eros-engine-store` crate under `migrations/`; point `sqlx::migrate!("<path>")` at that directory and run it against your pool. The macro needs a compile-time path, so vendor the directory or use a path dependency — the server itself does `sqlx::migrate!("../eros-engine-store/migrations")`.

## Bring-your-own auth

The default JWT validator is Supabase — JWKS asymmetric (ES256/RS256/EdDSA) via `SUPABASE_URL` / `SUPABASE_JWKS_URL`, with a legacy HS256 shared-secret fallback (`SUPABASE_JWT_SECRET`). Plug another IdP by implementing the trait:

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

(`eros-engine-server` is intentionally not published as a library, so this is guidance for running a fork of the server. Path 2 embedders skip the HTTP auth layer entirely and enforce their own.)

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

### Prompt logging (debug, optional)

Set `PROMPT_LOG_DIR` to capture the fully-assembled main-reply prompt for each
turn as one human-readable file (header + per-message blocks). It is **off by
default**, **operator-only** (files contain raw chat content), and writes
fire-and-forget so it never blocks or fails a reply. Point it at a mounted
volume you control:

```yaml
# docker-compose: mount a volume and set the env
services:
  engine:
    environment:
      PROMPT_LOG_DIR: /data/prompt-logs
    volumes:
      - ./prompt-logs:/data/prompt-logs
```

```toml
# fly.io: declare a mount + the env (illustrative)
[mounts]
source = "prompt_logs"
destination = "/data/prompt-logs"

[env]
PROMPT_LOG_DIR = "/data/prompt-logs"
```

There is no built-in rotation or retention — manage the volume yourself.

### World system (experimental, optional)

The [World system](world-system.md) (World Memories simulation + World Town
feed + World Stories per-instance life simulation) is fully off by default:
without a `[tasks.world_director]` model-config section it spawns no sweepers
and runs zero per-turn queries. Turning it on is a config + data decision, not
a deploy change:

1. Add the `[tasks.world_*]` sections to your model config (see
   [`examples/model_config.toml`](../examples/model_config.toml)).
2. Enroll owners by inserting rows into `engine.world_enrollments` over a
   `service_role` / owner connection (the engine only reads this table); set
   `town_enabled = true` per owner to also enable the feed, and
   `stories_enabled = true` to also enable World Stories.

- `engine.world_worldviews` — per-owner worldview text (1..=10000 chars).
  Downstream-written, engine-read. The engine ships no default: enrolled
  owners without a row (or with blank content) get **no** World System LLM
  activity until one is provided. Updating the content resets that owner's
  world on the next tick (published town posts are kept as history).

Operational switches, all optional:

| Variable | Effect |
|----------|--------|
| `WORLD_DISABLED=true` | Master off: no sweepers, no prompt injection, zero cost |
| `WORLD_PROMPT_DISABLED=true` | Simulate + accumulate, but don't touch chat prompts (staged-rollout valve) |
| `WORLD_TICK_SECS` | Director sweeper tick (default 300; `0` disables) |
| `WORLD_TOWN_DISABLED=true` | Town only: no post generation, no town sweeper; memories keep running |
| `WORLD_STORIES_DISABLED=true` | Stories only: no life rounds, no `[world_stories]` injection; memories keep running |
| `WORLD_STORIES_PROMPT_DISABLED=true` | Keep simulating lives, but don't touch chat prompts (staged-rollout valve) |

Cost shape: one director call per enrolled owner **with a usable worldview**
per `interval_hours`, plus (town only) activity-gated hourly comment rounds
and per-owner-capped replies, plus (stories only) per-instance life rounds on
their own cadence. Owners without a worldview (see above) are skipped entirely
and cost nothing. A world nobody interacts with costs exactly the director
call. Details, data model, and the boot-validation rules are in
[World system](world-system.md).

- **Env vars:** the complete variable list lives in [`.env.example`](../.env.example); it is deliberately terse — details live in this guide and in [model-config.md](model-config.md).
- **Background sweepers:** `serve` also runs the dreaming-lite (session-end memory classifier) and insight-snapshot sweepers. Both are optional: `DREAMING_DISABLED=1` / `SNAPSHOT_DISABLED=1` turn them off without affecting the chat path. Dreaming wakes every `DREAMING_TICK_SECS` (default 300) and classifies sessions idle for at least `DREAMING_IDLE_SECS` (default 1800); a classification claim older than `DREAMING_CLAIM_STALE_SECS` (default 600) is treated as a crashed worker and re-claimed. `DREAMING_VOICE_DISABLED=1` narrows the sweeper back to text-only sessions (by default, ended voice calls are distilled into memories too — see [memory-layers.md](memory-layers.md#voice-turns)). The snapshot sweeper runs on a 6-field cron `SNAPSHOT_CRON` (default `0 0 23 * * *`) in `SNAPSHOT_TZ` (default `Asia/Singapore`); an unparseable cron means the sweeper never starts (chat path unaffected), and an unparseable zone falls back to the default.
- **OpenRouter attribution (optional):** declare a `headers` table under `[providers.openrouter]` in the model config — `HTTP-Referer` / `X-OpenRouter-Title` / `X-OpenRouter-Categories` — to add attribution headers to every outbound OpenRouter call so the deployment shows up on OpenRouter's app dashboard; leave the entry (or its `headers` key) absent to stay anonymous. See [Model config → Built-in endpoint overrides](model-config.md#built-in-endpoint-overrides-via-providersopenrouter). The old `OPENROUTER_APP_REFERER` / `OPENROUTER_APP_TITLE` / `OPENROUTER_APP_CATEGORIES` env vars are soft-deprecated: still-set values are silently ignored, never a boot error, and `OPENROUTER_BASE_URL` has been removed outright — override the endpoint URL via `[providers].openrouter.chat` / `.embeddings` instead.
- **Health probe:** `GET /healthz` returns 200 with `{ status: "ok", service, version, timestamp }`. Wire this into your platform's health check.
- **OpenAPI / Scalar:** `GET /docs` serves a live Scalar reference. The raw OpenAPI JSON is not served over HTTP — dump it with the `print-openapi` subcommand.
- **Affinity debug:** `GET /comp/affinity/{session_id}` is gated by `EXPOSE_AFFINITY_DEBUG=true`. Production deploys typically leave it off; turn it on if your frontend renders a live radar of the affinity vector.
- **Logs:** `RUST_LOG=info` is the default. Set `RUST_LOG=debug,sqlx=warn` to see everything except SQLx query churn.
- **Cost:** the OSS deployment defaults to a fast, cheap model for chat and a capable extraction model for insight extraction (see `examples/model_config.toml` for current defaults). A typical chat turn costs ≪ $0.001 in token spend plus a Voyage embedding call (~$0.000003 for a memory-worthy fact). 10k chat turns costs single-digit dollars.

## Source

- `docker/Dockerfile` — multi-stage build (Rust 1.88 builder → debian:bookworm-slim runtime); the same artifact behind `ghcr.io/etherfunlab/eros-engine`
- `crates/eros-engine-server/src/main.rs` — subcommand dispatch (the four modes above)
- [`.env.example`](../.env.example) — operational env-var list (details in this guide)
