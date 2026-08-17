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
            // Not streaming, and no headers are in hand at this call form —
            // no Retry-After to carry.
            let err = LlmError::Status(status, crate::openrouter::parse_error_body(&text), None);
            tracing::warn!("embeddings: {err}");
            return Err(err);
        }
        parse_response(&text, texts.len(), &self.model)
    }
}

/// Parse an OpenRouter-compatible embeddings response: exactly one embedding
/// per input, re-ordered by `index` when present, every vector 512-dim.
fn parse_response(body: &str, expected: usize, model: &str) -> Result<Vec<Vec<f32>>, LlmError> {
    let parsed: WireResponse = serde_json::from_str(body).map_err(|e| {
        LlmError::Provider(crate::openrouter::ParsedErrorBody::message_only(&format!(
            "embeddings: bad response: {e}"
        )))
    })?;
    if parsed.data.len() != expected {
        return Err(LlmError::Provider(
            crate::openrouter::ParsedErrorBody::message_only(&format!(
                "embeddings: expected {expected} embeddings, got {}",
                parsed.data.len()
            )),
        ));
    }
    let mut data = parsed.data;
    // `index` is optional on the wire; when present on all rows, honour it —
    // but only as the exact permutation 0..expected. A duplicate or shifted
    // index set ([0,0], [1,2]) would silently associate vectors with the
    // wrong inputs and persist them; that must be a loud provider error.
    if data.iter().all(|d| d.index.is_some()) {
        data.sort_by_key(|d| d.index.unwrap_or(usize::MAX));
        for (i, d) in data.iter().enumerate() {
            if d.index != Some(i) {
                return Err(LlmError::Provider(
                    crate::openrouter::ParsedErrorBody::message_only(&format!(
                        "embeddings: model {model} returned indices that are not the \
                         permutation 0..{expected} — refusing to associate vectors with \
                         inputs"
                    )),
                ));
            }
        }
    }
    let out: Vec<Vec<f32>> = data.into_iter().map(|d| d.embedding).collect();
    for v in &out {
        if v.len() != EMBEDDING_DIM {
            return Err(LlmError::Provider(
                crate::openrouter::ParsedErrorBody::message_only(&format!(
                    "embeddings: model {model} returned a {}-dim embedding, expected \
                     {EMBEDDING_DIM} (the pgvector schema is VECTOR({EMBEDDING_DIM}))",
                    v.len()
                )),
            ));
        }
    }
    Ok(out)
}

/// One embedding backend: native Voyage (keeps `input_type`) or the
/// OpenRouter-compatible wire (no query/document distinction).
pub(crate) enum EmbedBackend {
    Voyage(crate::voyage::VoyageClient),
    OpenAiCompat(EmbedHttpClient),
}

impl EmbedBackend {
    async fn embed(&self, text: &str, input_type_query: bool) -> Result<Vec<f32>, LlmError> {
        match self {
            EmbedBackend::Voyage(v) if input_type_query => v.embed_query(text).await,
            EmbedBackend::Voyage(v) => v.embed_document(text).await,
            EmbedBackend::OpenAiCompat(c) => c.embed_one(text).await,
        }
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        match self {
            EmbedBackend::Voyage(v) => v.embed_documents(texts).await,
            EmbedBackend::OpenAiCompat(c) => c.embed_batch(texts).await,
        }
    }
}

/// Read/write embedding facade (spec §5.1). `read` serves `embed_query`
/// (the per-turn recall path); `write` serves `embed_document(s)` (the
/// storage paths). Both point at the same backend unless
/// `model_read`/`model_write` split them.
pub struct EmbeddingRouter {
    pub(crate) read: EmbedBackend,
    pub(crate) write: EmbedBackend,
}

impl EmbeddingRouter {
    /// Production constructor: real env. Call after
    /// `ModelConfig::validate_providers` passed.
    pub fn from_config(cfg: &crate::model_config::ModelConfig) -> Result<Self, LlmError> {
        Self::from_config_with(cfg, |k| std::env::var(k).ok())
    }

