// SPDX-License-Identifier: AGPL-3.0-only
//! Append-only writers for the two image-path audit tables:
//! `chat_images_events` (composer calls) and `chat_vision_events` (describe
//! calls). Both are best-effort telemetry, written fail-open by the server.

use sqlx::PgPool;
use uuid::Uuid;

/// One `chat_images_events` row — a single image-composer call.
///
/// `inputs` is the five composer slots, structured and never concatenated:
/// `{appearance, recent_scene, latest_user_msg, style, aspect_ratio}`. Absent
/// slots are the empty string, NOT the `（无）` placeholder the prompt renders —
/// that substitution is a rendering detail of the prompt, not an input.
///
/// `subject` / `caption` are the composer's own output and are NULL unless
/// `status == "ok"`. `composed_prompt` is NOT: the portrait fallback still
/// assembles a wire prompt on a failed compose, and this column is then the
/// only record of what the consumer was asked to draw.
pub struct ImageComposeEventInsert<'a> {
    pub source: &'a str,
    pub user_id: Uuid,
    pub instance_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub status: &'a str,
    pub inputs: serde_json::Value,
    pub subject: Option<&'a str>,
    pub caption: Option<&'a str>,
    pub composed_prompt: Option<&'a str>,
    pub variant: Option<&'a str>,
    /// `record_generation`'s return value. The model and usage for this call
    /// live in `engine.llm_generations`, reached by joining on this column.
    pub generation_id: Option<&'a str>,
    /// Models actually called off `[primary, ...fallback]`; 0 when the task is
    /// not configured.
    pub attempts: i16,
    /// Why the last attempt failed; NULL when `status == "ok"`.
    pub last_failure: Option<&'a str>,
    /// Provider-layer failures for the chains behind this call, as a JSON
    /// array. `None` when nothing failed; an empty array is never written.
    pub llm_attempts: Option<serde_json::Value>,
    /// Gateway-layer failures for the same chains.
    pub gateway_errors: Option<serde_json::Value>,
}

pub struct ImageComposeEventRepo<'a> {
    pub pool: &'a PgPool,
}

/// What a persona instance wore in her last picture inside a window: the
/// clothing slot as the composer was sent it, who dressed her
/// (`inputs.clothing_source`; `None` on a row that predates the key), and the
/// whole minutes between that picture and the turn's moment.
#[derive(Debug, Clone, PartialEq)]
pub struct LastWorn {
    pub clothing: String,
    pub source: Option<String>,
    pub minutes: i32,
}

impl ImageComposeEventRepo<'_> {
    /// The clothing of the newest picture of `instance_id`, in any session and
    /// of any source, taken inside the `window_minutes` before the turn's
    /// moment and strictly before it, whose `inputs.clothing` is not empty.
    ///
    /// The turn's moment is the `sent_at` of `driving_message`, the user
    /// message the turn answers; `now` stands in when the turn answers none or
    /// the id matches no row. The strict upper bound keeps the read to earlier
    /// turns: a turn never reads its own picture, and a later message the
    /// queue ran first does not leak into an earlier turn. The lower edge is
    /// inclusive.
    pub async fn last_worn(
        &self,
        instance_id: Uuid,
        driving_message: Option<Uuid>,
        now: chrono::DateTime<chrono::Utc>,
        window_minutes: i32,
    ) -> Result<Option<LastWorn>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String, Option<String>, i32)>(
            "WITH m AS ( \
               SELECT COALESCE((SELECT sent_at FROM engine.chat_messages WHERE id = $2), $3) AS t \
             ) \
             SELECT e.inputs->>'clothing', \
                    e.inputs->>'clothing_source', \
                    floor(extract(epoch FROM (m.t - e.created_at)) / 60)::int \
             FROM engine.chat_images_events e, m \
             WHERE e.instance_id = $1 \
               AND e.created_at < m.t \
               AND e.created_at >= m.t - make_interval(mins => $4) \
               AND COALESCE(e.inputs->>'clothing', '') <> '' \
             ORDER BY e.created_at DESC \
             LIMIT 1",
        )
        .bind(instance_id)
        .bind(driving_message)
        .bind(now)
        .bind(window_minutes)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(|(clothing, source, minutes)| LastWorn {
            clothing,
            source,
            minutes,
        }))
    }

    /// The composed wire prompt recorded on one compose event, scoped to the
    /// session that owns it — the session predicate is the ownership check,
    /// so a message whose `compose_event_id` points at another session's
    /// event resolves to `None`, never to someone else's prompt. `None` also
    /// when the row is gone or the compose never produced a prompt — the
    /// write is fail-open, so absence is a legal state the caller must
    /// surface, not an error.
    pub async fn composed_prompt_in_session(
        &self,
        event_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT composed_prompt FROM engine.chat_images_events \
             WHERE id = $1 AND session_id = $2",
        )
        .bind(event_id)
        .bind(session_id)
        .fetch_optional(self.pool)
        .await
        .map(Option::flatten)
    }

    /// Append one audit row and return its id. The caller stamps that id onto
    /// the assistant row (`metadata.image.compose_event_id`) — this table has
    /// no `message_id`, so the returned id IS the linkage.
    pub async fn record(&self, ev: ImageComposeEventInsert<'_>) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_images_events \
               (source, user_id, instance_id, session_id, status, inputs, subject, \
                caption, composed_prompt, variant, generation_id, \
                attempts, last_failure, llm_attempts, gateway_errors) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) \
             RETURNING id",
        )
        .bind(ev.source)
        .bind(ev.user_id)
        .bind(ev.instance_id)
        .bind(ev.session_id)
        .bind(ev.status)
        .bind(ev.inputs)
        .bind(ev.subject)
        .bind(ev.caption)
        .bind(ev.composed_prompt)
        .bind(ev.variant)
        .bind(ev.generation_id)
        .bind(ev.attempts)
        .bind(ev.last_failure)
        .bind(ev.llm_attempts)
        .bind(ev.gateway_errors)
        .fetch_one(self.pool)
        .await
    }
}

