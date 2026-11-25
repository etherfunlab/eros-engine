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
    /// Rows gone or placed elsewhere meanwhile.
    pub skipped: usize,
    /// Rows whose placement failed, and the same user's later rows held back
    /// behind them; all stay unplaced.
    pub failed: usize,
}

/// Place one batch: the oldest unplaced rows, embedded in one call (none when
/// there are none), then placed one at a time in order. `Err` when the read
/// or the embedding call fails; nothing is placed then and the rows wait for
/// the next batch. A placement that fails is logged and counted as failed,
/// and the batch goes on without that user's later rows, so a user's facts
/// are always placed oldest first and a group's first fact is its earliest.
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
    let mut held_back = std::collections::HashSet::new();
    for (fact, vector) in facts.iter().zip(&vectors) {
        if held_back.contains(&fact.user_id) {
            tally.failed += 1;
            continue;
        }
        match repo.place(fact, vector, cfg.threshold).await {
            Ok(Placement::Joined) => tally.joined += 1,
            Ok(Placement::Alone) => tally.alone += 1,
            Ok(Placement::Skipped) => tally.skipped += 1,
            Err(e) => {
                tracing::warn!(fact_id = %fact.id, "fact groups: placing a fact failed: {e}");
                tally.failed += 1;
                held_back.insert(fact.user_id);
            }
        }
    }
    Ok(tally)
}

/// One tick's work: batches back to back while each comes back full and
/// placed something, so a backlog drains without waiting for the tick. A
/// batch with a failed placement ends the tick, since its rows are still
/// unplaced and the next batch would embed them again for nothing; so does
/// a batch that placed nothing, whose rows another instance is placing.
/// Returns the summed tally.
pub(crate) async fn drain(state: &AppState) -> PlaceTally {
    let batch = state.config.fact_grouping.batch;
    let mut total = PlaceTally::default();
    loop {
        match place_batch(state).await {
            Ok(t) => {
                let placed = t.joined + t.alone;
                let go_on = t.read as i64 == batch && t.failed == 0 && placed > 0;
                total.read += t.read;
                total.joined += t.joined;
                total.alone += t.alone;
                total.skipped += t.skipped;
                total.failed += t.failed;
                if !go_on {
                    return total;
                }
            }
            Err(e) => {
                tracing::warn!("fact groups: {e}");
                return total;
            }
        }
    }
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
        let total = drain(&state).await;
        if total.read > 0 {
            tracing::info!(
                read = total.read,
                joined = total.joined,
                alone = total.alone,
                skipped = total.skipped,
                failed = total.failed,
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
                skipped: 0,
                failed: 0
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

    /// A mock embedder answering every call with one `unit(0)` vector.
    async fn one_vector_embedder(calls: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "embedding": unit(0.0), "index": 0 } ]
            })))
            .expect(calls)
            .mount(&server)
            .await;
        server
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn drain_runs_full_batches_back_to_back(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed(&pool, user, "用户喜欢爬山", h(1)).await;
        let b = seed(&pool, user, "用户很喜欢爬山", h(2)).await;
        let server = one_vector_embedder(2).await;
        let mut state = state_with(pool.clone(), &server);
        state.config.fact_grouping.batch = 1;

        let total = drain(&state).await;
        assert_eq!((total.read, total.joined, total.alone), (2, 1, 1));
        assert_eq!(placement(&pool, a).await.0, placement(&pool, b).await.0);
    }

    /// The database takes reads but refuses writes (a read-only failover, a
    /// full disk): every placement fails and the rows stay unplaced. The drain
    /// must stop rather than embed the same full batch again and again.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn drain_stops_when_placements_fail(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed(&pool, user, "用户喜欢爬山", h(1)).await;
        seed(&pool, user, "用户很喜欢爬山", h(2)).await;
        sqlx::raw_sql(
            "CREATE FUNCTION public.refuse_fact_update() RETURNS trigger \
               LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'read-only'; END $$; \
             CREATE TRIGGER refuse BEFORE UPDATE ON engine.human_facts \
               FOR EACH ROW EXECUTE FUNCTION public.refuse_fact_update();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let server = one_vector_embedder(1).await;
        let mut state = state_with(pool.clone(), &server);
        state.config.fact_grouping.batch = 1;

        let total = tokio::time::timeout(std::time::Duration::from_secs(5), drain(&state))
            .await
            .expect("the drain stops after a batch whose placements all failed");
        assert_eq!((total.read, total.failed), (1, 1));
        assert!(!placement(&pool, a).await.1, "still unplaced");
    }

    /// A batch that placed nothing (here every write is silently dropped, so
    /// every row comes back skipped and stays in the queue) ends the drain:
    /// another batch would embed the same rows again.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn drain_stops_after_a_batch_that_placed_nothing(pool: PgPool) {
        let user = Uuid::new_v4();
        seed(&pool, user, "用户喜欢爬山", h(1)).await;
        seed(&pool, user, "用户很喜欢爬山", h(2)).await;
        sqlx::raw_sql(
            "CREATE FUNCTION public.drop_fact_update() RETURNS trigger \
               LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$; \
             CREATE TRIGGER drop_update BEFORE UPDATE ON engine.human_facts \
               FOR EACH ROW EXECUTE FUNCTION public.drop_fact_update();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let server = one_vector_embedder(1).await;
        let mut state = state_with(pool.clone(), &server);
        state.config.fact_grouping.batch = 1;

        let total = tokio::time::timeout(std::time::Duration::from_secs(5), drain(&state))
            .await
            .expect("the drain stops after a batch that placed nothing");
        assert_eq!((total.read, total.skipped), (1, 1));
    }

    /// A failed placement holds back the same user's later rows in the batch,
    /// so an older fact is never placed after a younger one of the same user;
    /// other users' rows go on.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_placement_holds_back_the_users_later_rows(pool: PgPool) {
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let a = seed(&pool, user, "用户喜欢爬山", h(1)).await;
        let b = seed(&pool, user, "用户很喜欢爬山", h(2)).await;
        let c = seed(&pool, other, "用户养了一只猫", h(3)).await;
        sqlx::raw_sql(&format!(
            "CREATE FUNCTION public.refuse_one() RETURNS trigger LANGUAGE plpgsql AS $$ \
               BEGIN IF OLD.id = '{a}' THEN RAISE EXCEPTION 'refused'; END IF; \
               RETURN NEW; END $$; \
             CREATE TRIGGER refuse_one BEFORE UPDATE ON engine.human_facts \
               FOR EACH ROW EXECUTE FUNCTION public.refuse_one();"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let server = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "embedding": unit(0.0), "index": 0 },
                    { "embedding": unit(5.0), "index": 1 },
                    { "embedding": unit(90.0), "index": 2 }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let state = state_with(pool.clone(), &server);

        let tally = place_batch(&state).await.unwrap();
        assert_eq!((tally.read, tally.failed, tally.alone), (3, 2, 1));
        assert!(!placement(&pool, a).await.1);
        assert!(
            !placement(&pool, b).await.1,
            "held back behind the failed row"
        );
        assert!(placement(&pool, c).await.1, "another user goes on");
    }
}
