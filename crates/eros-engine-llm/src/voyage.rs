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
}