/// One `chat_vision_events` row — a single `chat_vision` describe call over a
/// user-sent image.
///
/// `vision` duplicates the successful describe already merged into the user
/// row's `metadata.vision`. That redundancy is deliberate: it lets this table
/// answer "how many describes ran, on what, at what success rate" without
/// joining `chat_messages` to establish a denominator.
///
/// The user's accompanying text is NOT stored — `message_id` points at
/// `chat_messages.content`, and real user text is not duplicated across tables.
///
/// One row per image-carrying, non-tipped turn THAT REACHES THE TEXT-REPLY
/// PATH. A turn that never reaches that path — ghosted, routed to
/// product_qa, or answered with an image-only reply — writes no row: the
/// describe never runs on those paths either (running one on a ghost turn
/// would waste a paid call). Callers computing a describe success rate off
/// this table should treat "image-carrying turns that reached the text-reply
/// path" as the denominator, not every image-carrying turn.
pub struct ChatVisionEventInsert<'a> {
    pub user_id: Uuid,
    pub session_id: Uuid,
    /// The `role='user'` row carrying the image.
    pub message_id: Uuid,
    pub status: &'a str,
    pub image_url: &'a str,
    pub vision: Option<serde_json::Value>,
    /// `record_generation`'s return value. The model and usage for this call
    /// live in `engine.llm_generations`, reached by joining on this column.
    pub generation_id: Option<&'a str>,
    /// Models actually called off `[primary, ...fallback]`; 0 when
    /// `[tasks.chat_vision]` is not configured.
    pub attempts: i16,
    /// `empty` / `unparseable` / `content_filter` / `blank_description` /
    /// `refusal_pattern` / `upstream_error` / `gateway_error`; NULL when
    /// `status == "ok"`.
    pub last_failure: Option<&'a str>,
    /// Provider-layer failures for the chains behind this call, as a JSON
    /// array. `None` when nothing failed; an empty array is never written.
    pub llm_attempts: Option<serde_json::Value>,
    /// Gateway-layer failures for the same chains.
    pub gateway_errors: Option<serde_json::Value>,
}

