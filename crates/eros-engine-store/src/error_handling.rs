// SPDX-License-Identifier: AGPL-3.0-only
//! Error-handling config persistence.
//!
//! Wraps the `engine.error_handling_config` kv table introduced in
//! migration 0020.  Each row has a string `kind` PK and a JSONB `payload`.
//! Consumers pattern-match on `kind`; unknown kinds are simply ignored.

use sqlx::PgPool;

pub struct ErrorHandlingRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> ErrorHandlingRepo<'a> {
    /// Pick one of the configured chat-stream fallback phrases at random.
    ///
    /// Returns `None` if the config row is missing or the payload is not a
    /// non-empty JSON array of strings — the caller should fall back to the
    /// raw Error frame in that case.
    pub async fn pick_chat_stream_fallback_phrase(&self) -> Result<Option<String>, sqlx::Error> {
        let payload: Option<serde_json::Value> = sqlx::query_scalar(
            "SELECT payload FROM engine.error_handling_config \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .fetch_optional(self.pool)
        .await?;

        let Some(serde_json::Value::Array(arr)) = payload else {
            return Ok(None);
        };
        if arr.is_empty() {
            return Ok(None);
        }

        use rand::seq::SliceRandom as _;
        let mut rng = rand::thread_rng();
        Ok(arr
            .choose(&mut rng)
            .and_then(|v| v.as_str().map(str::to_string)))
    }
}
