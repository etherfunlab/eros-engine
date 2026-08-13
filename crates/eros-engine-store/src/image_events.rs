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
    pub model: Option<&'a str>,
    pub usage: Option<serde_json::Value>,
    pub generation_id: Option<&'a str>,
    /// Models actually called off `[primary, ...fallback]`; 0 when the task is
    /// not configured.
    pub attempts: i16,
    /// Why the last attempt failed; NULL when `status == "ok"`.
    pub last_failure: Option<&'a str>,
}

pub struct ImageComposeEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl ImageComposeEventRepo<'_> {
    /// Append one audit row and return its id. The caller stamps that id onto
    /// the assistant row (`metadata.image.compose_event_id`) — this table has
    /// no `message_id`, so the returned id IS the linkage.
    pub async fn record(&self, ev: ImageComposeEventInsert<'_>) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_images_events \
               (source, user_id, instance_id, session_id, status, inputs, subject, \
                caption, composed_prompt, variant, model, usage, generation_id, \
                attempts, last_failure) \
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
        .bind(ev.model)
        .bind(ev.usage)
        .bind(ev.generation_id)
        .bind(ev.attempts)
        .bind(ev.last_failure)
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
    pub model: Option<&'a str>,
    pub usage: Option<serde_json::Value>,
    pub generation_id: Option<&'a str>,
    /// Models actually called off `[primary, ...fallback]`; 0 when
    /// `[tasks.chat_vision]` is not configured.
    pub attempts: i16,
    /// `model_error` / `timeout` / `empty` / `unparseable` / `content_filter` /
    /// `blank_description` / `refusal_pattern`; NULL when `status == "ok"`.
    pub last_failure: Option<&'a str>,
}

pub struct ChatVisionEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl ChatVisionEventRepo<'_> {
    /// Append one audit row. No id is returned: `message_id` is the linkage.
    pub async fn record(&self, ev: ChatVisionEventInsert<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.chat_vision_events \
               (user_id, session_id, message_id, status, image_url, vision, model, \
                usage, generation_id, attempts, last_failure) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(ev.user_id)
        .bind(ev.session_id)
        .bind(ev.message_id)
        .bind(ev.status)
        .bind(ev.image_url)
        .bind(ev.vision)
        .bind(ev.model)
        .bind(ev.usage)
        .bind(ev.generation_id)
        .bind(ev.attempts)
        .bind(ev.last_failure)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn compose_event_round_trips_ok_and_exhausted(pool: PgPool) {
        let repo = ImageComposeEventRepo { pool: &pool };
        let user = Uuid::new_v4();
        let session = Uuid::new_v4();

        let ok_id = repo
            .record(ImageComposeEventInsert {
                source: "chat_reply_text_image",
                user_id: user,
                instance_id: Some(Uuid::new_v4()),
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
                model: Some("x-ai/grok-4-mini"),
                usage: Some(serde_json::json!({"total_tokens": 88})),
                generation_id: Some("gen_compose_1"),
                attempts: 1,
                last_failure: None,
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
            model: None,
            usage: None,
            generation_id: None,
            attempts: 2,
            last_failure: Some("timeout"),
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
        let session = Uuid::new_v4();
        let msg_ok = Uuid::new_v4();

        repo.record(ChatVisionEventInsert {
            user_id: user,
            session_id: session,
            message_id: msg_ok,
            status: "ok",
            image_url: "https://example.invalid/a.jpg",
            vision: Some(serde_json::json!({"description": "a cat on a sofa"})),
            model: Some("vendor/vision-1"),
            usage: Some(serde_json::json!({"total_tokens": 41})),
            generation_id: Some("gen_vision_1"),
            attempts: 1,
            last_failure: None,
        })
        .await
        .unwrap();

        // No [tasks.chat_vision] on this deployment: no call was made at all.
        repo.record(ChatVisionEventInsert {
            user_id: user,
            session_id: session,
            message_id: Uuid::new_v4(),
            status: "not_configured",
            image_url: "https://example.invalid/b.jpg",
            vision: None,
            model: None,
            usage: None,
            generation_id: None,
            attempts: 0,
            last_failure: None,
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
}