pub struct ChatVisionEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl ChatVisionEventRepo<'_> {
    /// Append one audit row. No id is returned: `message_id` is the linkage.
    pub async fn record(&self, ev: ChatVisionEventInsert<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.chat_vision_events \
               (user_id, session_id, message_id, status, image_url, vision, \
                generation_id, attempts, last_failure, llm_attempts, gateway_errors) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(ev.user_id)
        .bind(ev.session_id)
        .bind(ev.message_id)
        .bind(ev.status)
        .bind(ev.image_url)
        .bind(ev.vision)
        .bind(ev.generation_id)
        .bind(ev.attempts)
        .bind(ev.last_failure)
        .bind(ev.llm_attempts)
        .bind(ev.gateway_errors)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        seed_chat_message, seed_chat_session, seed_generation, seed_persona_instance, seed_session,
    };
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn compose_event_round_trips_ok_and_exhausted(pool: PgPool) {
        let repo = ImageComposeEventRepo { pool: &pool };
        let user = Uuid::new_v4();
        // instance_id and session_id are FK-referenced since 0058.
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        // ...and generation_id since 0060.
        seed_generation(&pool, "gen_compose_1").await;

        let ok_id = repo
            .record(ImageComposeEventInsert {
                source: "chat_reply_text_image",
                user_id: user,
                instance_id: Some(instance),
                session_id: Some(session),
                status: "ok",
                inputs: serde_json::json!({
                    "appearance": "long black hair",
                    "recent_scene": "在厨房",
                    "latest_user_msg": "拍张照",
                    "style": "realistic",
                    "aspect_ratio": "3:4",
                }),
                subject: Some("she leans on the counter"),
                caption: Some("厨房里的她"),
                composed_prompt: Some("photorealistic, long black hair, she leans on the counter"),
                variant: Some("raw"),
                generation_id: Some("gen_compose_1"),
                attempts: 1,
                last_failure: None,
                llm_attempts: None,
                gateway_errors: None,
            })
            .await
            .unwrap();

        // Chain exhausted: no composer output, but the portrait fallback still
        // produced a wire prompt, so composed_prompt is NOT null here.
        repo.record(ImageComposeEventInsert {
            source: "chat_reply_image",
            user_id: user,
            instance_id: None,
            session_id: Some(session),
            status: "exhausted",
            inputs: serde_json::json!({
                "appearance": "",
                "recent_scene": "",
                "latest_user_msg": "画一张",
                "style": "realistic",
                "aspect_ratio": "",
            }),
            subject: None,
            caption: None,
            composed_prompt: Some("photorealistic portrait"),
            variant: None,
            generation_id: None,
            attempts: 2,
            last_failure: Some("timeout"),
            llm_attempts: None,
            gateway_errors: None,
        })
        .await
        .unwrap();

        let (status, subject, inputs, attempts, last_failure): (
            String,
            Option<String>,
            serde_json::Value,
            i16,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT status, subject, inputs, attempts, last_failure \
             FROM engine.chat_images_events WHERE id = $1",
        )
        .bind(ok_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "ok");
        assert_eq!(subject.as_deref(), Some("she leans on the counter"));
        assert_eq!(inputs["recent_scene"].as_str(), Some("在厨房"));
        assert_eq!(attempts, 1);
        assert_eq!(last_failure, None);

        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_images_events WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_event_source_is_accepted(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        ImageComposeEventRepo { pool: &pool }
            .record(ImageComposeEventInsert {
                source: "event",
                user_id: user,
                instance_id: Some(instance),
                session_id: Some(session),
                status: "ok",
                inputs: serde_json::json!({"recent_scene": "AI: 刚拍的", "latest_user_msg": ""}),
                subject: Some("a selfie by the sea"),
                caption: Some("海边自拍"),
                composed_prompt: Some("photorealistic, a selfie by the sea"),
                variant: None,
                generation_id: None,
                attempts: 1,
                last_failure: None,
                llm_attempts: None,
                gateway_errors: None,
            })
            .await
            .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn chat_images_events_has_rls_enabled(pool: PgPool) {
        let enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
             WHERE oid = 'engine.chat_images_events'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity for chat_images_events");
        assert!(enabled, "RLS must be enabled on engine.chat_images_events");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn vision_event_round_trips_ok_and_not_configured(pool: PgPool) {
        let repo = ChatVisionEventRepo { pool: &pool };
        let user = Uuid::new_v4();
        // session_id and message_id are FK-referenced since 0058.
        let session = seed_chat_session(&pool, user).await;
        let msg_ok = seed_chat_message(&pool, session).await;
        // ...and generation_id since 0060.
        seed_generation(&pool, "gen_vision_1").await;

        repo.record(ChatVisionEventInsert {
            user_id: user,
            session_id: session,
            message_id: msg_ok,
            status: "ok",
            image_url: "https://example.invalid/a.jpg",
            vision: Some(serde_json::json!({"description": "a cat on a sofa"})),
            generation_id: Some("gen_vision_1"),
            attempts: 1,
            last_failure: None,
            llm_attempts: None,
            gateway_errors: None,
        })
        .await
        .unwrap();

        // No [tasks.chat_vision] on this deployment: no call was made at all.
        repo.record(ChatVisionEventInsert {
            user_id: user,
            session_id: session,
            message_id: seed_chat_message(&pool, session).await,
            status: "not_configured",
            image_url: "https://example.invalid/b.jpg",
            vision: None,
            generation_id: None,
            attempts: 0,
            last_failure: None,
            llm_attempts: None,
            gateway_errors: None,
        })
        .await
        .unwrap();

        let (status, vision, attempts): (String, Option<serde_json::Value>, i16) = sqlx::query_as(
            "SELECT status, vision, attempts FROM engine.chat_vision_events WHERE message_id = $1",
        )
        .bind(msg_ok)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "ok");
        assert_eq!(
            vision.expect("describe round-trips")["description"].as_str(),
            Some("a cat on a sofa")
        );
        assert_eq!(attempts, 1);

        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_vision_events WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn chat_vision_events_has_rls_enabled(pool: PgPool) {
        let enabled: bool = sqlx::query_scalar(
            "SELECT relrowsecurity FROM pg_class \
             WHERE oid = 'engine.chat_vision_events'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("query relrowsecurity for chat_vision_events");
        assert!(enabled, "RLS must be enabled on engine.chat_vision_events");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn image_edit_is_an_accepted_source(pool: PgPool) {
        let repo = ImageComposeEventRepo { pool: &pool };
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        seed_generation(&pool, "g").await;
        let id = repo
            .record(ImageComposeEventInsert {
                llm_attempts: None,
                gateway_errors: None,
                source: "image_edit",
                user_id: user,
                instance_id: Some(instance),
                session_id: Some(session),
                status: "ok",
                inputs: serde_json::json!({ "instruction": "换套衣服" }),
                subject: Some("SUBJECT"),
                caption: Some("换了条裙子"),
                composed_prompt: Some("STYLE\n银发红瞳\nSUBJECT"),
                variant: None,
                generation_id: Some("g"),
                attempts: 1,
                last_failure: None,
            })
            .await
            .expect("the widened CHECK must accept image_edit");

        let (source, instruction): (String, String) = sqlx::query_as(
            "SELECT source, inputs->>'instruction' FROM engine.chat_images_events WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(source, "image_edit");
        assert_eq!(instruction, "换套衣服");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_source_is_still_rejected(pool: PgPool) {
        // The widening must not have dropped the constraint outright.
        let repo = ImageComposeEventRepo { pool: &pool };
        let err = repo
            .record(ImageComposeEventInsert {
                llm_attempts: None,
                gateway_errors: None,
                source: "not_a_real_source",
                user_id: Uuid::new_v4(),
                instance_id: None,
                session_id: None,
                status: "ok",
                inputs: serde_json::json!({}),
                subject: None,
                caption: None,
                composed_prompt: None,
                variant: None,
                generation_id: None,
                attempts: 0,
                last_failure: None,
            })
            .await
            .expect_err("an unknown source must violate the CHECK");
        assert!(err.as_database_error().is_some(), "got {err:?}");
    }

    /// The fixed moment the `last_worn` tests count back from.
    fn noon() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc
            .with_ymd_and_hms(2026, 11, 28, 12, 0, 0)
            .unwrap()
    }

    /// Append one picture row of `instance` at `at` and return its id. `record`
    /// stamps `now()`, so the row is moved to `at` afterwards.
    async fn seed_picture(
        pool: &PgPool,
        user: Uuid,
        instance: Uuid,
        session: Uuid,
        at: chrono::DateTime<chrono::Utc>,
        inputs: serde_json::Value,
    ) -> Uuid {
        let id = ImageComposeEventRepo { pool }
            .record(ImageComposeEventInsert {
                source: "chat_reply_text_image",
                user_id: user,
                instance_id: Some(instance),
                session_id: Some(session),
                status: "ok",
                inputs,
                subject: None,
                caption: None,
                composed_prompt: None,
                variant: None,
                generation_id: None,
                attempts: 1,
                last_failure: None,
                llm_attempts: None,
                gateway_errors: None,
            })
            .await
            .unwrap();
        sqlx::query("UPDATE engine.chat_images_events SET created_at = $2 WHERE id = $1")
            .bind(id)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    fn worn(clothing: &str, source: &str) -> serde_json::Value {
        serde_json::json!({ "clothing": clothing, "clothing_source": source })
    }

    fn minutes_before(mins: i64) -> chrono::DateTime<chrono::Utc> {
        noon() - chrono::Duration::minutes(mins)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_reads_the_newest_row_in_the_window(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(30),
            worn("毛衣", "steward"),
        )
        .await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(10),
            worn("米色开衫", "steward"),
        )
        .await;

        let got = ImageComposeEventRepo { pool: &pool }
            .last_worn(instance, None, noon(), 45)
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(LastWorn {
                clothing: "米色开衫".to_string(),
                source: Some("steward".to_string()),
                minutes: 10,
            })
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_lapses_after_45_minutes(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        let repo = ImageComposeEventRepo { pool: &pool };
        let id = seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(46),
            worn("毛衣", "steward"),
        )
        .await;
        assert_eq!(
            repo.last_worn(instance, None, noon(), 45).await.unwrap(),
            None
        );

        // The window's lower edge is inclusive.
        sqlx::query("UPDATE engine.chat_images_events SET created_at = $2 WHERE id = $1")
            .bind(id)
            .bind(minutes_before(45))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            repo.last_worn(instance, None, noon(), 45).await.unwrap(),
            Some(LastWorn {
                clothing: "毛衣".to_string(),
                source: Some("steward".to_string()),
                minutes: 45,
            })
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_never_reads_at_or_after_the_moment(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            noon(),
            worn("毛衣", "steward"),
        )
        .await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            noon() + chrono::Duration::minutes(5),
            worn("睡衣", "steward"),
        )
        .await;

        let got = ImageComposeEventRepo { pool: &pool }
            .last_worn(instance, None, noon(), 45)
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_skips_an_empty_slot(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(5),
            worn("", "engine"),
        )
        .await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(20),
            worn("毛衣", "steward"),
        )
        .await;

        let got = ImageComposeEventRepo { pool: &pool }
            .last_worn(instance, None, noon(), 45)
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(LastWorn {
                clothing: "毛衣".to_string(),
                source: Some("steward".to_string()),
                minutes: 20,
            })
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_is_per_instance_across_sessions(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let other_instance = seed_persona_instance(&pool, user).await;
        // The turn answers a message in a session of its own; the picture was
        // taken in an earlier session of the same instance.
        let earlier_session = seed_session(&pool, user, instance, false).await;
        let turn_session = seed_session(&pool, user, instance, false).await;
        let message = seed_chat_message(&pool, turn_session).await;
        sqlx::query("UPDATE engine.chat_messages SET sent_at = $2 WHERE id = $1")
            .bind(message)
            .bind(noon())
            .execute(&pool)
            .await
            .unwrap();
        let other_session = seed_session(&pool, user, other_instance, false).await;
        // Another instance's picture is newer and is not read.
        seed_picture(
            &pool,
            user,
            other_instance,
            other_session,
            minutes_before(10),
            worn("睡衣", "steward"),
        )
        .await;
        seed_picture(
            &pool,
            user,
            instance,
            earlier_session,
            minutes_before(20),
            worn("毛衣", "steward"),
        )
        .await;

        let got = ImageComposeEventRepo { pool: &pool }
            .last_worn(instance, Some(message), noon(), 45)
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(LastWorn {
                clothing: "毛衣".to_string(),
                source: Some("steward".to_string()),
                minutes: 20,
            })
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_returns_a_row_without_source(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(10),
            serde_json::json!({ "clothing": "毛衣" }),
        )
        .await;

        let got = ImageComposeEventRepo { pool: &pool }
            .last_worn(instance, None, noon(), 45)
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(LastWorn {
                clothing: "毛衣".to_string(),
                source: None,
                minutes: 10,
            })
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_takes_the_moment_from_the_message(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        let message = seed_chat_message(&pool, session).await;
        sqlx::query("UPDATE engine.chat_messages SET sent_at = $2 WHERE id = $1")
            .bind(message)
            .bind(noon())
            .execute(&pool)
            .await
            .unwrap();
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(10),
            worn("毛衣", "steward"),
        )
        .await;

        // `now` is ninety minutes after the message: the window follows the
        // message, not the clock.
        let now = noon() + chrono::Duration::minutes(90);
        let got = ImageComposeEventRepo { pool: &pool }
            .last_worn(instance, Some(message), now, 45)
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(LastWorn {
                clothing: "毛衣".to_string(),
                source: Some("steward".to_string()),
                minutes: 10,
            })
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn last_worn_falls_back_to_now_without_the_message(pool: PgPool) {
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let session = seed_chat_session(&pool, user).await;
        seed_picture(
            &pool,
            user,
            instance,
            session,
            minutes_before(10),
            worn("毛衣", "steward"),
        )
        .await;

        let repo = ImageComposeEventRepo { pool: &pool };
        let want = Some(LastWorn {
            clothing: "毛衣".to_string(),
            source: Some("steward".to_string()),
            minutes: 10,
        });
        // A message id that matches no row reads as no message.
        assert_eq!(
            repo.last_worn(instance, Some(Uuid::nil()), noon(), 45)
                .await
                .unwrap(),
            want
        );
        assert_eq!(
            repo.last_worn(instance, None, noon(), 45).await.unwrap(),
            want
        );
    }
}
