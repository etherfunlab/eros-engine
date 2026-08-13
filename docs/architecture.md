# Architecture

[English](architecture.md) · [中文](architecture.zh.md)

## Crates

```
┌─────────────────────────────┐
│ eros-engine-server          │   Axum HTTP, auth middleware,
│   ↓ depends on all three    │   pipeline wiring
├─────────────────────────────┤
│ eros-engine-llm             │   OpenRouter + Voyage clients,
│                             │   TOML model config
│ eros-engine-store           │   Postgres + pgvector repos,
│                             │   sqlx migrations
│   ↓ both depend only on     │
├─────────────────────────────┤
│ eros-engine-core            │   Pure domain — affinity, ghost,
│                             │   PDE, persona, types. Zero I/O.
└─────────────────────────────┘
```

The dependency graph is strictly downward — `core` doesn't know about `llm`, `llm` doesn't know about `store`, etc. This means:

- `core` is a regular Rust crate you can pull into any other project. No async, no Postgres, no HTTP. Test in milliseconds.
- `llm` and `store` are independent integrations. Swapping the embedder is a config change now — `[tasks.embedding]` can route to the built-in Voyage client, the built-in OpenRouter endpoint, or any `[providers].<name>.embeddings` URL — not a crate swap; swapping pgvector for another vector DB is still a `store` crate swap.
- `server` glues them together. If you don't want HTTP, depend on `core + llm + store` directly and embed the engine as a library.

## Pipeline

`pipeline::stream::run_stream(state: Arc<AppState>, user_msg: PersistedUserMessage)` orchestrates a single chat turn, returning an SSE frame stream:

```
load context           load persona via PersonaRepo
                       load_or_create Affinity → apply_time_decay
                       compute ConversationSignals
       │
       ▼
PDE decide             eros_engine_core::pde::decide(&input) → ActionPlan
                       (rules-based by default; opt-in LLM judge via
                        [tasks.pde_decision].filter_prompt, fail-open to
                        rule engine; verdict audited to
                        companion_decision_events — payload = what the model
                        returned, inputs = the engine-computed state it saw)
       │
       ▼
action dispatch        inline `match plan.action_type` in run_stream:
                       Ghost     → mark row ghosted + record_ghost (no chat call)
                       ProductQa → independent product-QA executor
                       else      → reply path builds ChatRequest
       │
       ▼
chat exec              if Some(req): state.openrouter.execute(req).await?
                       (reply_text_image: the image composer is tokio::spawn'ed
                        before the chat call and joined after the text reply
                        streams — the compose hop hides under the chat call)
                       persist assistant message via ChatRepo
       │
       ▼
spawn post_process     tokio::spawn — runs concurrent with response return:
                       - affinity persist (LLM judge grades the turn →
                         engine grade pipeline → DB)
                       - memory   (Voyage embed → pgvector upsert)
                       - insight  (LLM extracts facts → human_insights UPSERT,
                         per-column incremental)
                       (the three run concurrently via tokio::join!)
```

**Ghost-streak reset** is handled by the orchestrator before spawning post-process: on Reply / Proactive the streak is cleared in a single idempotent UPDATE; on Ghost the orchestrator calls `AffinityRepo::record_ghost` instead. The `persist_with_event` repo method itself never touches the streak.

**PDE action list.** The judge's (or rule engine's) per-turn action is one of
`reply_text` | `ghost` | `reply_image` | `reply_text_image` | `product_qa`.
Three of these are conditionally available: `reply_image` and
`reply_text_image` require **both** an `image` block on the request **and**
`[tasks.chat_image_prompt_compose]` configured — since the judge stopped
writing image-prompt seeds the composer is the only thing that can produce an
image prompt, so a missing composer task leaves image turns unavailable;
`product_qa` requires `[tasks.chat_product_qa]` configured (with the LLM PDE
enabled). Each degrades to `reply_text` when unavailable — never upgrades.
`product_qa` short-circuits the whole companion chain (no persona prompt, no
post-process): it routes to an independent product-QA executor instead of the
reply path. See [model-config.md](model-config.md) for the per-action
gates.

## Auth

Middleware (`auth::middleware::require_auth`) protects everything except `/healthz` (merged outside the layer) and `/docs` (merged in `main.rs`) — today that means `/comp/*`, `/bff/v1/*`, `/world/*`, and `/persona/*`. The layer attaches to the merged sub-router in `routes/mod.rs`, not to a path prefix, which is why a new namespace (e.g. `/persona/*`) needs no extra auth wiring. It pulls the `Authorization: Bearer …` header, calls `state.auth.validate(token)`, and inserts an `AuthUser(user_id)` extension into the request. Every protected handler reads `Extension(AuthUser(user_id))`; `user_id` from request bodies is never trusted.

The default validator is `SupabaseJwtValidator`; it dispatches per-token on the JWT `alg` header, preferring JWKS-based asymmetric verification (ES256/RS256/EdDSA) — the default since Supabase's 2025 JWT Signing Keys rollout — sourced from `SUPABASE_JWKS_URL` or derived from `SUPABASE_URL`, and falling back to legacy HS256 against `SUPABASE_JWT_SECRET` for projects that haven't migrated. Self-hosters with a different IdP implement the `AuthValidator` trait and inject their impl into `AppState.auth`.

## Data flow

