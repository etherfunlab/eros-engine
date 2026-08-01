// SPDX-License-Identifier: AGPL-3.0-only
//! Embedding routing: the OpenRouter-compatible embeddings client and the
//! read/write `EmbeddingRouter` facade (spec 2026-08-01-embedding-providers).

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::voyage::EMBEDDING_DIM;

/// Built-in OpenRouter embeddings endpoint. Overridable per deployment via
/// `[providers].openrouter.embeddings`.
pub const OPENROUTER_EMBEDDINGS_URL: &str = "https://openrouter.ai/api/v1/embeddings";

/// OpenRouter-compatible embeddings client (also serves any `[providers]`
/// entry's `embeddings` URL — the third-party contract is exactly this wire).
/// No `input_type`: the query/document optimisation is Voyage-native.
#[derive(Clone)]
pub struct EmbedHttpClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    headers: reqwest::header::HeaderMap,
    model: String,
}

#[derive(Debug, Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    /// Always 512: the pgvector schema is VECTOR(512), and common models
    /// default elsewhere (text-embedding-3-small → 1536), so the param is
    /// mandatory, not optional.
    dimensions: u32,
}

#[derive(Debug, Deserialize)]
struct WireData {
    embedding: Vec<f32>,
    #[serde(default)]
    index: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    data: Vec<WireData>,
}

impl EmbedHttpClient {
    pub fn new(
        base_url: String,
        api_key: String,
        headers: reqwest::header::HeaderMap,
        model: String,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            headers,
            model,
        }
    }

    pub async fn embed_one(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        let mut out = self.post(&[text]).await?;
        Ok(out.remove(0))
    }

    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        self.post(texts).await
    }

    async fn post(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        if self.api_key.is_empty() {
            return Err(LlmError::Config("embeddings: api key not set".into()));
        }
        let body = WireRequest {
            model: &self.model,
            input: texts.to_vec(),
            dimensions: EMBEDDING_DIM as u32,
        };
        let resp = self
            .http
            .post(&self.base_url)
            .headers(self.headers.clone())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let err = LlmError::Status(status, crate::openrouter::scrub_error_body(&text));
            tracing::warn!("embeddings: {err}");
            return Err(err);
        }
        parse_response(&text, texts.len(), &self.model)
    }
}

/// Parse an OpenRouter-compatible embeddings response: exactly one embedding
/// per input, re-ordered by `index` when present, every vector 512-dim.
fn parse_response(body: &str, expected: usize, model: &str) -> Result<Vec<Vec<f32>>, LlmError> {
    let parsed: WireResponse = serde_json::from_str(body)
        .map_err(|e| LlmError::Provider(format!("embeddings: bad response: {e}")))?;
    if parsed.data.len() != expected {
        return Err(LlmError::Provider(format!(
            "embeddings: expected {expected} embeddings, got {}",
            parsed.data.len()
        )));
    }
    let mut data = parsed.data;
    // `index` is optional on the wire; when present on all rows, honour it.
    if data.iter().all(|d| d.index.is_some()) {
        data.sort_by_key(|d| d.index.unwrap_or(usize::MAX));
    }
    let out: Vec<Vec<f32>> = data.into_iter().map(|d| d.embedding).collect();
    for v in &out {
        if v.len() != EMBEDDING_DIM {
            return Err(LlmError::Provider(format!(
                "embeddings: model {model} returned a {}-dim embedding, expected \
                 {EMBEDDING_DIM} (the pgvector schema is VECTOR({EMBEDDING_DIM}))",
                v.len()
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_partial_json, header, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn sends_openrouter_compatible_body_and_headers() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .and(header("Authorization", "Bearer test-key"))
            .and(header("X-Team", "companion"))
            .and(body_partial_json(serde_json::json!({
                "model": "openai/text-embedding-3-small",
                "input": ["hello"],
                "dimensions": 512
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [ { "embedding": vec![0.0f32; 512], "index": 0 } ] }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("X-Team", "companion".parse().unwrap());
        let client = EmbedHttpClient::new(
            format!("{}/v1/embeddings", server.uri()),
            "test-key".into(),
            headers,
            "openai/text-embedding-3-small".into(),
        );
        let v = client.embed_one("hello").await.expect("embed succeeds");
        assert_eq!(v.len(), 512);
    }

    #[tokio::test]
    async fn batch_orders_by_index_and_checks_count() {
        let server = MockServer::start().await;
        let mut a = vec![0.0f32; 512];
        a[0] = 1.0;
        let mut b = vec![0.0f32; 512];
        b[0] = 2.0;
        // Deliberately index-swapped: the client must re-order.
        Mock::given(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "embedding": b, "index": 1 },
                    { "embedding": a, "index": 0 }
                ]
            })))
            .mount(&server)
            .await;
        let client = EmbedHttpClient::new(
            format!("{}/v1/embeddings", server.uri()),
            "k".into(),
            Default::default(),
            "m".into(),
        );
        let out = client.embed_batch(&["x", "y"]).await.expect("succeeds");
        assert_eq!(out[0][0], 1.0);
        assert_eq!(out[1][0], 2.0);
    }

    #[tokio::test]
    async fn count_mismatch_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "embedding": vec![0.0f32; 512], "index": 0 } ]
            })))
            .mount(&server)
            .await;
        let client = EmbedHttpClient::new(
            format!("{}/v1/embeddings", server.uri()),
            "k".into(),
            Default::default(),
            "m".into(),
        );
        assert!(client.embed_batch(&["x", "y"]).await.is_err());
    }

    #[tokio::test]
    async fn wrong_dim_is_a_clear_error() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "embedding": [0.0, 1.0], "index": 0 } ]
            })))
            .mount(&server)
            .await;
        let client = EmbedHttpClient::new(
            format!("{}/v1/embeddings", server.uri()),
            "k".into(),
            Default::default(),
            "m".into(),
        );
        let msg = client.embed_one("x").await.unwrap_err().to_string();
        assert!(msg.contains("512"), "{msg}");
    }

    #[tokio::test]
    async fn error_body_is_scrubbed_and_bounded() {
        let server = MockServer::start().await;
        let huge = format!(
            r#"{{"error":{{"code":400,"message":"{}","metadata":{{"flagged_input":"SECRET"}}}}}}"#,
            "x".repeat(5000)
        );
        Mock::given(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(400).set_body_string(huge))
            .mount(&server)
            .await;
        let client = EmbedHttpClient::new(
            format!("{}/v1/embeddings", server.uri()),
            "k".into(),
            Default::default(),
            "m".into(),
        );
        let err = client.embed_one("x").await.unwrap_err();
        let crate::error::LlmError::Status(_, msg) = err else {
            panic!("expected Status, got {err:?}");
        };
        assert!(msg.chars().count() <= 220);
        assert!(!msg.contains("SECRET"));
    }

    #[tokio::test]
    async fn empty_batch_short_circuits() {
        // No server: a network call would error, so Ok(vec![]) proves the
        // short-circuit.
        let client = EmbedHttpClient::new(
            "http://127.0.0.1:9/v1/embeddings".into(),
            "k".into(),
            Default::default(),
            "m".into(),
        );
        assert_eq!(
            client.embed_batch(&[]).await.expect("ok"),
            Vec::<Vec<f32>>::new()
        );
    }
}
