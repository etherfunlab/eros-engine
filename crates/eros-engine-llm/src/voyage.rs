// SPDX-License-Identifier: AGPL-3.0-only
//! Voyage embedding client — multilingual text → 512-dim vector.
//!
//! Docs: https://docs.voyageai.com/reference/embeddings-api

use serde::{Deserialize, Serialize};

use crate::error::LlmError;

const BASE_URL: &str = "https://api.voyageai.com/v1/embeddings";
const DEFAULT_MODEL: &str = "voyage-4-lite";
pub const EMBEDDING_DIM: usize = 512;

/// Voyage models with a FIXED output dimension, where the API's
/// `output_dimension` parameter must not be sent. voyage-3-lite (the
/// pre-voyage-4-era default, 512-dim, no longer recommended by Voyage) stays
/// listed so a deployment that pins it keeps its pre-config wire unchanged.
const FIXED_DIM_MODELS: &[&str] = &["voyage-3-lite"];

#[derive(Clone)]
pub struct VoyageClient {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    input: Vec<&'a str>,
    model: &'a str,
    input_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dimension: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

impl VoyageClient {
    pub fn new(api_key: String) -> Self {
        Self::with_base_url(api_key, BASE_URL.to_string())
    }

    /// Test constructor: point the client at a mock server. Production code
    /// uses `new`, which pins Voyage's canonical URL.
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            base_url,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Set the model from `[tasks.embedding]` (spec 2026-08-01). Consuming
    /// builder, boot-chained.
    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// `output_dimension` for the wire: always 512 (the pgvector schema is
    /// VECTOR(512)) — omitted for fixed-dim legacy models.
    fn wire_output_dimension(&self) -> Option<u32> {
        if FIXED_DIM_MODELS.contains(&self.model.as_str()) {
            None
        } else {
            Some(EMBEDDING_DIM as u32)
        }
    }

    /// Embed a single document (content-type = "document").
    pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.embed_internal(text, "document").await
    }

    /// Embed a query (content-type = "query"). Optimised for retrieval.
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.embed_internal(text, "query").await
    }

    async fn embed_internal(&self, text: &str, input_type: &str) -> Result<Vec<f32>, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Config("voyage: api key not set".into()));
        }
        if text.trim().is_empty() {
            return Err(LlmError::Config("voyage: empty input text".into()));
        }

        let body = EmbedRequest {
            input: vec![text],
            model: &self.model,
            input_type,
            output_dimension: self.wire_output_dimension(),
        };

        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let err = status_error(status, &text);
            tracing::warn!("voyage: {err}");
            return Err(err);
        }

        let parsed: EmbedResponse = resp.json().await?;
        let embedding = parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| LlmError::Provider("voyage: empty data array".into()))?;
        check_dim(&embedding, &self.model)?;
        Ok(embedding)
    }

    /// Embed a batch of documents in one HTTP call (order-preserving).
    /// Empty input short-circuits to `Ok(vec![])` without a network call.
    pub async fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        if self.api_key.is_empty() {
            return Err(LlmError::Config("voyage: api key not set".into()));
        }
        let body = EmbedRequest {
            input: texts.to_vec(),
            model: &self.model,
            input_type: "document",
            output_dimension: self.wire_output_dimension(),
        };
        let resp = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let err = status_error(status, &text);
            tracing::warn!("voyage: {err}");
            return Err(err);
        }
        parse_embed_batch(&text, texts.len(), &self.model)
    }
}

/// Check that an embedding has the expected dimension.
fn check_dim(embedding: &[f32], model: &str) -> Result<(), LlmError> {
    if embedding.len() != EMBEDDING_DIM {
        return Err(LlmError::Provider(format!(
            "voyage: model {model} returned a {}-dim embedding, expected {EMBEDDING_DIM} \
             (the pgvector schema is VECTOR({EMBEDDING_DIM}))",
            embedding.len()
        )));
    }
    Ok(())
}

/// Build the bounded `LlmError::Status` for a non-success Voyage response.
/// The raw body never reaches the error (or the log line that mirrors it):
/// provider bodies are unbounded and may echo input, so they are scrubbed /
/// capped first — the same bounded-log guarantee the chat client upholds
/// (issue #188).
fn status_error(status: reqwest::StatusCode, body: &str) -> LlmError {
    LlmError::Status(status, crate::openrouter::scrub_error_body(body))
}

/// Parse a Voyage batch response body into ordered vectors, enforcing that
/// the provider returned exactly one embedding per input. Checks that each
/// embedding has the expected dimension.
fn parse_embed_batch(body: &str, expected: usize, model: &str) -> Result<Vec<Vec<f32>>, LlmError> {
    let parsed: EmbedResponse = serde_json::from_str(body)
        .map_err(|e| LlmError::Provider(format!("voyage: bad response: {e}")))?;
    if parsed.data.len() != expected {
        return Err(LlmError::Provider(format!(
            "voyage: expected {expected} embeddings, got {}",
            parsed.data.len()
        )));
    }
    let embeddings: Result<Vec<_>, _> = parsed
        .data
        .into_iter()
        .map(|d| {
            check_dim(&d.embedding, model)?;
            Ok(d.embedding)
        })
        .collect();
    embeddings
}

