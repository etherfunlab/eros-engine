// SPDX-License-Identifier: AGPL-3.0-only
//! The fact-group sweeper (spec 2026-11-25-fact-groups-design.md §5.2):
//! embeds ledger facts that are not placed yet and places each one into the
//! group of an earlier fact it restates. New facts and the rows that predate
//! grouping go through the same queue, oldest first.

use eros_engine_store::human_fact::{HumanFactRepo, Placement};

use crate::state::AppState;

/// What one batch did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PlaceTally {
    /// Unplaced rows read for the batch.
    pub read: usize,
    pub joined: usize,
    pub alone: usize,
    /// Rows gone or placed elsewhere meanwhile, or whose placement failed.
    pub skipped: usize,
}

/// Place one batch: the oldest unplaced rows, embedded in one call (none when
/// there are none), then placed one at a time in order. `Err` when the read
/// or the embedding call fails; nothing is placed then and the rows wait for
/// the next batch. A single placement that fails is logged and counted as
/// skipped, and the batch goes on.
pub(crate) async fn place_batch(state: &AppState) -> Result<PlaceTally, String> {
    let cfg = &state.config.fact_grouping;
    let repo = HumanFactRepo { pool: &state.pool };
    let facts = repo
        .unplaced(cfg.batch)
        .await
        .map_err(|e| format!("reading unplaced facts failed: {e}"))?;
    let mut tally = PlaceTally {
        read: facts.len(),
        ..Default::default()
    };
    if facts.is_empty() {
        return Ok(tally);
    }
    let texts: Vec<&str> = facts.iter().map(|f| f.content.as_str()).collect();
    let vectors = state
        .embed
        .embed_documents(&texts)
        .await
        .map_err(|e| format!("embedding failed: {e}"))?;
    for (fact, vector) in facts.iter().zip(&vectors) {
        match repo.place(fact, vector, cfg.threshold).await {
            Ok(Placement::Joined) => tally.joined += 1,
            Ok(Placement::Alone) => tally.alone += 1,
            Ok(Placement::Skipped) => tally.skipped += 1,
            Err(e) => {
                tracing::warn!(fact_id = %fact.id, "fact groups: placing a fact failed: {e}");
                tally.skipped += 1;
            }
        }
    }
    Ok(tally)
}

/// Run forever, placing new ledger facts into groups. Spawn once at server
/// startup; returns at once when the configured tick is zero. A full batch
/// starts the next one at once, so a backlog drains without waiting for the
/// tick.
pub async fn sweeper(state: AppState) {
    let cfg = state.config.fact_grouping.clone();
    if cfg.tick.is_zero() {
        tracing::info!("fact-group sweeper disabled (FACT_GROUPING_DISABLED=1 or tick=0)");
        return;
    }
    tracing::info!(
        tick = ?cfg.tick,
        batch = cfg.batch,
        threshold = cfg.threshold,
        "fact-group sweeper starting"
    );

    let mut tick = tokio::time::interval(cfg.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mut total = PlaceTally::default();
        loop {
            match place_batch(&state).await {
                Ok(t) => {
                    let full = t.read as i64 == cfg.batch;
                    total.read += t.read;
                    total.joined += t.joined;
                    total.alone += t.alone;
                    total.skipped += t.skipped;
                    if !full {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("fact groups: {e}");
                    break;
                }
            }
        }
        if total.read > 0 {
            tracing::info!(
                read = total.read,
                joined = total.joined,
                alone = total.alone,
                skipped = total.skipped,
                "fact groups: facts placed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::path as wm_path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::state::AppState;

    /// See `human_fact` tests: cos(a − b) between `unit(a)` and `unit(b)`.
    fn unit(deg: f64) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[0] = deg.to_radians().cos() as f32;
        v[1] = deg.to_radians().sin() as f32;
        v
    }

    fn h(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 25, hour, 0, 0).unwrap()
    }

    async fn seed(pool: &PgPool, user: Uuid, content: &str, first_seen: DateTime<Utc>) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.human_facts \
               (user_id, content, category, domain, evidence_type, temporality, \
                persistence, confidence, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'other', 'hobby', 'explicit_statement', 'current', 'stable', \
                     'high', $3, $3) RETURNING id",
        )
        .bind(user)
        .bind(content)
        .bind(first_seen)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A test state whose write embedder is `server`, batch 500, threshold 0.96.
    fn state_with(pool: PgPool, server: &MockServer) -> AppState {
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
            "[providers.mockembed]\nembeddings = \"{}/v1/embeddings\"\n\
             [tasks.embedding]\nmodel = \"mock-embed@mockembed\"\n",
            server.uri()
        ))
        .unwrap();
        let mut state = crate::routes::companion::test_state(pool);
        state.embed = std::sync::Arc::new(
            eros_engine_llm::embedding::EmbeddingRouter::from_config_with(&cfg, |k| {
                (k == "MOCKEMBED_API_KEY").then(|| "test-key".to_string())
            })
            .unwrap(),
        );
        state.config.fact_grouping.batch = 500;
        state.config.fact_grouping.threshold = 0.96;
        state
    }

    async fn placement(pool: &PgPool, id: Uuid) -> (Uuid, bool) {
        sqlx::query_as(
            "SELECT group_id, embedding IS NOT NULL FROM engine.human_facts WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn place_batch_embeds_once_and_groups_a_paraphrase(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed(&pool, user, "用户喜欢爬山", h(1)).await;
        let b = seed(&pool, user, "用户很喜欢爬山", h(2)).await;
        let server = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "embedding": unit(0.0), "index": 0 },
                    { "embedding": unit(10.0), "index": 1 }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let state = state_with(pool.clone(), &server);

        let tally = place_batch(&state).await.unwrap();
        assert_eq!(
            tally,
            PlaceTally {
                read: 2,
                joined: 1,
                alone: 1,
                skipped: 0
            }
        );
        let (ga, pa) = placement(&pool, a).await;
        let (gb, pb) = placement(&pool, b).await;
        assert!(pa && pb, "both placed");
        assert_eq!(ga, gb, "the later paraphrase joined the first fact's group");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn place_batch_leaves_rows_unplaced_on_an_embedding_error(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed(&pool, user, "用户喜欢爬山", h(1)).await;
        let b = seed(&pool, user, "用户很喜欢爬山", h(2)).await;
        let server = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let state = state_with(pool.clone(), &server);

        assert!(place_batch(&state).await.is_err());
        assert!(!placement(&pool, a).await.1);
        assert!(!placement(&pool, b).await.1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn place_batch_with_nothing_unplaced_calls_nothing(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let state = state_with(pool, &server);
        assert_eq!(place_batch(&state).await.unwrap(), PlaceTally::default());
    }
}