    /// Testable core: `env` abstracts `std::env::var`.
    pub fn from_config_with(
        cfg: &crate::model_config::ModelConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, LlmError> {
        use crate::model_config::EmbedRoute;
        let resolved = cfg.resolve_embedding();
        let build = |target: &crate::model_config::EmbedTarget| -> Result<EmbedBackend, LlmError> {
            match &target.route {
                EmbedRoute::Voyage => {
                    let key = env("VOYAGE_API_KEY").unwrap_or_default();
                    if key.trim().is_empty() {
                        return Err(LlmError::Config(
                            "VOYAGE_API_KEY is unset or empty but the embedding config \
                             resolves to the built-in Voyage client"
                                .into(),
                        ));
                    }
                    Ok(EmbedBackend::Voyage(
                        crate::voyage::VoyageClient::new(key).with_model(target.model.clone()),
                    ))
                }
                EmbedRoute::OpenRouter => {
                    let key = env("OPENROUTER_API_KEY").unwrap_or_default();
                    if key.is_empty() {
                        return Err(LlmError::Config(
                            "OPENROUTER_API_KEY is unset but [tasks.embedding] routes to \
                             @openrouter"
                                .into(),
                        ));
                    }
                    let entry = cfg.providers.get("openrouter");
                    let url = entry
                        .and_then(|e| e.embeddings.clone())
                        .unwrap_or_else(|| OPENROUTER_EMBEDDINGS_URL.to_string());
                    let headers = entry.map(|e| e.header_map()).unwrap_or_default();
                    Ok(EmbedBackend::OpenAiCompat(EmbedHttpClient::new(
                        url,
                        key,
                        headers,
                        target.model.clone(),
                    )))
                }
                EmbedRoute::Custom(name) => {
                    let entry = cfg.providers.get(name).ok_or_else(|| {
                        LlmError::Config(format!("embedding provider `{name}` is not declared"))
                    })?;
                    let url = entry.embeddings.clone().ok_or_else(|| {
                        LlmError::Config(format!("[providers].{name} declares no `embeddings` URL"))
                    })?;
                    let var = format!("{}_API_KEY", name.to_uppercase());
                    let key = env(&var).unwrap_or_default();
                    if key.is_empty() {
                        return Err(LlmError::Config(format!("${var} is unset or empty")));
                    }
                    Ok(EmbedBackend::OpenAiCompat(EmbedHttpClient::new(
                        url,
                        key,
                        entry.header_map(),
                        target.model.clone(),
                    )))
                }
            }
        };
        Ok(Self {
            read: build(&resolved.read)?,
            write: build(&resolved.write)?,
        })
    }

    /// Embed a recall query (read backend; Voyage `input_type: "query"`).
    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.read.embed(text, true).await
    }

    /// Embed one document for storage (write backend).
    pub async fn embed_document(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.write.embed(text, false).await
    }

    /// Embed a batch of documents for storage (write backend,
    /// order-preserving; empty input short-circuits).
    pub async fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, LlmError> {
        self.write.embed_batch(texts).await
    }
}

#[cfg(test)]
impl EmbeddingRouter {
    pub(crate) fn read_is_voyage(&self) -> bool {
        matches!(self.read, EmbedBackend::Voyage(_))
    }
    pub(crate) fn write_is_voyage(&self) -> bool {
        matches!(self.write, EmbedBackend::Voyage(_))
    }
    pub(crate) fn read_url(&self) -> Option<&str> {
        match &self.read {
            EmbedBackend::OpenAiCompat(c) => Some(c.base_url.as_str()),
            EmbedBackend::Voyage(_) => None,
        }
    }
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
        let crate::error::LlmError::Status(_, body, _) = err else {
            panic!("expected Status, got {err:?}");
        };
        let msg = body.to_string();
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

