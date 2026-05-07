// SPDX-License-Identifier: AGPL-3.0-only
mod auth;
mod error;
mod openapi;
mod pipeline;
mod prompt;
mod routes;
mod state;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_scalar::{Scalar, Servable};

use crate::auth::supabase::SupabaseJwtValidator;
use crate::auth::AuthValidator;
use crate::openapi::ApiDoc;
use crate::state::{AppState, ServerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // Pull `.env` into the process environment if present. Production
    // deployments (Fly.io secrets, Docker run --env, k8s secrets) won't
    // have a file here and dotenv() returns Err — we ignore it and fall
    // through to the real env. Local quickstart `cp .env.example .env`
    // now works without an explicit `set -a; source .env`.
    let _ = dotenvy::dotenv();

    // The workspace pins `tracing-subscriber` without the `env-filter` feature,
    // so we use the plain fmt initialiser. RUST_LOG is honoured via the
    // tracing-log bridge once the subscriber is installed.
    tracing_subscriber::fmt::init();

    // Subcommand dispatch.
    // Usage:
    //   eros-engine                       run the HTTP service (default)
    //   eros-engine serve                 same as above, explicit
    //   eros-engine migrate               apply sqlx migrations and exit
    //                                     (Fly.io release_command)
    //   eros-engine seed-personas <dir>   load every *.toml in <dir> as a
    //                                     persona genome (idempotent on name)
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("migrate") => run_migrations().await,
        Some("seed-personas") => {
            let dir = args
                .next()
                .unwrap_or_else(|| "/etc/eros-engine/personas".to_string());
            run_seed_personas(&dir).await
        }
        Some("serve") | None => run_server().await,
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: eros-engine [serve|migrate|seed-personas <dir>]");
            std::process::exit(2);
        }
    }
}

/// Read a directory of `*.toml` persona files and insert each into
/// `engine.persona_genomes`. Idempotent: rows are matched by `name`,
/// so re-running won't duplicate or overwrite.
async fn run_seed_personas(dir: &str) -> Result<()> {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct PersonaFile {
        name: String,
        avatar_url: Option<String>,
        tip_personality: Option<String>,
        system_prompt: String,
        #[serde(default)]
        art_metadata: serde_json::Value,
    }

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let pool = eros_engine_store::pool::build(&database_url)
        .await
        .context("failed to connect to DATABASE_URL")?;
    let repo = eros_engine_store::persona::PersonaRepo { pool: &pool };

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir({dir}) failed"))?;
    let mut inserted = 0u32;
    let mut skipped = 0u32;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read {path:?}"))?;
        let f: PersonaFile = toml::from_str(&text)
            .with_context(|| format!("parse {path:?}"))?;

        let (id, created) = repo
            .upsert_genome(
                &f.name,
                &f.system_prompt,
                f.tip_personality.as_deref(),
                f.avatar_url.as_deref(),
                f.art_metadata,
                true,
            )
            .await
            .with_context(|| format!("upsert {}", f.name))?;
        if created {
            tracing::info!(name = %f.name, %id, "persona inserted");
            inserted += 1;
        } else {
            tracing::info!(name = %f.name, %id, "persona refreshed");
            skipped += 1;
        }
    }
    tracing::info!(inserted, skipped, "seed-personas complete");
    Ok(())
}

/// Apply all sqlx migrations and exit. Fly.io invokes this as the
/// release_command before swapping traffic to the new release.
async fn run_migrations() -> Result<()> {
    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let pool = eros_engine_store::pool::build(&database_url)
        .await
        .context("failed to connect to DATABASE_URL")?;
    tracing::info!("running migrations…");
    sqlx::migrate!("../eros-engine-store/migrations")
        .run(&pool)
        .await
        .context("sqlx migrate run failed")?;
    tracing::info!("migrations applied");
    Ok(())
}

async fn run_server() -> Result<()> {
    let cfg = ServerConfig::from_env();

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let pool = eros_engine_store::pool::build(&database_url)
        .await
        .context("failed to connect to DATABASE_URL")?;

    let openrouter_key =
        std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is required")?;
    let openrouter = Arc::new(eros_engine_llm::openrouter::OpenRouterClient::new(
        openrouter_key,
    ));

    let voyage_key = std::env::var("VOYAGE_API_KEY").context("VOYAGE_API_KEY is required")?;
    if voyage_key.trim().is_empty() {
        // Loud-fail vs gateway's silent-skip. The gateway has a known
        // regression where an empty VOYAGE_API_KEY silently disables
        // embeddings; we refuse to boot rather than carry that footgun.
        anyhow::bail!(
            "VOYAGE_API_KEY is empty — eros-engine refuses to boot rather than silently disable embeddings"
        );
    }
    let voyage = Arc::new(eros_engine_llm::voyage::VoyageClient::new(voyage_key));

    let jwt_secret =
        std::env::var("SUPABASE_JWT_SECRET").context("SUPABASE_JWT_SECRET is required")?;
    let auth: Arc<dyn AuthValidator> = Arc::new(SupabaseJwtValidator::new(jwt_secret));

    // model_config: env override > examples/model_config.toml dev default.
    // T14's Dockerfile sets MODEL_CONFIG_PATH=/etc/eros-engine/model_config.toml
    // and copies the file there.
    let model_config_path =
        std::env::var("MODEL_CONFIG_PATH").unwrap_or_else(|_| "examples/model_config.toml".into());
    let model_config_text = std::fs::read_to_string(&model_config_path)
        .with_context(|| format!("model_config read failed: {model_config_path}"))?;
    let model_config = Arc::new(
        eros_engine_llm::model_config::ModelConfig::from_toml_str(&model_config_text)
            .with_context(|| format!("model_config parse failed: {model_config_path}"))?,
    );

    let state = AppState {
        pool,
        auth,
        config: cfg.clone(),
        openrouter,
        voyage,
        model_config,
    };

    // Compose the OpenAPI-aware router. routes::router applies the auth
    // middleware to /comp/* internally; healthz stays public. We seed
    // the spec with `ApiDoc::openapi()` so the title/servers/tags from
    // `openapi.rs` show up at /docs alongside the per-route paths.
    let (open_router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(routes::router(state.clone()))
        .split_for_parts();

    let app: Router = open_router
        .with_state(state)
        .merge(Scalar::with_url("/docs", api))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(
        addr = %cfg.bind_addr,
        debug_affinity = cfg.expose_affinity_debug,
        "eros-engine starting"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