/// Format an f32 vector into the PostgreSQL pgvector textual form: `[0.1,0.2,...]`.
pub fn format_vector(embedding: &[f32]) -> String {
    let body: Vec<String> = embedding.iter().map(|v| format!("{v:.6}")).collect();
    format!("[{}]", body.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_vector_shape() {
        let v = vec![0.1, 0.2, -0.3];
        assert_eq!(format_vector(&v), "[0.100000,0.200000,-0.300000]");
    }

    #[test]
    fn test_format_vector_empty() {
        let v: Vec<f32> = vec![];
        assert_eq!(format_vector(&v), "[]");
    }

    #[test]
    fn parse_embed_batch_preserves_order_and_count() {
        let mut v1 = vec![0.0; 512];
        let mut v2 = vec![0.0; 512];
        v1[0] = 1.0;
        v2[0] = 0.0;
        let v1_vec = serde_json::json!(v1);
        let v2_vec = serde_json::json!(v2);
        let body =
            serde_json::json!({ "data": [{ "embedding": v1_vec }, { "embedding": v2_vec }] });
        let out = parse_embed_batch(&body.to_string(), 2, "voyage-3-lite").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], v1);
        assert_eq!(out[1], v2);
    }

    #[test]
    fn parse_embed_batch_rejects_count_mismatch() {
        let body = r#"{"data":[{"embedding":[1.0]}]}"#;
        assert!(
            parse_embed_batch(body, 2, "voyage-3-lite").is_err(),
            "1 vector for 2 inputs must error"
        );
    }

    #[test]
    fn parse_embed_batch_rejects_garbage() {
        assert!(parse_embed_batch("not json", 1, "voyage-3-lite").is_err());
    }

    fn vec512() -> Vec<f32> {
        vec![0.0; 512]
    }

    #[tokio::test]
    async fn voyage_4_sends_output_dimension_512() {
        use wiremock::matchers::{body_partial_json, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .and(body_partial_json(serde_json::json!({
                "model": "voyage-4-lite",
                "output_dimension": 512
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [ { "embedding": vec512() } ] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client =
            VoyageClient::with_base_url("k".into(), format!("{}/v1/embeddings", server.uri()))
                .with_model("voyage-4-lite".into());
        let v = client.embed_query("hello").await.expect("embed succeeds");
        assert_eq!(v.len(), 512);
    }

    #[tokio::test]
    async fn voyage_3_lite_omits_output_dimension() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [ { "embedding": vec512() } ] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client =
            VoyageClient::with_base_url("k".into(), format!("{}/v1/embeddings", server.uri()))
                .with_model("voyage-3-lite".into());
        let _ = client
            .embed_document("hello")
            .await
            .expect("embed succeeds");
        let reqs = server.received_requests().await.unwrap_or_default();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body.get("output_dimension").is_none(),
            "voyage-3-lite is fixed-dim; the param must be absent: {body}"
        );
        assert_eq!(body["input_type"], "document");
    }

    #[tokio::test]
    async fn wrong_dimension_response_is_a_clear_error() {
        use wiremock::matchers::path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "data": [ { "embedding": [0.0, 1.0] } ] })),
            )
            .mount(&server)
            .await;
        let client =
            VoyageClient::with_base_url("k".into(), format!("{}/v1/embeddings", server.uri()));
        let err = client.embed_query("hello").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("512"),
            "error must name the expected dim: {msg}"
        );
    }

    #[test]
    fn status_error_bounds_and_scrubs_provider_body() {
        // The stored error body must be bounded and stripped of any echoed
        // input (issue #188 item 4 — the bounded-log guarantee the v0.8.4
        // hardening established elsewhere extends to the embedding client).
        let huge = format!(
            r#"{{"error":{{"code":400,"message":"{}","metadata":{{"flagged_input":"SECRET"}}}}}}"#,
            "x".repeat(5000)
        );
        let e = status_error(reqwest::StatusCode::BAD_REQUEST, &huge);
        let LlmError::Status(status, msg) = e else {
            panic!("expected Status, got {e:?}");
        };
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
        assert!(
            msg.chars().count() <= 220,
            "must be bounded, got {} chars",
            msg.chars().count()
        );
        assert!(
            !msg.contains("SECRET"),
            "echoed input must be dropped: {msg}"
        );

        // A plain non-JSON body still comes through, just capped.
        let e = status_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream exploded",
        );
        let LlmError::Status(_, msg) = e else {
            panic!("expected Status, got {e:?}");
        };
        assert_eq!(msg, "upstream exploded");
    }
}
