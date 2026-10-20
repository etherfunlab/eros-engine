// SPDX-License-Identifier: AGPL-3.0-only
//! The ask-for-space choice on the pipeline side: when a judged turn makes her
//! ask, her generated request, and the slowdown after an accepted separation.
//! The answer, its costs and the ignore path live in the resolution route and
//! the turn entry.
//!
//! Spec: docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md
//! §3.1 (trigger), §3.2 (the ask), §3.5 (the slowdown).

use chrono::{DateTime, Utc};
use uuid::Uuid;

use eros_engine_core::events::EventKind;
use eros_engine_core::persona::CompanionPersona;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::events::EventRepo;
use eros_engine_store::life_choices::LifeChoiceRepo;

use crate::byoc::LoadedInstance;
use crate::error::AppError;
use crate::holiday::UserLocale;
use crate::pipeline::handlers::recall_query_text;
use crate::pipeline::proactive::{greet, Greeting, LifeChoiceCue, Occasion};
use crate::prompt::NowContext;
use crate::state::AppState;

/// The ask needs the tier lines this far apart, chemistry above bond.
const MIN_TIER_GAP: i16 = 2;
/// Judged turns in a row that must all be below [`LOW_PATIENCE`].
const STREAK: i64 = 3;
/// A rule threshold of the engine, not a judge band.
const LOW_PATIENCE: f64 = 0.5;
/// Positive line growth while the slowdown lasts.
const SLOWED_GROWTH: f64 = 0.5;

/// The scale on every positive line delta of a commit in `session_id` at
/// `now`: [`SLOWED_GROWTH`] in the 72 hours after a separation she asked for
/// ended, 1.0 otherwise. A read that fails commits at full speed.
pub(crate) async fn growth_scale_for(
    state: &AppState,
    session_id: Uuid,
    now: DateTime<Utc>,
) -> f64 {
    match (LifeChoiceRepo { pool: &state.pool })
        .growth_slowed(session_id, now)
        .await
    {
        Ok(true) => SLOWED_GROWTH,
        Ok(false) => 1.0,
        Err(e) => {
            tracing::warn!(%session_id, "growth slowdown read failed: {e}");
            1.0
        }
    }
}

/// After a judged turn's affinity committed: ask for space if the
/// relationship has worn thin. Every failure is logged and swallowed; the
/// turn is already answered and its affinity written.
pub(crate) async fn maybe_ask_for_space(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
) {
    if let Err(e) = try_ask(state, session_id, user_id, instance_id, persona).await {
        tracing::warn!(%session_id, "ask for space failed: {e}");
    }
}

