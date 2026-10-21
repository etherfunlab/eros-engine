// SPDX-License-Identifier: AGPL-3.0-only
//! The ask-for-space choice on the pipeline side: when a judged turn makes her
//! ask, her generated request, and the slowdown after an accepted separation.
//! The answer, its costs and the ignore path live in the resolution route and
//! the turn entry.
//!
//! Spec: docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md
//! §3.1 (trigger), §3.2 (the ask), §3.5 (the slowdown).

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use eros_engine_core::events::EventKind;
use eros_engine_core::persona::CompanionPersona;
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::events::{EventRepo, NewEvent};
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

/// How long the falling-out lasts, from the message that caused it.
const FELL_OUT_HOURS: i64 = 1;

/// The ignore path (spec §3.4c): an ordinary text turn that arrives while she
/// waits for an answer is the answer. The choice becomes `ignored` and a
/// `fell_out` event starts at the user's row, in one transaction, so the turn
/// that ignored her reads the falling-out in its events. An incident under
/// way ends there, a background keeps running. No tier changes: the judge
/// rates the turn itself.
///
/// The clock is the user row's `sent_at`: whether the message arrived while
/// the choice was pending is a fact about the message, so a turn that waited
/// in the queue past the 24 hours still answers it, and a re-drive reads the
/// same. A message sent before her ask landed answers nothing: the ask can be
/// written while the user's next message is already in flight.
///
/// Nothing happens (and nothing is written) when no choice is pending then —
/// a lapsed one converts nothing (§3.4d) — when a separation she agreed to is
/// under way at the message, or when the resolution route answered first. The
/// caller leaves out action turns: a give, a tip or a kiss does not answer
/// her. `Ok(true)` when the choice was converted.
pub(crate) async fn convert_ignored_ask(
    state: &AppState,
    session_id: Uuid,
    instance_id: Uuid,
    user_message_id: Uuid,
) -> Result<bool, AppError> {
    let Some(sent_at) = (ChatRepo { pool: &state.pool })
        .message_by_id_in_session(session_id, user_message_id)
        .await?
        .map(|m| m.sent_at)
    else {
        return Ok(false);
    };
    let Some(choice) = (LifeChoiceRepo { pool: &state.pool })
        .pending(session_id, sent_at)
        .await?
        .filter(|c| c.asked_at <= sent_at)
    else {
        return Ok(false);
    };
    // A message during an agreed separation only ghosts: converting would cap
    // the separation at the message.
    if separated(&state.pool, instance_id, Some(user_message_id), sent_at).await? {
        return Ok(false);
    }

    let mut tx = state.pool.begin().await?;
    // First, the race guard: the resolution route may have answered between
    // the read and here, and the loser writes nothing.
    let Some(choice) =
        LifeChoiceRepo::resolve_in_tx(&mut tx, choice.id, "ignored", sent_at).await?
    else {
        return Ok(false);
    };
    EventRepo::insert_preempting_in_tx(
        &mut tx,
        &NewEvent {
            instance_id,
            session_id: choice.session_id,
            event_key: &format!("life_choice:{}:fell_out", choice.id),
            kind: EventKind::FellOut,
            started_at: sent_at,
            ends_at: sent_at + Duration::hours(FELL_OUT_HOURS),
        },
    )
    .await?;
    tx.commit().await?;
    tracing::info!(%session_id, choice_id = %choice.id, "her ask went unanswered: fell out");
    Ok(true)
}

