// SPDX-License-Identifier: AGPL-3.0-only
//! Chat-queue worker: drives enqueued async turns through `run_stream`.
//!
//! Spec: docs/superpowers/specs/2026-08-20-async-chat-endpoint-design.md §6.
//! Fifth boot-time sweeper. Strict per-session LIFO via
//! `ChatQueueRepo::claim_next`; per-session serialization comes from the
//! claim query's in-flight guard, NOT from any in-process lock. Wakes on
//! `state.chat_queue_notify` (enqueue nudge, turn-completion nudge) with the
//! interval tick as the correctness backstop (missed notifies, stale claims,
//! restarts).

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::Semaphore;
use uuid::Uuid;

use eros_engine_store::chat::ChatRepo;
use eros_engine_store::chat_queue::{ChatQueueRepo, ClaimedTurn};

use crate::pipeline::stream::{run_stream, PersistedUserMessage, ProtocolFrame};
use crate::routes::companion_async::QueuedTurnParams;
use crate::routes::companion_stream::AffinityScopeDto;
use crate::state::AppState;

#[derive(Debug)]
pub(crate) enum TurnOutcome {
    Success,
    Failure(String),
}

/// Completion rule. `run_stream` persists each logical message BEFORE its
/// `Done` frame, so any Done ⇒ history already has a served reply ⇒ retrying
/// would double-reply ⇒ Success. Zero Dones + an Error ⇒ genuine failure.
/// Zero Dones + zero Errors ⇒ ghost turn ⇒ Success (a silent companion is a
/// completed turn).
pub(crate) fn classify_outcome(done_frames: usize, last_error: Option<String>) -> TurnOutcome {
    if done_frames > 0 {
        return TurnOutcome::Success;
    }
    match last_error {
        Some(msg) => TurnOutcome::Failure(msg),
        None => TurnOutcome::Success,
    }
}

/// Run forever. Spawn once at server startup; returns immediately when
/// disabled, mirroring the other sweepers.
pub async fn worker(state: AppState) {
    let cfg = state.config.chat_queue.clone();
    if cfg.disabled {
        tracing::info!("chat-queue worker disabled (CHAT_QUEUE_DISABLED)");
        return;
    }
    tracing::info!(?cfg, "chat-queue worker starting");
    let sem = Arc::new(Semaphore::new(cfg.concurrency));
    let mut tick = tokio::time::interval(cfg.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = state.chat_queue_notify.notified() => {}
        }
        if let Err(e) = drain_once(&state, &sem).await {
            tracing::warn!("chat-queue drain failed: {e}");
        }
    }
}

/// One pass: reap stale claims, then claim-and-spawn until the queue or the
/// concurrency budget runs dry. Factored out of `worker` for tests. Returns
/// the number of turns spawned this pass.
pub(crate) async fn drain_once(
    state: &AppState,
    sem: &Arc<Semaphore>,
) -> Result<usize, sqlx::Error> {
    let cfg = &state.config.chat_queue;
    let repo = ChatQueueRepo { pool: &state.pool };
    // Reap BEFORE claiming: a stale claim blocks its whole session in
    // claim_next's in-flight guard.
    let reaped = repo.reap_stale(cfg.claim_stale, cfg.max_attempts).await?;
    for t in &reaped {
        write_failure_row(state, t.session_id).await;
    }
    let mut spawned = 0;
    loop {
        let Ok(permit) = Arc::clone(sem).try_acquire_owned() else {
            break; // concurrency budget exhausted; next wake resumes
        };
        match repo.claim_next().await? {
            Some(turn) => {
                let st = state.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    process_turn(st, turn).await;
                });
                spawned += 1;
            }
            None => {
                drop(permit);
                break;
            }
        }
    }
    Ok(spawned)
}

/// The terminal-failure signal a Realtime consumer can see (spec §6):
/// role='system_error', the role the old async path once produced.
async fn write_failure_row(state: &AppState, session_id: Uuid) {
    let chat_repo = ChatRepo { pool: &state.pool };
    if let Err(e) = chat_repo
        .append_message(session_id, "system_error", "AI 回复失败，请稍后再试")
        .await
    {
        tracing::warn!(%session_id, "chat-queue: system_error row write failed: {e}");
    }
}