/// The trigger (spec §3.1), then her ask (§3.2): her words first, then the
/// choice row beside them. A reply that is blank, strips to nothing or fails
/// writes neither, so a later qualifying turn asks again.
async fn try_ask(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    persona: &CompanionPersona,
) -> Result<(), AppError> {
    let now = Utc::now();
    let affinity_repo = AffinityRepo { pool: &state.pool };
    let Some(mut affinity) = affinity_repo.load(session_id).await? else {
        return Ok(());
    };
    affinity.apply_time_decay();
    affinity.refresh_endpoints(&state.config.affinity_tuning);
    let tier_gap = i16::from(affinity.chem_tier()) - i16::from(affinity.bond_tier());
    if tier_gap < MIN_TIER_GAP {
        return Ok(());
    }

    let choices = LifeChoiceRepo { pool: &state.pool };
    if choices.pending(session_id, now).await?.is_some() {
        return Ok(());
    }
    let apart = EventRepo { pool: &state.pool }
        .current(instance_id, None, now)
        .await?
        .foreground
        .is_some_and(|e| e.kind == EventKind::TakingSpace);
    if apart {
        return Ok(());
    }

    // Three judged turns, this one included, all after the last ask.
    let since = choices.latest_asked_at(session_id).await?;
    let patiences = affinity_repo
        .last_judged_patiences(session_id, since, STREAK)
        .await?;
    if patiences.len() < STREAK as usize || patiences.iter().any(|p| *p >= LOW_PATIENCE) {
        return Ok(());
    }

    // 1:1 text only: the image-edit route also runs the affinity pass, and it
    // serves a voice session too.
    let chat_repo = ChatRepo { pool: &state.pool };
    match chat_repo.get_session(session_id).await? {
        Some(s) if s.channel == "text" => {}
        _ => return Ok(()),
    }

    // A genome instance only: the resolution route refuses BYOC, so an ask to
    // a BYOC session could never be answered.
    if !matches!(
        crate::byoc::load_instance(state, instance_id).await?,
        Some(LoadedInstance::Genome(_))
    ) {
        return Ok(());
    }

    // The turn's own request fields are not carried here; the locale and the
    // tier are read back from the user row that recorded them.
    let latest = chat_repo.latest_user_message(session_id).await?;
    let raw = |key: &str| {
        latest
            .as_ref()
            .and_then(|m| m.metadata.as_ref())
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
    };
    let (timezone, country, region) = (
        raw("user_timezone"),
        raw("user_country"),
        raw("user_region"),
    );
    let mut metadata = serde_json::Map::new();
    crate::holiday::record_raw(&mut metadata, timezone, country, region);
    let locale = UserLocale::resolve(timezone, country, region);
    let tier = raw("tier").map(String::from);
    let recall_query = latest.as_ref().map(recall_query_text).unwrap_or_default();

    let ask = greet(
        state,
        Greeting {
            session_id,
            user_id,
            instance_id,
            persona,
            now: &NowContext::for_user(&locale, now),
            occasion: Occasion::LifeChoice(LifeChoiceCue::Ask { tier_gap }),
            recall_query,
            since: now,
            tier,
            prompt_traits: Vec::new(),
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            audit: None,
            metadata,
        },
    )
    .await?;
    if ask.is_some() {
        tracing::info!(%session_id, tier_gap, "she asked for space");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use eros_engine_core::events::EventKind;
    use eros_engine_core::types::ActionType;
    use eros_engine_store::events::{EventRepo, NewEvent};
    use eros_engine_store::life_choices::LifeChoiceRepo;
    use eros_engine_store::persona::PersonaRepo;
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::pipeline::post_process::run_affinity_turn;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        seed_byoc_instance_with_key, seed_genome, seed_instance, seed_session,
    };

    /// Her ask, the cue that is the request's last `user` turn.
    const ASK_CUE: &str = "（这段关系最近让你很累：心动还在，但交情和耐心都被磨薄了。你决定郑重地告诉对方：你想先分开一阵子，希望对方给你一个答复。）";
    const ASK_TEXT: &str = "我想先分开一阵子，你能给我一个答复吗。";
    const SEEN: &str = "今天过得怎么样呀";

    /// The judge's verdict: nothing moved, patience at the bottom level.
    const LOW_PATIENCE: &str = r#"{"warmth":2,"trust":{"grade":0,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":1,"reason":"累了"}"#;
    /// Nothing moved, patience at the top level.
    const PATIENT: &str = r#"{"warmth":2,"trust":{"grade":0,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":3,"reason":"耐心还很足"}"#;
    /// A verdict that moves trust up a clear step, patience at the usual level.
    const TRUST_UP: &str = r#"{"warmth":2,"trust":{"grade":3,"direction":"up"},"intimacy":{"grade":0,"direction":"up"},"intrigue":{"grade":0,"direction":"up"},"tension":{"grade":0,"direction":"up"},"patience":2,"reason":"暖了"}"#;

    fn with_models(mut state: AppState, mock_uri: &str) -> AppState {
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"companion\"\n\
                 [tasks.affinity_evaluation]\nmodel = \"judge\"\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    /// A mock that judges every turn with `verdict` and, for the persona's
    /// own call ("you are a companion"), answers `companion`: `Some(text)` as
    /// her reply, `None` as a provider failure.
    async fn mock_llm(verdict: &str, companion: Option<&str>) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("你就是对话里的这个角色"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-judge",
                "model": "served/judge",
                "choices": [{"message": {"content": verdict}}],
            })))
            .mount(&mock)
            .await;
        let reply = match companion {
            Some(text) => ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-ask",
                "model": "served/companion",
                "choices": [{"message": {"content": text}}],
            })),
            None => ResponseTemplate::new(500).set_body_string("provider down"),
        };
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("you are a companion"))
            .respond_with(reply)
            .mount(&mock)
            .await;
        mock
    }

    /// Requests that reached her, as JSON bodies.
    async fn companion_calls(mock: &MockServer) -> Vec<serde_json::Value> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("you are a companion"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    struct Scene {
        pool: PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        persona: CompanionPersona,
    }

    /// A text session whose chemistry is far above its bond: bond 0.0 (tier 1),
    /// chemistry 0.70 (tier 4), a gap of 3.
    async fn scene(pool: &PgPool) -> Scene {
        scene_with_lines(pool, 0.0, 0.7).await
    }

    async fn scene_with_lines(pool: &PgPool, bond: f64, chem: f64) -> Scene {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(pool, "Aria").await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = session_with_lines(pool, user_id, instance_id, bond, chem).await;
        let persona = PersonaRepo { pool }
            .load_companion(instance_id)
            .await
            .unwrap()
            .expect("the persona");
        Scene {
            pool: pool.clone(),
            user_id,
            instance_id,
            session_id,
            persona,
        }
    }

    async fn lopsided_session(pool: &PgPool, user_id: Uuid, instance_id: Uuid) -> Uuid {
        session_with_lines(pool, user_id, instance_id, 0.0, 0.7).await
    }

    /// A text session whose bond line sits at `bond` and chemistry line at `chem`.
    async fn session_with_lines(
        pool: &PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        bond: f64,
        chem: f64,
    ) -> Uuid {
        let session_id = seed_session(pool, user_id, instance_id).await;
        eros_engine_store::affinity::AffinityRepo { pool }
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE engine.companion_affinity \
                SET trust = $2, intrigue = $2, intimacy = $3, tension = $3, updated_at = now() \
              WHERE session_id = $1",
        )
        .bind(session_id)
        .bind(bond)
        .bind(chem)
        .execute(pool)
        .await
        .unwrap();
        session_id
    }

    impl Scene {
        /// One 1:1 text turn in `session`, through the whole affinity pass.
        async fn turn_in(&self, state: &AppState, session_id: Uuid, text: &str) {
            let umid: Uuid = sqlx::query_scalar(
                "INSERT INTO engine.chat_messages (session_id, role, content) \
                 VALUES ($1, 'user', $2) RETURNING id",
            )
            .bind(session_id)
            .bind(text)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            run_affinity_turn(
                state,
                session_id,
                self.user_id,
                self.instance_id,
                &self.persona,
                ActionType::ReplyText,
                text,
                "嗯，我在听。",
                Default::default(),
                Default::default(),
                Some(umid),
                None,
                false,
                None,
            )
            .await;
        }

        async fn turn(&self, state: &AppState, text: &str) {
            self.turn_in(state, self.session_id, text).await;
        }

        /// (status, tier_gap, message_id) of every choice in the session.
        async fn choices(&self) -> Vec<(String, i16, Option<Uuid>)> {
            self.choices_in(self.session_id).await
        }

        async fn choices_in(&self, session_id: Uuid) -> Vec<(String, i16, Option<Uuid>)> {
            sqlx::query_as(
                "SELECT status, tier_gap, message_id FROM engine.life_choices \
                  WHERE session_id = $1 ORDER BY asked_at",
            )
            .bind(session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// Every assistant row of the session written as a life choice.
        async fn ask_rows(&self) -> Vec<(Uuid, String, Option<String>)> {
            sqlx::query_as(
                "SELECT id, content, assistant_action_type FROM engine.chat_messages \
                  WHERE session_id = $1 AND role = 'assistant' \
                    AND metadata->>'proactive' = 'life_choice'",
            )
            .bind(self.session_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// A choice asked `ago` ago, in `status`.
        async fn seed_choice(&self, status: &str, ago: Duration) {
            sqlx::query(
                "INSERT INTO engine.life_choices (session_id, kind, status, tier_gap, asked_at, \
                                                  resolved_at) \
                 VALUES ($1, 'ask_for_space', $2, 3, now() - make_interval(secs => $3), \
                         CASE WHEN $2 = 'pending' THEN NULL \
                              ELSE now() - make_interval(secs => $3) + interval '1 minute' END)",
            )
            .bind(self.session_id)
            .bind(status)
            .bind(ago.num_seconds() as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn three_low_patience_turns_with_a_gap_ask_for_space(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(
            s.choices().await.is_empty(),
            "two judged turns are not three"
        );
        assert!(companion_calls(&mock).await.is_empty(), "and cost nothing");

        s.turn(&state, SEEN).await;
        let choices = s.choices().await;
        assert_eq!(choices.len(), 1, "{choices:?}");
        let (status, tier_gap, message_id) = choices[0].clone();
        assert_eq!(status, "pending");
        let (bond, chem): (i16, i16) = sqlx::query_as(
            "SELECT bond_tier, chem_tier FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tier_gap, chem - bond, "the gap at ask time");
        assert_eq!(tier_gap, 3);

        // Her words: an ordinary assistant row, proactive, written as the
        // choice's message.
        let rows = s.ask_rows().await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(message_id, Some(rows[0].0));
        assert_eq!(rows[0].1, ASK_TEXT);
        assert_eq!(rows[0].2.as_deref(), Some("proactive"));

        // The stage cue is the request's final user turn.
        let calls = companion_calls(&mock).await;
        assert_eq!(calls.len(), 1);
        let last = calls[0]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last["role"], json!("user"));
        assert_eq!(last["content"], json!(ASK_CUE));

        // The turns' own affinity events are untouched: three judged turns.
        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.event_type = 'message'",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 3);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_unjudged_turn_neither_counts_nor_breaks(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, "hi").await; // too short to judge
        s.turn(&state, SEEN).await;
        assert!(
            s.choices().await.is_empty(),
            "the skipped turn is not the third low turn"
        );
        let skipped: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.context->>'eval_skip_reason' = 'short_user_msg'",
        )
        .bind(s.session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(skipped, 1);

        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "low, skipped, low, low fires");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn turns_before_the_last_ask_do_not_count(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        // The ask they led to came and went: both turns are older than it.
        sqlx::query(
            "UPDATE engine.companion_affinity_events \
                SET created_at = created_at - interval '2 hours'",
        )
        .execute(&pool)
        .await
        .unwrap();
        s.seed_choice("declined", Duration::hours(1)).await;

        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "one fresh turn is not a streak");
        assert!(companion_calls(&mock).await.is_empty());

        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "two fresh turns are not either");
        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 2, "three fresh turns ask again");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_ask_while_one_is_pending_or_separated(pool: PgPool) {
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        // A pending, un-expired choice holds the next ask back, however low
        // her patience runs; answering it lets the same streak through.
        let s = scene(&pool).await;
        s.seed_choice("pending", Duration::hours(1)).await;
        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(
            s.choices().await.len(),
            1,
            "no second ask beside a pending one"
        );
        assert!(
            companion_calls(&mock).await.is_empty(),
            "and no generation spent"
        );
        sqlx::query("UPDATE engine.life_choices SET status = 'ignored' WHERE session_id = $1")
            .bind(s.session_id)
            .execute(&pool)
            .await
            .unwrap();
        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 2);

        // A pending choice that lapsed does not hold anything back.
        let lapsed = scene(&pool).await;
        lapsed.seed_choice("pending", Duration::hours(25)).await;
        for _ in 0..3 {
            lapsed.turn(&state, SEEN).await;
        }
        assert_eq!(
            lapsed.choices().await.len(),
            2,
            "the lapsed one and the new ask"
        );

        // While she is separated from the user, she does not ask.
        let apart = scene(&pool).await;
        let now = Utc::now();
        EventRepo { pool: &pool }
            .insert_preempting(&NewEvent {
                instance_id: apart.instance_id,
                session_id: apart.session_id,
                event_key: "apart",
                kind: EventKind::TakingSpace,
                started_at: now - Duration::hours(1),
                ends_at: now + Duration::hours(10),
            })
            .await
            .unwrap();
        for _ in 0..3 {
            apart.turn(&state, SEEN).await;
        }
        assert!(apart.choices().await.is_empty(), "a separation under way");
        sqlx::query("UPDATE engine.persona_events SET ends_at = now() - interval '1 minute'")
            .execute(&pool)
            .await
            .unwrap();
        apart.turn(&state, SEEN).await;
        assert_eq!(apart.choices().await.len(), 1, "back, the same streak asks");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_ask_outside_a_text_session(pool: PgPool) {
        let s = scene(&pool).await;
        sqlx::query("UPDATE engine.chat_sessions SET channel = 'voice' WHERE id = $1")
            .bind(s.session_id)
            .execute(&pool)
            .await
            .unwrap();
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.choices().await.is_empty());
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// A BYOC session meeting every other condition never asks: the resolution
    /// route refuses BYOC, so the ask could only be ignored or left to lapse.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_byoc_session_never_asks(pool: PgPool) {
        let mut s = scene(&pool).await;
        let (instance_id, session_id) = {
            let (instance_id, _) = seed_byoc_instance_with_key(&pool, s.user_id).await;
            let session_id = session_with_lines(&pool, s.user_id, instance_id, 0.0, 0.7).await;
            (instance_id, session_id)
        };
        // The in-memory card a BYOC turn runs on has no genome behind it.
        s.persona.instance_id = instance_id;
        s.persona.instance.id = instance_id;
        s.persona.instance.genome_id = None;
        s.persona.genome.id = None;
        s.instance_id = instance_id;
        s.session_id = session_id;

        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        for _ in 0..4 {
            s.turn(&state, SEEN).await;
        }
        assert!(s.choices().await.is_empty());
        assert!(s.ask_rows().await.is_empty());
        assert!(companion_calls(&mock).await.is_empty());
    }

    /// The ask needs chemistry two tiers or more above bond; a smaller gap, or
    /// a bond ahead of chemistry, never asks, however many turns run.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_ask_without_a_wide_enough_gap(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        // (bond line, chemistry line, tiers): equal; chemistry one tier ahead
        // (bond tier 3, chemistry tier 4); bond ahead; then a gap of exactly 2
        // (bond tier 2, chemistry tier 4), which asks.
        for (bond, chem, asks) in [
            (0.45, 0.45, false),
            (0.45, 0.70, false),
            (0.70, 0.0, false),
            (0.20, 0.70, true),
        ] {
            let session = session_with_lines(&pool, s.user_id, s.instance_id, bond, chem).await;
            for _ in 0..4 {
                s.turn_in(&state, session, SEEN).await;
            }
            let asked = s.choices_in(session).await;
            assert_eq!(!asked.is_empty(), asks, "bond {bond}, chemistry {chem}");
            if asks {
                assert_eq!(asked[0].1, 2, "the gap stored is the gap at ask time");
            }
        }
    }

    /// A judged turn that is not low (patience level 3 at the lowest bond
    /// still reads above 0.5) is in the newest three, so it holds the ask back
    /// until three low ones have come after it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_turn_with_patience_to_spare_breaks_the_streak(pool: PgPool) {
        // Bond 0.14 is tier 1, where a level-3 verdict reads 0.56.
        let s = scene_with_lines(&pool, 0.14, 0.7).await;
        let low = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let high = mock_llm(PATIENT, Some(ASK_TEXT)).await;
        let low_state = with_models(test_state(pool.clone()), &low.uri());
        let high_state = with_models(test_state(pool.clone()), &high.uri());

        s.turn(&low_state, SEEN).await;
        s.turn(&high_state, SEEN).await;
        s.turn(&low_state, SEEN).await;
        s.turn(&low_state, SEEN).await;
        assert!(s.choices().await.is_empty(), "low, high, low, low");
        s.turn(&low_state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "high, low, low, low asks");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_blank_ask_writes_nothing(pool: PgPool) {
        let s = scene(&pool).await;
        // A reply the model leaves blank, an empty one, and one the provider
        // fails.
        let blank = mock_llm(LOW_PATIENCE, Some("  ")).await;
        let empty = mock_llm(LOW_PATIENCE, Some("")).await;
        let failing = mock_llm(LOW_PATIENCE, None).await;
        let spoken = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let blank_state = with_models(test_state(pool.clone()), &blank.uri());
        let empty_state = with_models(test_state(pool.clone()), &empty.uri());
        let failing_state = with_models(test_state(pool.clone()), &failing.uri());
        let spoken_state = with_models(test_state(pool.clone()), &spoken.uri());

        s.turn(&blank_state, SEEN).await;
        s.turn(&blank_state, SEEN).await;
        s.turn(&blank_state, SEEN).await;
        assert_eq!(companion_calls(&blank).await.len(), 1, "she was asked");
        assert!(s.choices().await.is_empty(), "blank: no choice row");
        assert!(s.ask_rows().await.is_empty(), "blank: no message");

        s.turn(&empty_state, SEEN).await;
        assert_eq!(companion_calls(&empty).await.len(), 1);
        assert!(s.choices().await.is_empty(), "empty: no choice row");
        assert!(s.ask_rows().await.is_empty(), "empty: no message");

        s.turn(&failing_state, SEEN).await;
        assert_eq!(companion_calls(&failing).await.len(), 1);
        assert!(s.choices().await.is_empty(), "failed: no choice row");
        assert!(s.ask_rows().await.is_empty(), "failed: no message");

        // Nothing was consumed: the next qualifying turn asks again.
        s.turn(&spoken_state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1);
        assert_eq!(s.ask_rows().await.len(), 1);
    }

    /// An accepted choice whose separation ended at `ends`, in `session`.
    async fn accepted_separation(s: &Scene, session_id: Uuid, ends: DateTime<Utc>) {
        let repo = LifeChoiceRepo { pool: &s.pool };
        let message = eros_engine_store::chat::ProactiveInsert {
            id: Uuid::new_v4(),
            content: "我想先分开一阵子".into(),
            generation_id: None,
            metadata: json!({"proactive": "life_choice"}),
            llm_attempts: None,
            gateway_errors: None,
        };
        let choice = repo
            .insert_ask(session_id, 3, &message)
            .await
            .unwrap()
            .expect("written");
        repo.resolve(choice.id, "accepted", choice.asked_at)
            .await
            .unwrap()
            .expect("resolved");
        let event = EventRepo { pool: &s.pool }
            .insert_preempting(&NewEvent {
                instance_id: s.instance_id,
                session_id,
                event_key: &format!("apart-{session_id}"),
                kind: EventKind::TakingSpace,
                started_at: ends - Duration::hours(18),
                ends_at: ends,
            })
            .await
            .unwrap();
        repo.set_event(choice.id, event.id).await.unwrap();
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn growth_scale_is_half_inside_the_window_in_the_choice_session_only(pool: PgPool) {
        let s = scene(&pool).await;
        let sibling = seed_session(&pool, s.user_id, s.instance_id).await;
        let state = test_state(pool.clone());
        let ends = Utc.with_ymd_and_hms(2026, 3, 1, 12, 0, 0).unwrap();
        let micro = Duration::microseconds(1);
        accepted_separation(&s, s.session_id, ends).await;

        let scale = |session, at| {
            let state = state.clone();
            async move { growth_scale_for(&state, session, at).await }
        };
        assert_eq!(scale(s.session_id, ends - micro).await, 1.0, "still apart");
        assert_eq!(
            scale(s.session_id, ends).await,
            0.5,
            "back: the window opens"
        );
        assert_eq!(
            scale(s.session_id, ends + Duration::hours(72) - micro).await,
            0.5
        );
        assert_eq!(
            scale(s.session_id, ends + Duration::hours(72)).await,
            1.0,
            "72 hours on, full speed"
        );
        // The cost is that relationship's, not the instance's.
        assert_eq!(scale(sibling, ends).await, 1.0);
        assert_eq!(scale(sibling, ends + Duration::hours(1)).await, 1.0);
    }

    /// The window is wired into the turn's commit: the same verdict lands
    /// half as much inside it, and only in the session that asked.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn growth_inside_the_window_lands_halved_through_the_turn(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(TRUST_UP, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        let twin = lopsided_session(&pool, s.user_id, s.instance_id).await;
        accepted_separation(&s, s.session_id, Utc::now() - Duration::hours(1)).await;

        s.turn_in(&state, s.session_id, SEEN).await;
        s.turn_in(&state, twin, SEEN).await;

        let trust = |session: Uuid| {
            let pool = pool.clone();
            async move {
                let (eff, ctx): (serde_json::Value, serde_json::Value) = sqlx::query_as(
                    "SELECT e.effective_deltas, e.context \
                       FROM engine.companion_affinity_events e \
                       JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                      WHERE a.session_id = $1 AND e.event_type = 'message'",
                )
                .bind(session)
                .fetch_one(&pool)
                .await
                .unwrap();
                (eff["trust"].as_f64().unwrap(), ctx)
            }
        };
        let (slowed, slowed_ctx) = trust(s.session_id).await;
        let (full, full_ctx) = trust(twin).await;
        assert!(full > 0.0, "the verdict moves trust: {full}");
        assert!((slowed - full / 2.0).abs() < 1e-9, "{slowed} vs {full}");
        assert_eq!(slowed_ctx["growth_scale"], json!(0.5));
        assert!(full_ctx.get("growth_scale").is_none(), "{full_ctx}");
    }
}
