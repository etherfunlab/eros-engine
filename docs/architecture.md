# Architecture

[English](architecture.md) · [中文](architecture.zh.md)

## Crates

```
┌─────────────────────────────┐
│ eros-engine-server          │   Axum HTTP, auth middleware,
│   ↓ depends on              │   pipeline wiring
│ eros-engine-store           │   Postgres + pgvector repos,
│   ↓                         │   sqlx migrations
│ eros-engine-llm             │   OpenRouter + Voyage clients,
│   ↓                         │   TOML model config
│ eros-engine-core            │   Pure domain — affinity, ghost,
│                             │   PDE, persona, types. Zero I/O.
└─────────────────────────────┘
```

The dependency graph is strictly downward — `core` doesn't know about `llm`, `llm` doesn't know about `store`, etc. This means:

- `core` is a regular Rust crate you can pull into any other project. No async, no Postgres, no HTTP. Test in milliseconds.
- `llm` and `store` are independent integrations. Swap Voyage for another embedder, or pgvector for another vector DB, by replacing one crate.
- `server` glues them together. If you don't want HTTP, depend on `core + llm + store` directly and embed the engine as a library.

## Pipeline

`pipeline::run(state, session_id, event)` orchestrates a single chat turn:

```
load context           load persona via PersonaRepo
                       load_or_create Affinity → apply_time_decay
                       compute ConversationSignals
       │
       ▼
PDE decide             eros_engine_core::pde::decide(&input) → ActionPlan
                       (rules-based: Reply / Ghost / Proactive / GiftReaction)
       │
       ▼
handler dispatch       Reply  → ReplyHandler  builds ChatRequest
                       Ghost  → GhostHandler  returns None (no chat call)
                       Proact → ProactiveHandler
                       Gift   → GiftHandler   uses event-supplied deltas
       │
       ▼
chat exec              if Some(req): state.openrouter.execute(req).await?
                       persist assistant message via ChatRepo
       │
       ▼
spawn post_process     tokio::spawn — runs concurrent with response return:
                       - affinity persist (LLM evaluates 6-dim Δ → DB)
                       - memory   (Voyage embed → pgvector upsert)
                       - insight  (LLM extracts facts → companion_insights merge)
```

**Ghost-streak reset** is handled by the orchestrator before spawning post-process: on Reply / Proactive / GiftReaction the streak is cleared in a single idempotent UPDATE; on Ghost the orchestrator calls `AffinityRepo::record_ghost` instead. The `persist_with_event` repo method itself never touches the streak.

## Auth

Middleware (`auth::middleware::require_auth`) is layered onto `/comp/*` only. It pulls the `Authorization: Bearer …` header, calls `state.auth.validate(token)`, and inserts an `AuthUser(user_id)` extension into the request. Every protected handler reads `Extension(AuthUser(user_id))`; `user_id` from request bodies is never trusted.

The default validator is `SupabaseJwtValidator` (HS256 against `SUPABASE_JWT_SECRET`). Self-hosters with a different IdP implement the `AuthValidator` trait and inject their impl into `AppState.auth`.

## Data flow

```
Browser / mobile client
    │  Authorization: Bearer <Supabase JWT>
    ▼
eros-engine-server :8080
    │
    ├─► auth middleware → user_id from JWT claims
    │
    ├─► pipeline::run(session_id, event)
    │       │
    │       └─► spawn post_process(state.clone(), …)
    │              │
    │              ▼
    └────────────► Postgres (`engine` schema)
                       chat_sessions / chat_messages
                       companion_affinity / companion_affinity_events
                       companion_memories (vector(512))
                       persona_genomes / persona_instances
                       companion_insights
```

The post-process spawn returns `()` and is fire-and-forget by design — the user-facing response doesn't block on the affinity / memory / insight writes. If any of them fail, the chat reply still lands; failures are logged but not surfaced.

## Why pure-domain core

Two reasons:

1. **Reasoning load.** Affinity math, ghost decisions, and PDE rules are the load-bearing logic. Keeping them I/O-free means a 0-dep cargo test runs in 0ms and never flakes on network. The 25 tests in `core` are the safety net for everything above.
2. **Embeddability.** Anyone wanting to build a different product on top — journaling agent, language tutor, coaching companion — can pull in `core` without inheriting the HTTP shape, the Postgres schema, or the JWT auth. The 6-dim affinity model is the part most worth lifting; we made that easy.

## File structure

```
crates/
├── eros-engine-core/
│   └── src/
│       ├── affinity.rs       # 6-dim vector + EMA + time decay + labels
│       ├── ghost.rs          # score formula + 4-tier protection
│       ├── pde.rs            # rules-based action decision
│       ├── persona.rs        # PersonaGenome + Instance + CompanionPersona
│       └── types.rs          # ActionType / Event / DecisionInput / ConversationSignals
├── eros-engine-llm/
│   └── src/
│       ├── openrouter.rs     # ChatRequest / ChatResponse / fallback retry
│       ├── voyage.rs         # 512-dim embeddings, fail-loud on empty key
│       └── model_config.rs   # TOML loader
├── eros-engine-store/
│   ├── migrations/           # 0000_schema → 0005_insights
│   └── src/
│       ├── pool.rs           # PgPoolOptions builder
│       ├── chat.rs           # ChatRepo
│       ├── affinity.rs       # AffinityRepo (persist_with_event, record_ghost)
│       ├── memory.rs         # MemoryRepo (Profile/Relationship layers)
│       ├── insight.rs        # InsightRepo (weighted training_level)
│       └── persona.rs        # PersonaRepo (upsert_genome for seeding)
└── eros-engine-server/
    └── src/
        ├── main.rs           # serve | migrate | seed-personas subcommands
        ├── state.rs          # AppState (pool/auth/openrouter/voyage/config)
        ├── error.rs          # AppError → axum IntoResponse
        ├── auth/             # AuthValidator trait + Supabase impl + middleware
        ├── pipeline/         # mod (orchestrator) / handlers / post_process
        ├── prompt.rs         # system-prompt builder (affinity → directives)
        ├── routes/           # health / companion / debug / mod
        └── openapi.rs        # utoipa ApiDoc spec metadata
```

## Sub-pages

- [Affinity model](affinity-model.md) — 6 dimensions, EMA, time decay, relationship labels
- [Ghost mechanics](ghost-mechanics.md) — score formula + protection rules + worked examples
- [Memory layers](memory-layers.md) — profile vs relationship, Voyage, pgvector retrieval
- [Deploying](deploying.md) — Docker, Fly.io, bring-your-own-Postgres / IdP
- [API reference](api-reference.md) — every `/comp/*` endpoint