async fn process_turn(state: AppState, turn: ClaimedTurn) {
    let cfg = state.config.chat_queue.clone();
    let repo = ChatQueueRepo { pool: &state.pool };
    let outcome =
        match tokio::time::timeout(cfg.generation_timeout, drive_turn(&state, &turn)).await {
            Ok(o) => o,
            Err(_) => TurnOutcome::Failure("generation timeout".into()),
        };
    let res = match outcome {
        TurnOutcome::Success => repo.mark_done(turn.queue_id).await,
        TurnOutcome::Failure(msg) => {
            if turn.attempts >= cfg.max_attempts {
                write_failure_row(&state, turn.session_id).await;
                repo.mark_failed(turn.queue_id, &msg).await
            } else {
                repo.release(turn.queue_id, &msg).await
            }
        }
    };
    if let Err(e) = res {
        tracing::warn!(queue_id = %turn.queue_id, "chat-queue: ladder update failed: {e}");
    }
    // This session may have a next stacked turn — wake the loop now rather
    // than waiting out a tick.
    state.chat_queue_notify.notify_one();
}

/// Rebuild the turn from the queue row + message row and drive `run_stream`
/// to exhaustion, discarding frames. Persistence, PDE, filters, ghosting and
/// post-process all live inside the generator already (spec §6 Execution).
async fn drive_turn(state: &AppState, turn: &ClaimedTurn) -> TurnOutcome {
    let chat_repo = ChatRepo { pool: &state.pool };

    // Replay guard. `run_stream` has none of its own — the stream path's guard
    // lives in the handler, which the worker bypasses — so every re-drive of an
    // already-answered turn double-replies. Re-drives are reachable: process
    // death between `insert_assistant_batch` and `mark_done`, a `mark_done`
    // error (logged and swallowed) leaving the row claimed until the reaper
    // releases it, a generation timeout that dropped the Done tally. A served
    // or deliberately ghosted driving row IS a completed turn.
    match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM engine.chat_messages \
                        WHERE user_message_id = $1 AND role = 'assistant') \
             OR COALESCE((SELECT ghost_decision FROM engine.chat_messages \
                          WHERE id = $1), false)",
    )
    .bind(turn.user_message_id)
    .fetch_one(&state.pool)
    .await
    {
        Ok(true) => {
            tracing::info!(
                queue_id = %turn.queue_id,
                user_message_id = %turn.user_message_id,
                "chat-queue: turn already answered — skipping re-drive"
            );
            return TurnOutcome::Success;
        }
        Ok(false) => {}
        Err(e) => return TurnOutcome::Failure(format!("replay guard failed: {e}")),
    }

    let row: Option<(String, Option<Uuid>)> = match sqlx::query_as(
        "SELECT m.content, s.instance_id \
         FROM engine.chat_messages m JOIN engine.chat_sessions s ON s.id = m.session_id \
         WHERE m.id = $1",
    )
    .bind(turn.user_message_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return TurnOutcome::Failure(format!("turn load failed: {e}")),
    };
    let Some((content, Some(instance_id))) = row else {
        return TurnOutcome::Failure("user message or instance_id missing".into());
    };

    // A malformed blob silently re-drives a gift / image turn as plain text,
    // so say so in the log rather than swallowing the error.
    let params: QueuedTurnParams = match turn.params.clone() {
        Some(v) => serde_json::from_value(v).unwrap_or_else(|e| {
            tracing::warn!(
                queue_id = %turn.queue_id,
                "chat-queue: unreadable params blob, driving as a plain turn: {e}"
            );
            QueuedTurnParams::default()
        }),
        None => QueuedTurnParams::default(),
    };
    // Re-run the pure validators the endpoint already ran; they cannot fail
    // here on rows the endpoint accepted, and a defensive failure just means
    // empty traits / no audit.
    let prompt_traits = crate::routes::companion::validate_prompt_traits(
        params.prompt_traits.as_deref().unwrap_or(&[]),
    )
    .unwrap_or_default();
    let audit =
        crate::routes::companion::validate_llm_audit(params.audit.clone()).unwrap_or_default();

    // Spec §9: an older-than-latest driving message must see everything that
    // landed after it — HistoryAnchor::Latest is exactly "the most recent
    // window as of now". reply_to quotes still anchor explicitly.
    let history_anchor = match params.reply_to_message_id {
        None => eros_engine_core::types::HistoryAnchor::Latest,
        Some(id) => match chat_repo
            .message_sent_at_in_session(turn.session_id, id)
            .await
        {
            Ok(Some(sent_at)) => eros_engine_core::types::HistoryAnchor::At {
                message_id: id,
                sent_at,
            },
            Ok(None) => eros_engine_core::types::HistoryAnchor::DropHistory,
            Err(e) => return TurnOutcome::Failure(format!("anchor resolve failed: {e}")),
        },
    };

    let user_msg = PersistedUserMessage {
        user_message_id: turn.user_message_id,
        session_id: turn.session_id,
        user_id: turn.user_id,
        instance_id,
        content,
        prompt_traits,
        audit,
        tier: params.tier.clone(),
        memory_scope: params.memory_scope.unwrap_or_default(),
        affinity_scope: params
            .affinity_scope
            .as_ref()
            .map(AffinityScopeDto::resolve)
            .unwrap_or_default(),
        tips_amount_usd: params.tips_amount_usd,
        image_url: params.image_url.clone(),
        image: params.image.clone(),
        history_anchor,
    };

    let stream = run_stream(Arc::new(state.clone()), user_msg, None);
    futures_util::pin_mut!(stream);
    let mut done_frames = 0usize;
    let mut last_error: Option<String> = None;
    while let Some(frame) = stream.next().await {
        match frame {
            ProtocolFrame::Done { .. } => done_frames += 1,
            ProtocolFrame::Error { message, .. } => last_error = Some(message),
            _ => {}
        }
    }
    classify_outcome(done_frames, last_error)
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn done_frames_mean_success_even_with_a_trailing_error() {
        assert!(matches!(classify_outcome(1, None), TurnOutcome::Success));
        assert!(matches!(
            classify_outcome(1, Some("upstream died".into())),
            TurnOutcome::Success
        ));
    }

    #[test]
    fn error_without_done_is_failure() {
        let TurnOutcome::Failure(msg) = classify_outcome(0, Some("upstream 500".into())) else {
            panic!("expected Failure");
        };
        assert_eq!(msg, "upstream 500");
    }

    #[test]
    fn no_done_no_error_is_a_ghost_and_a_success() {
        assert!(matches!(classify_outcome(0, None), TurnOutcome::Success));
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;
    use eros_engine_store::chat_queue::{ChatQueueRepo, EnqueueOutcome};
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::path as wm_path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn wait_for_status(pool: &PgPool, queue_id: Uuid, wanted: &[&str]) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let s: String =
                    sqlx::query_scalar("SELECT status FROM engine.chat_turn_queue WHERE id = $1")
                        .bind(queue_id)
                        .fetch_one(pool)
                        .await
                        .unwrap();
                if wanted.contains(&s.as_str()) {
                    return s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("turn should reach a terminal status")
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn drain_once_serves_an_enqueued_turn(pool: PgPool) {
        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" back\"}}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4},\"id\":\"gen-q\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let repo = ChatQueueRepo { pool: &pool };
        let EnqueueOutcome::Queued {
            queue_id,
            user_message_id,
        } = repo
            .enqueue_user_message(
                session_id,
                "hi",
                "01JW000000000000000000001A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap()
        else {
            panic!("expected Queued");
        };

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        let spawned = drain_once(&state, &sem).await.unwrap();
        assert_eq!(spawned, 1);

        let status = wait_for_status(&pool, queue_id, &["done", "failed"]).await;
        assert_eq!(status, "done");
        // Tolerant on PDE (it may pick ghost with the rule engine): if the
        // action was a reply, the assistant row must exist; a ghosted turn is
        // still done with ghost_decision on the user row.
        let assistant: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let ghosted: bool =
            sqlx::query_scalar("SELECT ghost_decision FROM engine.chat_messages WHERE id = $1")
                .bind(user_message_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            assistant > 0 || ghosted,
            "a done turn is either served or deliberately ghosted"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn failing_turn_releases_then_goes_terminal_with_system_error(pool: PgPool) {
        // Adapted from the task brief's contingency, but not the way the brief
        // anticipated. The brief expected a bare hard-500 chain to reach
        // run_stream's Error frame directly; two things make that untrue for
        // the plain-reply (`chat_companion`) path specifically:
        //
        // 1. `engine.error_handling_config` seeds 10 pseudo-ghost fallback
        //    phrases in every fresh migrated DB (migration 0020), so on chain
        //    exhaustion `build_stream_failure_pseudo_ghost` finds a phrase and
        //    serves it as a Done frame — regardless of failure kind (verified
        //    against `stream.rs`'s `exhausted_chain_pseudo_ghost_records_the_
        //    hops_and_the_verdict`, which hits the same path off a 503).
        //    Swapping to a connection-refused URL (the brief's suggested fix)
        //    would NOT help: the pseudo-ghost is keyed on chain exhaustion,
        //    not on the transport shape of the failure.
        // 2. Clearing the phrase table (mirroring
        //    `product_qa_error_frame_carries_the_providers_status`,
        //    stream.rs:13133) is necessary but not sufficient for the plain
        //    reply path: `run_stream`'s LIVE per-attempt loop persists a row
        //    and yields `Done{truncated:true}` for EVERY failed attempt (see
        //    stream.rs:508-535, the `Ok(Err(e))` arm falling through to the
        //    shared persist-and-Done code at :686-730) before the chain
        //    exhaustion check ever runs — confirmed empirically: a plain-reply
        //    turn against an always-500 mock, even with phrases cleared,
        //    yields `[Meta, Done{truncated:true}, Error]`. Per the completion
        //    ladder, any Done ⇒ Success, so that combination can never
        //    classify as Failure through this action type.
        //
        // `chat_product_qa` walks its own hand-rolled candidate loop instead
        // of the shared per-attempt persist logic (see
        // `recovered_product_qa_chain_persists_the_507_on_the_answer_row`'s
        // comment, stream.rs:13279-13282) and its no-phrase exhaustion arm
        // persists nothing and emits ONLY an Error frame
        // (`product_qa_error_frame_carries_the_providers_status` asserts
        // `rows == 0`). Routing this test's turn through product_qa — via a
        // mocked PDE judge, exactly like that test does — is the actually
        // effective way to reach zero-Done+Error (Failure) end to end through
        // the real worker/queue pipeline.
        sqlx::query(
            "UPDATE engine.error_handling_config \
             SET payload = '[]'::jsonb \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mock = MockServer::start().await;
        let verdict = serde_json::json!({ "action": "product_qa", "inner_state": "" }).to_string();
        let judge_body = serde_json::json!({
            "id": "gj", "model": "pde/judge",
            "choices": [{"message": {"content": verdict}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_string_contains("pde/judge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(judge_body))
            .mount(&mock)
            .await;
        // Both product_qa executor candidates fail — the chain exhausts.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_string_contains("qa/exec-a"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_string_contains("qa/exec-b"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n\
                 [tasks.chat_product_qa]\nmodel=\"qa/exec-a\"\nfallback=[\"qa/exec-b\"]\nretry_depth=1\n\
                 filter_prompt=\"Answer using the product docs below.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let repo = ChatQueueRepo { pool: &pool };
        let EnqueueOutcome::Queued { queue_id, .. } = repo
            .enqueue_user_message(
                session_id,
                "这个产品能用几年",
                "01JW000000000000000000002A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap()
        else {
            panic!("expected Queued");
        };

        // Attempt 1: released back to pending with last_error recorded.
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        drain_once(&state, &sem).await.unwrap();
        let status = wait_for_status(&pool, queue_id, &["pending", "failed"]).await;
        assert_eq!(status, "pending", "attempt 1 of 3 must release, not fail");

        // Fast-forward to the final attempt and drain again → terminal.
        sqlx::query("UPDATE engine.chat_turn_queue SET attempts = 3 WHERE id = $1")
            .bind(queue_id)
            .execute(&pool)
            .await
            .unwrap();
        drain_once(&state, &sem).await.unwrap();
        let status = wait_for_status(&pool, queue_id, &["failed", "done"]).await;
        assert_eq!(status, "failed");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "terminal failure must leave a system_error row");
    }

    /// Replay guard. The stream path's already-answered guard lives in the
    /// handler, which the worker bypasses entirely — so a re-drive of a turn
    /// whose reply is already persisted (worker death after
    /// `insert_assistant_batch` but before `mark_done`, a swallowed
    /// `mark_done` error, a generation
    /// timeout that dropped the Done tally) would double-reply into the
    /// session. The guard must short-circuit before any upstream call.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_turn_whose_reply_is_already_persisted_is_not_re_driven(pool: PgPool) {
        use eros_engine_store::chat::{AssistantInsert, ChatRepo};

        // Any upstream call at all is a failure of this test; a 500 keeps a
        // regression from silently persisting a second reply.
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let repo = ChatQueueRepo { pool: &pool };
        let EnqueueOutcome::Queued {
            queue_id,
            user_message_id,
        } = repo
            .enqueue_user_message(
                session_id,
                "在吗",
                "01JW000000000000000000003A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap()
        else {
            panic!("expected Queued");
        };

        // The reply landed; the queue row never reached `done` (crash / DB
        // blip) and the reaper handed it back as claimable.
        ChatRepo { pool: &pool }
            .insert_assistant_batch(
                session_id,
                user_message_id,
                &[AssistantInsert {
                    id: ulid::Ulid::new().into(),
                    content: "已经回过了".into(),
                    assistant_action_type: "reply".into(),
                    continues_from_message_id: None,
                    truncated: false,
                    model: None,
                    usage: None,
                    generation_id: None,
                    filter_audit: None,
                    metadata: None,
                    llm_attempts: None,
                    gateway_errors: None,
                }],
            )
            .await
            .unwrap();

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        assert_eq!(drain_once(&state, &sem).await.unwrap(), 1);

        let status = wait_for_status(&pool, queue_id, &["done", "failed", "pending"]).await;
        assert_eq!(status, "done", "an already-served turn settles as done");
        assert!(
            mock.received_requests().await.unwrap().is_empty(),
            "the guard must fire before any upstream call",
        );
        let assistant: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE user_message_id = $1 AND role = 'assistant'",
        )
        .bind(user_message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(assistant, 1, "a re-drive must not append a second reply");
    }

    /// Spec §9. On the worker path the driving message is NOT the newest row
    /// in its session: a LIFO burst (and any exchange that landed while the
    /// turn waited) can bury it past `HISTORY_WINDOW` = 20. The prompt's
    /// model-facing messages come only from that window, so without a pin the
    /// model answers a message it never sees.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_old_driving_message_is_pinned_into_the_prompt(pool: PgPool) {
        use wiremock::matchers::body_string_contains;

        const DRIVING: &str = "我的暗号是 ZQ-8817";

        let mock = MockServer::start().await;
        // Judge → reply_text, so the turn is never ghosted and the chat call
        // definitely fires.
        let verdict = serde_json::json!({ "action": "reply_text", "inner_state": "" }).to_string();
        let judge_body = serde_json::json!({
            "id": "gj", "model": "pde/judge",
            "choices": [{"message": {"content": verdict}}],
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("pde/judge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(judge_body))
            .mount(&mock)
            .await;
        let chat_body = "data: {\"choices\":[{\"delta\":{\"content\":\"收到\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2},\"id\":\"g\",\"model\":\"deepseek/x\"}\n\ndata: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("deepseek/x"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(chat_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.chat_companion]\nmodel=\"deepseek/x\"\n\
                 [tasks.pde_decision]\nmodel=\"pde/judge\"\nfilter_prompt=\"Decide the action and inner_state.\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let repo = ChatQueueRepo { pool: &pool };
        let EnqueueOutcome::Queued {
            queue_id,
            user_message_id,
        } = repo
            .enqueue_user_message(
                session_id,
                DRIVING,
                "01JW000000000000000000004A",
                "user",
                None,
                user_id,
                &serde_json::json!({}),
                20,
            )
            .await
            .unwrap()
        else {
            panic!("expected Queued");
        };

        // 24 rows land after it — twelve exchanges, more than the 20-row
        // window — so the driving row is no longer in `history()`'s result.
        // Explicit `sent_at` offsets keep the DESC ordering tie-free.
        for i in 0..12i64 {
            for (offset, role, content) in [
                (2 * i + 1, "user", format!("后来的提问 {i}")),
                (2 * i + 2, "assistant", format!("后来的回答 {i}")),
            ] {
                sqlx::query(
                    "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
                     VALUES ($1, $2, $3, now() + make_interval(secs => $4::float8))",
                )
                .bind(session_id)
                .bind(role)
                .bind(content)
                .bind(offset as f64)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        let in_window: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT id FROM engine.chat_messages \
             WHERE session_id = $1 ORDER BY sent_at DESC LIMIT 20) w WHERE w.id = $2",
        )
        .bind(session_id)
        .bind(user_message_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            in_window, 0,
            "test setup must actually evict the driving row from the window",
        );

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        assert_eq!(drain_once(&state, &sem).await.unwrap(), 1);
        let status = wait_for_status(&pool, queue_id, &["done", "failed"]).await;
        assert_eq!(status, "done");

        let reqs = mock.received_requests().await.unwrap();
        let chat_req = reqs
            .iter()
            .find(|r| String::from_utf8_lossy(&r.body).contains("deepseek/x"))
            .expect("the chat generation call must have fired");
        let sent = String::from_utf8_lossy(&chat_req.body);
        assert!(
            sent.contains(DRIVING),
            "the driving message must reach the model even when evicted from \
             the newest-20 window; got {sent}",
        );
    }
}