```
Browser / mobile client
    │  Authorization: Bearer <Supabase JWT>
    ▼
eros-engine-server :8080
    │
    ├─► auth middleware → user_id from JWT claims
    │
    ├─► pipeline::stream::run_stream(state, user_msg)
    │       │
    │       └─► spawn post_process(state.clone(), …)
    │              │
    │              ▼
    ├─► routes::persona (/persona/{instance_id}/image/compose)
    │       └─► image-prompt composer LLM only — nothing persisted
    │
    └────────────► Postgres (`engine` schema)
                       chat_sessions / chat_messages
                       companion_affinity / companion_affinity_events
                       companion_memories (vector(512))
                       persona_genomes / persona_instances
                       human_insights
                       companion_decision_events
```

The post-process spawn returns `()` and is fire-and-forget by design — the user-facing response doesn't block on the affinity / memory / insight writes. If any of them fail, the chat reply still lands; failures are logged but not surfaced.

**`chat_messages.channel`**: `NULL` = normal text; `'voice'` = voice channel;
`'product_qa'` = out-of-character product answer — non-NULL rows are
excluded from short-term recall, conversation signals, affinity evaluation,
and insight extraction while staying fully visible on the live stream,
replay, and client history. Dreaming is the exception: it also reads
`'voice'` rows by default (once a call goes idle; opt out with
`DREAMING_VOICE_DISABLED`), but still excludes `'product_qa'`.

**Channel ownership is enforced on writes, both ways.** `companion_stream`
refuses a voice session and `voice` refuses a non-voice one, each with
`409 wrong_channel` before persisting anything. Without that symmetry a text
turn could land in a voice conversation, and its `client_msg_id` could then
collide with the voice-turn lookups.

**A voice turn holds at most one assistant reply**, enforced by a partial
unique index on `(user_message_id) WHERE role='assistant' AND channel='voice'`
(migration 0041). The text path has no such constraint — it legitimately
writes several assistant rows per user turn via `continues_from_message_id`.
The index exists because a voice turn has two possible writers: the streaming
generator, and the barge-in interrupt endpoint reporting what the client
actually played. They are ordered by a shared `FOR UPDATE` lock on the user
row, and `content` belongs to the interrupt while the audit columns
(`model` / `usage` / `generation_id`) belong to the generator. A user row
carrying `metadata.voice_interrupt` marks a turn the user deliberately cut
off — which is also what tells a retry apart from a repairable disconnect.
See [voice barge-in](superpowers/specs/2026-08-11-voice-barge-in-interrupt-design.md).

## Why pure-domain core

Two reasons:

1. **Reasoning load.** Affinity math, ghost decisions, and PDE rules are the load-bearing logic. Keeping them I/O-free means a 0-dep cargo test runs in 0ms and never flakes on network. The 69 tests in `core` are the safety net for everything above. (The opt-in LLM judge layer lives in `server`, not `core`, so `core` stays zero-I/O.)
2. **Embeddability.** Anyone wanting to build a different product on top — journaling agent, language tutor, coaching companion — can pull in `core` without inheriting the HTTP shape, the Postgres schema, or the JWT auth. The 6-dim affinity model is the part most worth lifting; we made that easy.

## File structure

```
crates/
├── eros-engine-core/
│   └── src/
│       ├── affinity.rs       # 6-dim vector + grade pipeline + time decay + labels
│       ├── ghost.rs          # score formula + 4-tier protection
│       ├── pde.rs            # rules-based action decision
│       ├── persona.rs        # PersonaGenome + Instance + CompanionPersona
│       ├── scope.rs          # per-request injection scope (InsightMode / MemoryScope)
│       └── types.rs          # ActionType / Event / DecisionInput / ConversationSignals
├── eros-engine-llm/
│   └── src/
│       ├── openrouter.rs     # ChatRequest / ChatResponse / fallback retry
│       ├── voyage.rs         # 512-dim embeddings, fail-loud on empty key
│       └── model_config.rs   # TOML loader
├── eros-engine-store/
│   ├── migrations/           # 0000_schema → 0044_decision_event_inputs
│   └── src/
│       ├── pool.rs           # PgPoolOptions builder
│       ├── chat.rs           # ChatRepo
│       ├── affinity.rs       # AffinityRepo (persist_with_event, record_ghost)
│       ├── memory.rs         # MemoryRepo (Profile/Relationship layers)
│       ├── insight.rs        # InsightEventRepo (companion_insights_events audit rows)
│       ├── human_insight.rs  # HumanInsightRepo (typed profile columns, per-field UPSERT)
│       └── persona.rs        # PersonaRepo (upsert_genome for seeding)
└── eros-engine-server/
    └── src/
        ├── main.rs           # serve | migrate | seed-personas | print-openapi subcommands
        ├── state.rs          # AppState (pool/auth/openrouter/embed/config)
        ├── error.rs          # AppError → axum IntoResponse
        ├── auth/             # AuthValidator trait + Supabase impl + middleware
        ├── pipeline/         # stream (run_stream) / handlers / post_process / dreaming / …
        ├── prompt.rs         # system-prompt builder (affinity → directives)
        ├── routes/           # health / companion / companion_stream / voice / persona / world_town / bff / dto / debug / mod
        └── openapi.rs        # utoipa ApiDoc spec metadata
```

## Sub-pages

- [Affinity model](affinity-model.md) — 6 dimensions, graded write pipeline, time decay, relationship labels
- [Ghost mechanics](ghost-mechanics.md) — score formula + protection rules + worked examples
- [Memory layers](memory-layers.md) — profile vs relationship, Voyage, pgvector retrieval
- [Deploying](deploying.md) — Docker, bring-your-own Postgres / IdP
- [API reference](api-reference.md) — every `/comp/*` endpoint