/// Whether a separation she agreed to is under way for the instance: at
/// `at_message`'s `sent_at`, or at `now` without one.
async fn separated(
    pool: &PgPool,
    instance_id: Uuid,
    at_message: Option<Uuid>,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    Ok(EventRepo { pool }
        .current(instance_id, at_message, now)
        .await?
        .foreground
        .is_some_and(|e| e.kind == EventKind::TakingSpace))
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

    // She has one life: one pending ask at a time across the instance's
    // sessions, and none while she is apart from the user.
    let choices = LifeChoiceRepo { pool: &state.pool };
    if choices.pending_for_instance(instance_id, now).await? {
        return Ok(());
    }
    if separated(&state.pool, instance_id, None, now).await? {
        return Ok(());
    }

    // Three judged turns, this one included, all after the last episode: the
    // ask, its answer and any separation it started.
    let since = choices.streak_anchor(session_id).await?;
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
        /// No ignore conversion runs: as far as a pending ask is concerned
        /// this is an action turn, which does not answer her.
        async fn turn_in(&self, state: &AppState, session_id: Uuid, text: &str) {
            let umid = self.user_row(session_id, text).await;
            self.affinity_pass(state, session_id, text, umid).await;
        }

        /// The user's row of a turn, as the stream persists it.
        async fn user_row(&self, session_id: Uuid, text: &str) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO engine.chat_messages (session_id, role, content) \
                 VALUES ($1, 'user', $2) RETURNING id",
            )
            .bind(session_id)
            .bind(text)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// An ordinary text turn in the scene's own session: the ignore
        /// conversion first, as the stream runs it, then the affinity pass.
        async fn converting_turn(&self, state: &AppState, text: &str) {
            let umid = self.user_row(self.session_id, text).await;
            convert_ignored_ask(state, self.session_id, self.instance_id, umid)
                .await
                .unwrap();
            self.affinity_pass(state, self.session_id, text, umid).await;
        }

        async fn affinity_pass(&self, state: &AppState, session_id: Uuid, text: &str, umid: Uuid) {
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

    // ---- The ignore conversion (spec §3.4c, §3.4d) ----

    /// The turn's judge verdict: an ordinary reply.
    const TURN_VERDICT: &str = r#"{"action": "reply_text", "inner_state": ""}"#;
    /// The falling-out as the judge and the reply read it.
    const FELL_OUT_LABEL: &str = "为分开的事闹得不愉快";

    /// A whole turn's providers: the judge ("pde/judge") and her reply
    /// ("deepseek/x"). Returns the state wired to them and the mock.
    async fn turn_env(pool: &PgPool) -> (Arc<AppState>, MockServer) {
        let mock = MockServer::start().await;
        let judge = json!({
            "id": "gj", "model": "pde/judge",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": TURN_VERDICT}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("pde/judge"))
            .respond_with(crate::pipeline::reasoning_call::test_support::sse_completion(judge))
            .mount(&mock)
            .await;
        let reply = "data: {\"choices\":[{\"delta\":{\"content\":\"嗯\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(reply, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        let mut state = test_state(pool.clone());
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel = \"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel = \"pde/judge\"\nfilter_prompt = \"Decide.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        (Arc::new(state), mock)
    }

    /// A request body the judge or the reply received, by the model it names.
    async fn request_to(mock: &MockServer, model: &str) -> Vec<String> {
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .filter(|b| b.contains(model))
            .collect()
    }

    /// A time in the recent past, whole microseconds, so what Postgres stores
    /// is what the test compares.
    fn recent(minutes_ago: i64) -> DateTime<Utc> {
        use chrono::SubsecRound;
        (Utc::now() - Duration::minutes(minutes_ago)).trunc_subsecs(6)
    }

    impl Scene {
        /// A choice asked at `asked_at`, still pending; returns its id.
        async fn pending_at(&self, asked_at: DateTime<Utc>) -> Uuid {
            self.pending_in(self.session_id, asked_at).await
        }

        /// [`Self::pending_at`] in any session of the instance.
        async fn pending_in(&self, session_id: Uuid, asked_at: DateTime<Utc>) -> Uuid {
            sqlx::query_scalar(
                "INSERT INTO engine.life_choices (session_id, kind, tier_gap, asked_at) \
                 VALUES ($1, 'ask_for_space', 3, $2) RETURNING id",
            )
            .bind(session_id)
            .bind(asked_at)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// One 1:1 turn through the whole stream, the user's row sent at
        /// `sent_at` and carrying `action`. Returns the user row's id.
        async fn stream_turn(
            &self,
            state: &Arc<AppState>,
            sent_at: DateTime<Utc>,
            action: Option<eros_engine_core::types::UserAction>,
        ) -> Uuid {
            self.stream_turn_in(self.session_id, state, sent_at, action)
                .await
        }

        /// [`Self::stream_turn`] in any session of the instance.
        async fn stream_turn_in(
            &self,
            session_id: Uuid,
            state: &Arc<AppState>,
            sent_at: DateTime<Utc>,
            action: Option<eros_engine_core::types::UserAction>,
        ) -> Uuid {
            use eros_engine_store::chat::{ChatRepo, UpsertUserOutcome};
            use futures_util::StreamExt;

            let metadata = action
                .as_ref()
                .map(|a| json!({ "action": serde_json::to_value(a).unwrap() }));
            let umid = match (ChatRepo { pool: &self.pool })
                .upsert_user_message_idempotent(
                    session_id,
                    if action.is_some() { "" } else { "在吗" },
                    &ulid::Ulid::new().to_string(),
                    "user",
                    metadata.as_ref(),
                )
                .await
                .unwrap()
            {
                UpsertUserOutcome::Inserted { message_id } => message_id,
                _ => unreachable!(),
            };
            sqlx::query("UPDATE engine.chat_messages SET sent_at = $2 WHERE id = $1")
                .bind(umid)
                .bind(sent_at)
                .execute(&self.pool)
                .await
                .unwrap();
            let _frames: Vec<_> = crate::pipeline::stream::run_stream(
                state.clone(),
                crate::pipeline::stream::PersistedUserMessage {
                    user_message_id: umid,
                    session_id,
                    user_id: self.user_id,
                    instance_id: self.instance_id,
                    content: if action.is_some() { "" } else { "在吗" }.into(),
                    prompt_traits: vec![],
                    audit: None,
                    tier: None,
                    memory_scope: Default::default(),
                    affinity_scope: Default::default(),
                    image_url: None,
                    image: None,
                    quote: Default::default(),
                    user_locale: Default::default(),
                    action,
                    outfit: None,
                },
                None,
            )
            .collect()
            .await;
            umid
        }

        /// (status, resolved_at) of the choice.
        async fn status_of(&self, choice: Uuid) -> (String, Option<DateTime<Utc>>) {
            sqlx::query_as("SELECT status, resolved_at FROM engine.life_choices WHERE id = $1")
                .bind(choice)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// (kind, event_key, session_id, started_at, ends_at, message_id) of
        /// the instance's events, oldest first.
        #[allow(clippy::type_complexity)]
        async fn instance_events(
            &self,
        ) -> Vec<(
            String,
            String,
            Option<Uuid>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<Uuid>,
        )> {
            sqlx::query_as(
                "SELECT kind, event_key, session_id, started_at, ends_at, message_id \
                   FROM engine.persona_events WHERE instance_id = $1 \
                  ORDER BY started_at, id",
            )
            .bind(self.instance_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// A running event of the instance, written in a sibling session (an
        /// event belongs to the persona, not the session it was drawn in).
        async fn seed_running(
            &self,
            kind: &str,
            key: &str,
            started_at: DateTime<Utc>,
            ends_at: DateTime<Utc>,
        ) {
            let elsewhere = seed_session(&self.pool, self.user_id, self.instance_id).await;
            sqlx::query(
                "INSERT INTO engine.persona_events \
                   (instance_id, session_id, event_key, kind, started_at, ends_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(self.instance_id)
            .bind(elsewhere)
            .bind(key)
            .bind(kind)
            .bind(started_at)
            .bind(ends_at)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// The four line axes, whose position is the tiers, and the number of
        /// `life_choice` affinity events of the session. The endpoints (warmth,
        /// patience) and the stored tier columns follow the lines on any turn
        /// and settle in the turn's detached post-process, so they are left out.
        async fn affinity_fingerprint(&self) -> (Vec<f64>, i64) {
            let axes: Vec<f64> = sqlx::query_scalar(
                "SELECT ARRAY[trust, intrigue, intimacy, tension] \
                   FROM engine.companion_affinity WHERE session_id = $1",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            let drops: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.session_id = $1 AND e.event_type = 'life_choice'",
            )
            .bind(self.session_id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            (axes, drops)
        }
    }

    /// The first ordinary text message after she asked is the user's answer:
    /// the choice is `ignored`, the falling-out starts at that message, and
    /// the very turn that ignored her is answered with it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_first_ordinary_message_converts_to_fell_out(pool: PgPool) {
        let s = scene(&pool).await;
        let (state, mock) = turn_env(&pool).await;
        let sent = recent(10);
        let choice = s.pending_at(sent - Duration::hours(2)).await;

        let umid = s.stream_turn(&state, sent, None).await;

        assert_eq!(
            s.status_of(choice).await,
            ("ignored".into(), Some(sent)),
            "resolved at the message that ignored her"
        );
        let events = s.instance_events().await;
        assert_eq!(
            events,
            vec![(
                "fell_out".to_string(),
                format!("life_choice:{choice}:fell_out"),
                Some(s.session_id),
                sent,
                sent + Duration::hours(1),
                None,
            )],
            "an hour from the message, in the choice's session, with no message of its own"
        );
        let current = EventRepo { pool: &pool }
            .current(s.instance_id, Some(umid), Utc::now())
            .await
            .unwrap();
        assert_eq!(
            current.foreground.map(|e| e.kind),
            Some(EventKind::FellOut),
            "under way"
        );

        // The same turn's judge and reply see it.
        let judge = request_to(&mock, "pde/judge").await;
        assert_eq!(judge.len(), 1);
        assert!(
            judge[0].contains(&format!("[近期事件] {FELL_OUT_LABEL}（")),
            "{}",
            judge[0]
        );
        let reply = request_to(&mock, "deepseek/x").await;
        assert_eq!(reply.len(), 1);
        assert!(
            reply[0].contains(&format!("- 正在经历：{FELL_OUT_LABEL}（")),
            "{}",
            reply[0]
        );

        // No tier change, and no drop event: the judge rates the turn itself.
        // The scene's lines (bond 0.0, chemistry 0.7: tiers 1 and 4) stand.
        assert_eq!(
            s.affinity_fingerprint().await,
            (vec![0.0, 0.0, 0.7, 0.7], 0)
        );

        // The next message finds nothing pending: no second row, no new resolve.
        s.stream_turn(&state, sent + Duration::minutes(5), None)
            .await;
        assert_eq!(s.instance_events().await, events);
        assert_eq!(s.status_of(choice).await, ("ignored".into(), Some(sent)));
    }

    /// A give, a tip or a kiss while she waits is an attempt to mend, not an
    /// answer: the ask stays pending and the action is answered by its own
    /// rules. The next ordinary message is the answer.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_action_turn_leaves_the_ask_pending(pool: PgPool) {
        use eros_engine_core::types::UserAction;

        let s = scene(&pool).await;
        let (state, mock) = turn_env(&pool).await;
        let sent = recent(30);
        let choice = s.pending_at(sent - Duration::hours(2)).await;

        s.stream_turn(&state, sent, Some(UserAction::Kiss)).await;
        s.stream_turn(
            &state,
            sent + Duration::minutes(1),
            Some(UserAction::Tip { amount_usd: 1.0 }),
        )
        .await;
        assert_eq!(s.status_of(choice).await, ("pending".into(), None));
        assert!(s.instance_events().await.is_empty(), "no falling-out");
        assert!(
            request_to(&mock, "deepseek/x")
                .await
                .iter()
                .all(|r| !r.contains(FELL_OUT_LABEL)),
            "and her replies know of none"
        );

        let word = sent + Duration::minutes(2);
        s.stream_turn(&state, word, None).await;
        assert_eq!(s.status_of(choice).await, ("ignored".into(), Some(word)));
        assert_eq!(s.instance_events().await.len(), 1);
    }

    /// A choice that lapsed (24 hours past its ask, the instant itself
    /// included) converts nothing: it stays `pending`, there is no falling-out
    /// and the turn goes on as any other. One microsecond earlier it was
    /// still waiting.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_expired_ask_converts_nothing(pool: PgPool) {
        let (state, mock) = turn_env(&pool).await;
        let micro = Duration::microseconds(1);
        let asked = recent(60 * 24 + 30);
        let expiry = asked + Duration::hours(24);

        for (sent, converts) in [
            (expiry + micro, false),
            (expiry, false),
            (expiry - micro, true),
        ] {
            let s = scene(&pool).await;
            let choice = s.pending_at(asked).await;
            s.stream_turn(&state, sent, None).await;
            let (status, resolved) = s.status_of(choice).await;
            if converts {
                assert_eq!((status.as_str(), resolved), ("ignored", Some(sent)));
                assert_eq!(s.instance_events().await.len(), 1);
            } else {
                assert_eq!((status.as_str(), resolved), ("pending", None), "at {sent}");
                assert!(s.instance_events().await.is_empty(), "at {sent}");
            }
        }
        // The lapsed turns were answered all the same, with nothing said of it.
        let replies = request_to(&mock, "deepseek/x").await;
        assert_eq!(replies.len(), 3);
        assert_eq!(
            replies
                .iter()
                .filter(|r| r.contains(FELL_OUT_LABEL))
                .count(),
            1,
            "only the turn that converted hears of the falling-out"
        );
    }

    /// A message sent before her ask landed cannot be the answer to it: the
    /// ask can be written while the user's next message is already in flight.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_message_sent_before_the_ask_does_not_answer_it(pool: PgPool) {
        let s = scene(&pool).await;
        let (state, _mock) = turn_env(&pool).await;
        let asked = recent(20);
        let choice = s.pending_at(asked).await;

        s.stream_turn(&state, asked - Duration::seconds(1), None)
            .await;
        assert_eq!(s.status_of(choice).await, ("pending".into(), None));
        assert!(s.instance_events().await.is_empty());

        s.stream_turn(&state, asked + Duration::seconds(1), None)
            .await;
        assert_eq!(s.status_of(choice).await.0, "ignored");
    }

    /// The falling-out displaces the incident she was in: it ends where the
    /// falling-out starts, so the one-incident invariant holds. A background
    /// (a trip) keeps running underneath, and the turn hears of both.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_running_incident_is_capped_by_the_conversion(pool: PgPool) {
        let s = scene(&pool).await;
        let (state, mock) = turn_env(&pool).await;
        let sent = recent(10);
        s.pending_at(sent - Duration::hours(2)).await;
        // She was reading, and she is on a business trip.
        s.seed_running(
            "reading",
            "r",
            sent - Duration::minutes(20),
            sent + Duration::minutes(100),
        )
        .await;
        s.seed_running(
            "business_trip",
            "t",
            sent - Duration::hours(5),
            sent + Duration::hours(10),
        )
        .await;

        s.stream_turn(&state, sent, None).await;

        let events = s.instance_events().await;
        let by_kind = |k: &str| events.iter().find(|e| e.0 == k).unwrap().clone();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(
            by_kind("reading").4,
            sent,
            "the incident ends at the message"
        );
        assert_eq!(
            by_kind("business_trip").4,
            sent + Duration::hours(10),
            "a background is not displaced"
        );
        assert_eq!(
            (by_kind("fell_out").3, by_kind("fell_out").4),
            (sent, sent + Duration::hours(1))
        );

        let judge = request_to(&mock, "pde/judge").await;
        assert!(judge[0].contains(FELL_OUT_LABEL), "{}", judge[0]);
        assert!(judge[0].contains("出差"), "{}", judge[0]);
        assert!(
            !judge[0].contains("看书"),
            "she is no longer reading: {}",
            judge[0]
        );
    }

    /// The conversion is all or nothing, and a failure of it never costs the
    /// user their reply. A falling-out already sits under the key the
    /// conversion is about to write (the incident cap runs first, then the
    /// insert hits the unique key and fails), so the production function's
    /// own transaction must undo the resolve and the cap together: the ask
    /// stays pending, the seeded row is untouched, and the turn is answered.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_conversion_insert_leaves_the_ask_pending_and_the_turn_alive(pool: PgPool) {
        let s = scene(&pool).await;
        let (state, mock) = turn_env(&pool).await;
        let sent = recent(10);
        let choice = s.pending_at(sent - Duration::hours(2)).await;
        // Running at the message, in the choice's own session: the cap would
        // end it at `sent`, and its key is the one the insert is about to take.
        let (started, ends) = (sent - Duration::minutes(20), sent + Duration::minutes(40));
        sqlx::query(
            "INSERT INTO engine.persona_events \
               (instance_id, session_id, event_key, kind, started_at, ends_at) \
             VALUES ($1, $2, $3, 'fell_out', $4, $5)",
        )
        .bind(s.instance_id)
        .bind(s.session_id)
        .bind(format!("life_choice:{choice}:fell_out"))
        .bind(started)
        .bind(ends)
        .execute(&pool)
        .await
        .unwrap();
        let seeded = vec![(
            "fell_out".to_string(),
            format!("life_choice:{choice}:fell_out"),
            Some(s.session_id),
            started,
            ends,
            None,
        )];
        assert_eq!(s.instance_events().await, seeded);

        s.stream_turn(&state, sent, None).await;

        assert_eq!(
            s.status_of(choice).await,
            ("pending".into(), None),
            "the resolve went down with the failed insert"
        );
        assert_eq!(
            s.instance_events().await,
            seeded,
            "no second row, and the cap went down with the insert (ends_at not cut to the message)"
        );
        assert_eq!(
            request_to(&mock, "deepseek/x").await.len(),
            1,
            "the error was logged and swallowed: the turn was still answered"
        );
    }

    // ---- One pending ask per instance; the conversion never runs apart ----

    /// She has one life: while any session of the instance waits for an answer
    /// the others do not ask, however thin her patience runs there. Answering
    /// it lets the same streak through.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_sibling_sessions_pending_ask_holds_the_trigger_back(pool: PgPool) {
        let a = scene(&pool).await;
        let b = lopsided_session(&pool, a.user_id, a.instance_id).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        a.seed_choice("pending", Duration::hours(1)).await;

        // B meets every other condition: the gap, three low judged turns, a
        // text session of a genome instance, nothing under way.
        for _ in 0..3 {
            a.turn_in(&state, b, SEEN).await;
        }
        assert!(a.choices_in(b).await.is_empty(), "A's ask holds B back");
        assert!(
            companion_calls(&mock).await.is_empty(),
            "and no generation spent"
        );
        assert_eq!(a.choices().await.len(), 1, "A's ask is untouched");

        sqlx::query("UPDATE engine.life_choices SET status = 'ignored' WHERE session_id = $1")
            .bind(a.session_id)
            .execute(&pool)
            .await
            .unwrap();
        a.turn_in(&state, b, SEEN).await;
        assert_eq!(a.choices_in(b).await.len(), 1, "answered, B asks");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    /// A message during an agreed separation only ghosts. A choice a sibling
    /// session holds pending (the state before the instance held one ask at a
    /// time) is not converted by it: no falling-out, and the separation
    /// keeps its `ends_at`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_message_during_a_separation_converts_no_pending_ask(pool: PgPool) {
        let a = scene(&pool).await;
        let b = lopsided_session(&pool, a.user_id, a.instance_id).await;
        let (state, _mock) = turn_env(&pool).await;
        let sent = recent(10);
        // A's ask was accepted: she is apart until ten hours after the message.
        accepted_separation(&a, a.session_id, sent + Duration::hours(10)).await;
        let choice = a.pending_in(b, sent - Duration::hours(2)).await;
        let before = a.instance_events().await;
        assert_eq!(before.len(), 1, "the separation, nothing else");

        let umid = a.stream_turn_in(b, &state, sent, None).await;

        let ghosted: bool =
            sqlx::query_scalar("SELECT ghost_decision FROM engine.chat_messages WHERE id = $1")
                .bind(umid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ghosted, "a message during a separation ghosts");
        assert_eq!(
            a.status_of(choice).await,
            ("pending".into(), None),
            "and converts nothing"
        );
        assert_eq!(
            a.instance_events().await,
            before,
            "no falling-out, and the separation is not capped at the message"
        );
    }

    // ---- The streak starts over after the whole episode (spec §3.1) ----

    impl Scene {
        /// An accepted choice asked `asked_ago` ago and answered `resolved_ago`
        /// ago, whose separation ran from the answer until `ends_ago` ago.
        async fn seed_accepted_choice(
            &self,
            asked_ago: Duration,
            resolved_ago: Duration,
            ends_ago: Duration,
        ) {
            let now = Utc::now();
            let choice: Uuid = sqlx::query_scalar(
                "INSERT INTO engine.life_choices \
                   (session_id, kind, status, tier_gap, asked_at, resolved_at) \
                 VALUES ($1, 'ask_for_space', 'accepted', 3, $2, $3) RETURNING id",
            )
            .bind(self.session_id)
            .bind(now - asked_ago)
            .bind(now - resolved_ago)
            .fetch_one(&self.pool)
            .await
            .unwrap();
            let event = EventRepo { pool: &self.pool }
                .insert_preempting(&NewEvent {
                    instance_id: self.instance_id,
                    session_id: self.session_id,
                    event_key: "apart",
                    kind: EventKind::TakingSpace,
                    started_at: now - resolved_ago,
                    ends_at: now - ends_ago,
                })
                .await
                .unwrap();
            LifeChoiceRepo { pool: &self.pool }
                .set_event(choice, event.id)
                .await
                .unwrap();
        }

        /// Move every affinity event of the scene `ago` into the past.
        async fn age_events(&self, ago: Duration) {
            sqlx::query(
                "UPDATE engine.companion_affinity_events \
                    SET created_at = created_at - make_interval(secs => $1)",
            )
            .bind(ago.num_seconds() as f64)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    /// Judged turns while she waits (a give, a tip or a kiss does not answer
    /// her) are not the new streak: the ignore that follows resolves the
    /// choice at the converting message, and the converting turn is the first
    /// of three. Anchored at the ask it would fire on the converting turn
    /// itself.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_give_then_an_ignore_does_not_ask_again_on_the_converting_turn(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());
        s.pending_at(Utc::now() - Duration::hours(2)).await;

        // Three low judged turns, all after the ask, none of them an answer.
        for _ in 0..3 {
            s.turn(&state, SEEN).await;
        }
        assert_eq!(s.choices().await.len(), 1, "held back while she waits");

        // An ordinary message: the ask is ignored. Its own judge reading is low,
        // and it is turn one of the new streak.
        s.converting_turn(&state, SEEN).await;
        let choices = s.choices().await;
        assert_eq!(choices.len(), 1, "{choices:?}");
        assert_eq!(choices[0].0, "ignored");
        assert!(
            companion_calls(&mock).await.is_empty(),
            "no ask on the turn that ignored her"
        );

        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "two turns after the episode");
        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 2, "three turns after it ask again");
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }

    /// Action turns inside a separation are judged like any other, but they
    /// are part of the episode: once she is back the streak counts only the
    /// turns after the separation ended.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn action_turns_during_a_separation_do_not_pre_load_the_return_streak(pool: PgPool) {
        let s = scene(&pool).await;
        let mock = mock_llm(LOW_PATIENCE, Some(ASK_TEXT)).await;
        let state = with_models(test_state(pool.clone()), &mock.uri());

        // Two judged action turns, 20 hours ago. The separation they fell in
        // ran from 29 to 11 hours ago, and the ask was put 30 hours ago.
        s.turn(&state, SEEN).await;
        s.turn(&state, SEEN).await;
        assert!(s.choices().await.is_empty(), "two turns are not three");
        s.age_events(Duration::hours(20)).await;
        s.seed_accepted_choice(
            Duration::hours(30),
            Duration::hours(29),
            Duration::hours(11),
        )
        .await;

        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "one turn since she came back");
        assert!(companion_calls(&mock).await.is_empty());
        s.turn(&state, SEEN).await;
        assert_eq!(s.choices().await.len(), 1, "two");
        s.turn(&state, SEEN).await;
        assert_eq!(
            s.choices().await.len(),
            2,
            "three since the return ask again"
        );
        assert_eq!(companion_calls(&mock).await.len(), 1);
    }
}