    #[test]
    fn malformed_indices_are_rejected() {
        // Duplicate ([0,0]) or shifted ([1,2]) index sets pass the count and
        // dim checks but would misassociate vectors with inputs — must error.
        let v = vec![0.0f32; 512];
        for indices in [[0usize, 0], [1, 2]] {
            let body = serde_json::json!({
                "data": [
                    { "embedding": v, "index": indices[0] },
                    { "embedding": v, "index": indices[1] }
                ]
            })
            .to_string();
            let msg = parse_response(&body, 2, "m").unwrap_err().to_string();
            assert!(msg.contains("permutation"), "{indices:?}: {msg}");
        }
    }

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn from_config_defaults_to_voyage() {
        let cfg = crate::model_config::ModelConfig::from_toml_str("").unwrap();
        let router =
            EmbeddingRouter::from_config_with(&cfg, env_with(&[("VOYAGE_API_KEY", "k")])).unwrap();
        // Shape assertion via Debug-free matches: expose #[cfg(test)] accessors.
        assert!(router.read_is_voyage() && router.write_is_voyage());
    }

    #[test]
    fn from_config_missing_voyage_key_errors() {
        let cfg = crate::model_config::ModelConfig::from_toml_str("").unwrap();
        assert!(EmbeddingRouter::from_config_with(&cfg, |_| None).is_err());
    }

    #[test]
    fn from_config_openrouter_route_uses_override_url_and_headers() {
        let cfg = crate::model_config::ModelConfig::from_toml_str(
            "[providers.openrouter]\n\
             embeddings = \"http://proxy/v1/embeddings\"\n\
             headers = { \"HTTP-Referer\" = \"https://eros.example\" }\n\
             [tasks.embedding]\nmodel = \"openai/text-embedding-3-small@openrouter\"\n",
        )
        .unwrap();
        let router =
            EmbeddingRouter::from_config_with(&cfg, env_with(&[("OPENROUTER_API_KEY", "ok")]))
                .unwrap();
        assert_eq!(router.read_url(), Some("http://proxy/v1/embeddings"));
    }

    #[test]
    fn from_config_custom_route_reads_name_key() {
        let cfg = crate::model_config::ModelConfig::from_toml_str(
            "[providers]\nlocal = { embeddings = \"http://e/v1/embeddings\" }\n\
             [tasks.embedding]\nmodel = \"bge-m3@local\"\n",
        )
        .unwrap();
        assert!(
            EmbeddingRouter::from_config_with(&cfg, |_| None).is_err(),
            "missing LOCAL_API_KEY must error"
        );
        assert!(
            EmbeddingRouter::from_config_with(&cfg, env_with(&[("LOCAL_API_KEY", "k")])).is_ok()
        );
    }

    #[tokio::test]
    async fn read_write_dispatch_hits_the_right_backend() {
        // read → voyage-4-lite (query), write → voyage-4 (document), one mock
        // server standing in for Voyage.
        let server = MockServer::start().await;
        Mock::given(path("/v1/embeddings"))
            .and(body_partial_json(
                serde_json::json!({ "model": "voyage-4-lite", "input_type": "query" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [ { "embedding": vec![0.0f32; 512] } ] }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/embeddings"))
            .and(body_partial_json(
                serde_json::json!({ "model": "voyage-4", "input_type": "document" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [ { "embedding": vec![0.0f32; 512] } ] }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/v1/embeddings", server.uri());
        let router = EmbeddingRouter {
            read: EmbedBackend::Voyage(
                crate::voyage::VoyageClient::with_base_url("k".into(), url.clone())
                    .with_model("voyage-4-lite".into()),
            ),
            write: EmbedBackend::Voyage(
                crate::voyage::VoyageClient::with_base_url("k".into(), url)
                    .with_model("voyage-4".into()),
            ),
        };
        router.embed_query("q").await.expect("read ok");
        router.embed_document("d").await.expect("write ok");
    }
}
