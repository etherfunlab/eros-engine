// SPDX-License-Identifier: AGPL-3.0-only
//! Voyage embedding client — multilingual text → 512-dim vector.
//!
//! Docs: https://docs.voyageai.com/reference/embeddings-api

use serde::{Deserialize, Serialize};

use crate::error::LlmError;

const BASE_URL: &str = "https://api.voyageai.com/v1/embeddings";
const DEFAULT_MODEL: &str = "voyage-3-lite";
pub const EMBEDDING_DIM: usize = 512;

#[derive(Clone)]
pub struct VoyageClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    input: Vec<&'a str>,
    model: &'a str,
    input_type: &'a str,
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
        Self {
            http: reqwest::Client::new(),
            api_key,
            model: DEFAULT_MODEL.to_string(),
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
        };

        let resp = self
            .http
            .post(BASE_URL)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!("voyage: status={status} body={text}");
            return Err(LlmError::Status(status, text));
        }

        let parsed: EmbedResponse = resp.json().await?;
        parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| LlmError::Provider("voyage: empty data array".into()))
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
        };
        let resp = self
            .http
            .post(BASE_URL)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            tracing::warn!("voyage: status={status} body={text}");
            return Err(LlmError::Status(status, text));
        }
        parse_embed_batch(&text, texts.len())
    }
}

/// Parse a Voyage batch response body into ordered vectors, enforcing that
/// the provider returned exactly one embedding per input.
fn parse_embed_batch(body: &str, expected: usize) -> Result<Vec<Vec<f32>>, LlmError> {
    let parsed: EmbedResponse = serde_json::from_str(body)
        .map_err(|e| LlmError::Provider(format!("voyage: bad response: {e}")))?;
    if parsed.data.len() != expected {
        return Err(LlmError::Provider(format!(
            "voyage: expected {expected} embeddings, got {}",
            parsed.data.len()
        )));
    }
    Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
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
        let body = r#"{"data":[{"embedding":[1.0,0.0]},{"embedding":[0.0,1.0]}]}"#;
        let out = parse_embed_batch(body, 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![1.0, 0.0]);
        assert_eq!(out[1], vec![0.0, 1.0]);
    }

    #[test]
    fn parse_embed_batch_rejects_count_mismatch() {
        let body = r#"{"data":[{"embedding":[1.0]}]}"#;
        assert!(
            parse_embed_batch(body, 2).is_err(),
            "1 vector for 2 inputs must error"
        );
    }

    #[test]
    fn parse_embed_batch_rejects_garbage() {
        assert!(parse_embed_batch("not json", 1).is_err());
    }
}
