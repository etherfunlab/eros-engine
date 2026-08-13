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
}
