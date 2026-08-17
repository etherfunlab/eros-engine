// SPDX-License-Identifier: AGPL-3.0-only
//! Append-only writer for `companion_decision_events` (PDE judge audit).

use sqlx::PgPool;
use uuid::Uuid;

/// One `companion_decision_events` row. `payload` is the full verdict JSON when
/// the judge parsed, or the raw model text on a parse error. `proposed_action`
/// is the judge's pre-guardrail action (NULL unless `status == "ok"`).
/// `inputs` is the engine-computed relationship state the judge was shown
/// (issue #254) — engine-supplied, never model output, NULL when unavailable.
pub struct DecisionEventInsert<'a> {
    pub run_id: Uuid,
    pub user_id: Uuid,
    pub session_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub status: &'a str,
    pub action: Option<&'a str>,
    pub proposed_action: Option<&'a str>,
    pub payload: Option<serde_json::Value>,
    pub inputs: Option<serde_json::Value>,
    pub model: Option<&'a str>,
    pub usage: Option<serde_json::Value>,
    pub generation_id: Option<&'a str>,
    /// Provider-layer failures for the chains behind this call, as a JSON
    /// array. `None` when nothing failed; an empty array is never written.
    pub llm_attempts: Option<serde_json::Value>,
    /// Gateway-layer failures for the same chains.
    pub gateway_errors: Option<serde_json::Value>,
}

pub struct DecisionEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl DecisionEventRepo<'_> {
    /// Append one audit row. Append-only; no FK (a row may precede any related row).
    pub async fn record(&self, ev: DecisionEventInsert<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.companion_decision_events \
               (run_id, user_id, session_id, message_id, status, action, \
                proposed_action, payload, inputs, model, usage, generation_id, \
                llm_attempts, gateway_errors) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
        )
        .bind(ev.run_id)
        .bind(ev.user_id)
        .bind(ev.session_id)
        .bind(ev.message_id)
        .bind(ev.status)
        .bind(ev.action)
        .bind(ev.proposed_action)
        .bind(ev.payload)
        .bind(ev.inputs)
        .bind(ev.model)
        .bind(ev.usage)
        .bind(ev.generation_id)
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
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn record_round_trips_ok_and_parse_error(pool: PgPool) {
        let repo = DecisionEventRepo { pool: &pool };
        let run_ok = Uuid::new_v4();
        let user = Uuid::new_v4();

        repo.record(DecisionEventInsert {
            run_id: run_ok,
            user_id: user,
            session_id: Some(Uuid::new_v4()),
            message_id: Some(Uuid::new_v4()),
            status: "ok",
            action: Some("reply_text"),
            proposed_action: Some("ghost"),
            payload: Some(serde_json::json!({"action":"ghost","inner_state":"想躲"})),
            inputs: Some(serde_json::json!({
                "v": 1, "intimacy_rung": 2, "patience_band": "mid",
                "bond": 0.41, "chemistry": 0.27,
            })),
            model: Some("x-ai/grok-4-mini"),
            usage: Some(serde_json::json!({"total_tokens": 12})),
            generation_id: Some("gen_1"),
            llm_attempts: None,
            gateway_errors: None,
        })
        .await
        .unwrap();

        // parse_error row: raw text payload, NULL proposed_action, NULL inputs
        // (the fail-open path when snapshot serialisation fails).
        repo.record(DecisionEventInsert {
            run_id: Uuid::new_v4(),
            user_id: user,
            session_id: None,
            message_id: None,
            status: "parse_error",
            action: Some("reply_text"),
            proposed_action: None,
            payload: Some(serde_json::Value::String("garbage from model".into())),
            inputs: None,
            model: Some("x-ai/grok-4-mini"),
            usage: None,
            generation_id: None,
            llm_attempts: None,
            gateway_errors: None,
        })
        .await
        .unwrap();

        let (status, action, proposed, inputs): (
            String,
            Option<String>,
            Option<String>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT status, action, proposed_action, inputs FROM engine.companion_decision_events \
             WHERE run_id = $1",
        )
        .bind(run_ok)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "ok");
        assert_eq!(action.as_deref(), Some("reply_text"));
        assert_eq!(proposed.as_deref(), Some("ghost"));
        let inputs = inputs.expect("engine-supplied inputs snapshot round-trips");
        assert_eq!(inputs["intimacy_rung"].as_i64(), Some(2));
        assert_eq!(inputs["patience_band"].as_str(), Some("mid"));

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.companion_decision_events WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn decision_status_accepts_pointer_values_and_historical_ones(pool: PgPool) {
        let repo = DecisionEventRepo { pool: &pool };
        for status in [
            "ok",
            "empty",
            "parse_error",
            "timeout",
            "error",
            "upstream_error",
            "gateway_error",
        ] {
            repo.record(DecisionEventInsert {
                run_id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                session_id: None,
                message_id: None,
                status,
                action: None,
                proposed_action: None,
                payload: None,
                inputs: None,
                model: None,
                usage: None,
                generation_id: None,
                llm_attempts: None,
                gateway_errors: None,
            })
            .await
            .unwrap_or_else(|e| panic!("status {status} must be accepted: {e}"));
        }
    }
}
