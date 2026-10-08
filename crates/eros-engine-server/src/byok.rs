// SPDX-License-Identifier: AGPL-3.0-only
//! Bring-your-own-key chat turns (spec
//! docs/superpowers/specs/2026-10-08-byok-chat-design.md): the request
//! field, its admission and validation, the per-turn client, and the reply
//! chain's hop list.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use eros_engine_llm::model_config::{CompiledRegexRule, ModelSpec};
use eros_engine_llm::openrouter::{ChatRequest, OpenRouterClient};
use eros_engine_llm::provider::{bare_model_id, split_model_slug, ProviderEndpoint};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, StreamPreError};
use crate::state::{AppState, ByokConfig};

/// Process-wide BYOK state: the guarded HTTP client (spec §7.2) and the
/// per-user round-robin cursors (§6.1). Cursors live per process and reset
/// on restart, like config round-robin.
#[derive(Clone)]
pub(crate) struct ByokRuntime {
    http: reqwest::Client,
    cursors: Arc<Mutex<HashMap<Uuid, Arc<AtomicUsize>>>>,
}

impl ByokRuntime {
    pub(crate) fn new(allow_private_network: bool) -> Self {
        Self {
            http: eros_engine_llm::byok::build_http(allow_private_network),
            cursors: Arc::default(),
        }
    }

    /// This user's round-robin cursor, created at 0 on first use.
    pub(crate) fn cursor(&self, user_id: Uuid) -> Arc<AtomicUsize> {
        self.cursors
            .lock()
            .unwrap()
            .entry(user_id)
            .or_default()
            .clone()
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

/// Request header carrying the deployment's caller secret (spec §5).
pub(crate) const CALLER_SECRET_HEADER: &str = "x-byok-caller-secret";

const MAX_PROVIDERS: usize = 4;
const MAX_PROVIDER_NAME_LEN: usize = 32;
const MAX_URL_LEN: usize = 2048;
const MAX_KEY_LEN: usize = 1024;
const MAX_HEADERS: usize = 8;
const MAX_HEADER_VALUE_LEN: usize = 1024;
const MAX_MODELS: usize = 8;
const MAX_SLUG_LEN: usize = 256;
const MAX_REGEX_RULES: usize = 16;
const MAX_PATTERN_CHARS: usize = 512;
const MAX_REPLACEMENT_CHARS: usize = 512;
const DEFAULT_RETRY_DEPTH: u32 = 2;

/// The `byok` field of a stream send (spec §4.1). Settings BYOK does not
/// take are refused, not ignored.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ByokRequest {
    /// The end user's endpoints, by a name the model slugs reference.
    pub providers: BTreeMap<String, ByokProvider>,
    /// Primary model: one slug, an array (round-robin), or a slug → weight
    /// map (weighted random). Every slug ends in `@<provider name>`.
    pub model: ByokModel,
    /// Sequential fallback chain.
    #[serde(default)]
    pub fallback: Option<ByokFallback>,
    /// Fallbacks tried after the primary. Default 2.
    #[serde(default)]
    pub retry_depth: Option<u32>,
    /// Strip rules for this chain's replies.
    #[serde(default)]
    pub output_regex: Option<Vec<ByokRegexRule>>,
    /// Continue on the deployment's own chain when every BYOK hop fails.
    #[serde(default)]
    pub fallback_to_platform: bool,
}

impl std::fmt::Debug for ByokRequest {
    /// Provider names and the model shape only: no URL, key or header value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shape = match self.model {
            ByokModel::Fixed(_) => "fixed",
            ByokModel::RoundRobin(_) => "round_robin",
            ByokModel::Weighted(_) => "weighted",
        };
        f.debug_struct("ByokRequest")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("model", &shape)
            .field("fallback_to_platform", &self.fallback_to_platform)
            .finish_non_exhaustive()
    }
}

/// One end-user endpoint.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ByokProvider {
    /// Complete chat-completions URL, posted verbatim.
    pub chat: String,
    /// Sent as `Authorization: Bearer <api_key>`.
    pub api_key: String,
    /// Sent verbatim on every request to this provider.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum ByokModel {
    Fixed(String),
    RoundRobin(Vec<String>),
    Weighted(BTreeMap<String, f64>),
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum ByokFallback {
    One(String),
    Many(Vec<String>),
}

/// One `output_regex` rule. A rule never lengthens a reply.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ByokRegexRule {
    /// Regex matched against the reply.
    pub pattern: String,
    /// Literal text put in place of each match; default empty. Contains no
    /// `$` and is no longer in bytes than the shortest text `pattern` can
    /// match.
    #[serde(default)]
    pub replacement: Option<String>,
    /// Bare model ids the rule applies to (`gpt-x`, not `gpt-x@mine`).
    /// Absent ⇒ every model on the BYOK chain.
    #[serde(default)]
    pub models: Option<Vec<String>>,
}

/// What the queue row keeps of a BYOK turn (spec §9.1): that it asked for
/// BYOK, and what to do when it cannot be served. Nothing else.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct QueuedByok {
    pub fallback_to_platform: bool,
}

/// One turn's BYOK configuration, validated and ready to drive (spec §6).
/// It lives only in the turn's memory.
pub(crate) struct ByokTurn {
    client: OpenRouterClient,
    model: ModelSpec,
    fallback: Vec<String>,
    retry_depth: u32,
    rules: Vec<CompiledRegexRule>,
    pub(crate) fallback_to_platform: bool,
}

impl std::fmt::Debug for ByokTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByokTurn")
            .field("fallbacks", &self.fallback.len())
            .field("rules", &self.rules.len())
            .field("fallback_to_platform", &self.fallback_to_platform)
            .finish_non_exhaustive()
    }
}

impl ByokTurn {
    pub(crate) fn client(&self) -> &OpenRouterClient {
        &self.client
    }

    pub(crate) fn rules(&self) -> &[CompiledRegexRule] {
        &self.rules
    }

    /// The BYOK half of the chain: the selected primary, then the fallbacks
    /// without it, cut to `retry_depth`. Advances the user's round-robin
    /// cursor — call once per reply.
    fn select_slugs(&self) -> Vec<String> {
        let primary = self.model.select().expect("validated: model is non-empty");
        let mut fallback: Vec<String> = self
            .fallback
            .iter()
            .filter(|m| **m != primary)
            .cloned()
            .collect();
        fallback.truncate(self.retry_depth as usize);
        std::iter::once(primary).chain(fallback).collect()
    }
}

/// One hop of the reply chain (spec §6.2).
#[derive(Debug, Clone)]
pub(crate) struct ChatHop {
    /// What the client routes on.
    pub slug: String,
    /// What audit, logs and failure records name: the slug on a platform
    /// hop, `<id>@byok` on a BYOK hop.
    pub audit: String,
    /// The turn's BYOK configuration on a BYOK hop; `None` on a platform hop.
    pub byok: Option<Arc<ByokTurn>>,
}

impl ChatHop {
    pub(crate) fn client<'a>(&'a self, platform: &'a OpenRouterClient) -> &'a OpenRouterClient {
        self.byok.as_deref().map_or(platform, ByokTurn::client)
    }

    pub(crate) fn rules<'a>(
        &'a self,
        platform: &'a [CompiledRegexRule],
    ) -> &'a [CompiledRegexRule] {
        self.byok.as_deref().map_or(platform, ByokTurn::rules)
    }
}

/// The reply chain: the platform chain from `req`, or the BYOK chain
/// followed by the platform chain when `fallback_to_platform`. Selects the
/// BYOK primary, so call once per reply.
pub(crate) fn reply_chain(req: &ChatRequest, byok: Option<&Arc<ByokTurn>>) -> Vec<ChatHop> {
    let platform = std::iter::once(&req.model)
        .chain(req.fallback_model.iter())
        .filter(|s| !s.is_empty())
        .map(|s| ChatHop {
            slug: s.clone(),
            audit: s.clone(),
            byok: None,
        });
    let Some(b) = byok else {
        return platform.collect();
    };
    let byok_hops = b.select_slugs().into_iter().map(|slug| ChatHop {
        audit: eros_engine_llm::byok::audit_slug(&slug),
        slug,
        byok: Some(b.clone()),
    });
    if b.fallback_to_platform {
        byok_hops.chain(platform).collect()
    } else {
        byok_hops.collect()
    }
}

/// Spec §5: `byok` is honoured only from a caller holding the deployment's
/// secret. `Err` carries the 403 message.
pub(crate) fn admit(cfg: &ByokConfig, headers: &HeaderMap) -> Result<(), &'static str> {
    let Some(secret) = cfg.caller_secret.as_deref() else {
        return Err("byok is not enabled on this deployment");
    };
    let sent = headers
        .get(CALLER_SECRET_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if constant_time_eq(sent, secret.as_bytes()) {
        Ok(())
    } else {
        Err("byok caller secret missing or wrong")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The route-side entry: admission (403) then validation (400), both before
/// any row is written (spec §4.2, §5).
pub(crate) fn admit_and_prepare(
    state: &AppState,
    headers: &HeaderMap,
    req: &ByokRequest,
    user_id: Uuid,
) -> Result<ByokTurn, AppError> {
    admit(&state.config.byok, headers)
        .map_err(|m| pre_error(StatusCode::FORBIDDEN, "byok_forbidden", m.to_string()))?;
    prepare(req, &state.config.byok, &state.byok, user_id)
        .map_err(|m| pre_error(StatusCode::BAD_REQUEST, "invalid_payload", m))
}

fn pre_error(status: StatusCode, code: &'static str, message: String) -> AppError {
    AppError::StreamPre(StreamPreError {
        status,
        code,
        message,
        user_message: "请求无效".into(),
        original_user_message_id: None,
    })
}

/// Validate `req` (spec §4.2) and build the turn. `Err` carries the 400
/// message: it names the offending field and never a key, a header value or
/// a URL.
pub(crate) fn prepare(
    req: &ByokRequest,
    cfg: &ByokConfig,
    rt: &ByokRuntime,
    user_id: Uuid,
) -> Result<ByokTurn, String> {
    if req.providers.is_empty() || req.providers.len() > MAX_PROVIDERS {
        return Err(format!(
            "byok.providers: 1..={MAX_PROVIDERS} entries required"
        ));
    }
    let mut endpoints = HashMap::with_capacity(req.providers.len());
    for (name, p) in &req.providers {
        if !valid_provider_name(name) {
            return Err(format!(
                "byok.providers: names must match [a-z0-9_]{{1,{MAX_PROVIDER_NAME_LEN}}} \
                 and must not be `openrouter`"
            ));
        }
        let scope = format!("byok.providers.{name}");
        check_url(&scope, &p.chat, cfg.allow_private_network)?;
        endpoints.insert(name.clone(), endpoint(&scope, p)?);
    }

    let primary: Vec<&String> = match &req.model {
        ByokModel::Fixed(s) => vec![s],
        ByokModel::RoundRobin(v) => v.iter().collect(),
        ByokModel::Weighted(m) => m.keys().collect(),
    };
    check_count("byok.model", primary.len())?;
    for s in &primary {
        check_slug("byok.model", s, &req.providers)?;
    }
    if let ByokModel::Weighted(m) = &req.model {
        if m.values().any(|w| !(w.is_finite() && *w > 0.0)) || !m.values().sum::<f64>().is_finite()
        {
            return Err(
                "byok.model: weights must be finite, greater than 0, and sum to a finite total"
                    .into(),
            );
        }
    }

    let fallback: Vec<String> = match &req.fallback {
        None => Vec::new(),
        Some(ByokFallback::One(s)) => vec![s.clone()],
        Some(ByokFallback::Many(v)) => v.clone(),
    };
    if fallback.len() > MAX_MODELS {
        return Err(format!("byok.fallback: at most {MAX_MODELS} entries"));
    }
    for s in &fallback {
        check_slug("byok.fallback", s, &req.providers)?;
    }

    let mut chain_ids: Vec<String> = primary
        .iter()
        .map(|s| bare_model_id(s))
        .chain(fallback.iter().map(|s| bare_model_id(s)))
        .collect();
    chain_ids.sort();
    chain_ids.dedup();
    let rules = compile_rules(req.output_regex.as_deref().unwrap_or_default(), &chain_ids)?;

    let model = match &req.model {
        ByokModel::Fixed(s) => ModelSpec::Fixed(s.clone()),
        ByokModel::RoundRobin(v) => ModelSpec::RoundRobin {
            models: v.clone(),
            cursor: rt.cursor(user_id),
        },
        ByokModel::Weighted(m) => {
            ModelSpec::Weighted(m.iter().map(|(k, w)| (k.clone(), *w)).collect())
        }
    };

    Ok(ByokTurn {
        client: OpenRouterClient::byok(rt.http().clone(), endpoints),
        model,
        fallback,
        retry_depth: req.retry_depth.unwrap_or(DEFAULT_RETRY_DEPTH),
        rules,
        fallback_to_platform: req.fallback_to_platform,
    })
}

/// `openrouter` is refused: `x@openrouter` routes to the built-in endpoint.
fn valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROVIDER_NAME_LEN
        && name != "openrouter"
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Spec §4.2 / §7.2. reqwest skips the resolver for a literal-IP host, so
/// the address guard runs here for those. Messages never contain the URL.
fn check_url(scope: &str, raw: &str, allow_private: bool) -> Result<(), String> {
    if raw.len() > MAX_URL_LEN {
        return Err(format!("{scope}.chat: longer than {MAX_URL_LEN} bytes"));
    }
    let url = reqwest::Url::parse(raw).map_err(|_| format!("{scope}.chat: not a valid URL"))?;
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(format!("{scope}.chat: scheme must be https")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{scope}.chat: credentials inside the URL are not accepted"
        ));
    }
    let Some(host) = url.host_str() else {
        return Err(format!("{scope}.chat: URL has no host"));
    };
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        if !allow_private && !eros_engine_llm::byok::address_allowed(ip) {
            return Err(format!("{scope}.chat: destination address not allowed"));
        }
    }
    Ok(())
}

fn endpoint(scope: &str, p: &ByokProvider) -> Result<ProviderEndpoint, String> {
    if p.api_key.is_empty()
        || p.api_key.len() > MAX_KEY_LEN
        || reqwest::header::HeaderValue::from_str(&p.api_key).is_err()
    {
        return Err(format!(
            "{scope}.api_key: 1..={MAX_KEY_LEN} bytes of valid header text required"
        ));
    }
    let headers = p.headers.clone().unwrap_or_default();
    if headers.len() > MAX_HEADERS {
        return Err(format!("{scope}.headers: at most {MAX_HEADERS} entries"));
    }
    if let Some(name) = headers
        .iter()
        .find(|(_, v)| v.len() > MAX_HEADER_VALUE_LEN)
        .map(|(n, _)| n)
    {
        return Err(format!(
            "{scope}.headers: value for `{name}` exceeds {MAX_HEADER_VALUE_LEN} bytes"
        ));
    }
    eros_engine_llm::model_config::check_header_pairs(&format!("{scope}.headers"), &headers)?;
    let mut map = reqwest::header::HeaderMap::with_capacity(headers.len());
    for (n, v) in &headers {
        map.insert(
            reqwest::header::HeaderName::from_bytes(n.as_bytes())
                .expect("checked by check_header_pairs"),
            reqwest::header::HeaderValue::from_str(v).expect("checked by check_header_pairs"),
        );
    }
    Ok(ProviderEndpoint {
        base_url: p.chat.clone(),
        api_key: p.api_key.clone(),
        headers: map,
        body_rules: Vec::new(),
    })
}

fn check_count(field: &str, n: usize) -> Result<(), String> {
    if n == 0 || n > MAX_MODELS {
        Err(format!("{field}: 1..={MAX_MODELS} entries required"))
    } else {
        Ok(())
    }
}

fn check_slug(
    field: &str,
    slug: &str,
    providers: &BTreeMap<String, ByokProvider>,
) -> Result<(), String> {
    if slug.len() > MAX_SLUG_LEN {
        return Err(format!(
            "{field}: every model is at most {MAX_SLUG_LEN} bytes"
        ));
    }
    match split_model_slug(slug) {
        Ok((id, Some(p))) if !id.is_empty() && providers.contains_key(p) => Ok(()),
        _ => Err(format!(
            "{field}: every model must end in @<name> naming one of byok.providers"
        )),
    }
}

fn compile_rules(
    rules: &[ByokRegexRule],
    chain_ids: &[String],
) -> Result<Vec<CompiledRegexRule>, String> {
    if rules.len() > MAX_REGEX_RULES {
        return Err(format!(
            "byok.output_regex: at most {MAX_REGEX_RULES} rules"
        ));
    }
    rules
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let at = format!("byok.output_regex[{i}]");
            if r.pattern.is_empty() || r.pattern.chars().count() > MAX_PATTERN_CHARS {
                return Err(format!(
                    "{at}.pattern: 1..={MAX_PATTERN_CHARS} chars required"
                ));
            }
            let replacement = r.replacement.clone().unwrap_or_default();
            if replacement.chars().count() > MAX_REPLACEMENT_CHARS {
                return Err(format!(
                    "{at}.replacement: at most {MAX_REPLACEMENT_CHARS} chars"
                ));
            }
            if let Some(models) = &r.models {
                if models.len() > MAX_MODELS || models.iter().any(|m| m.len() > MAX_SLUG_LEN) {
                    return Err(format!(
                        "{at}.models: at most {MAX_MODELS} entries of at most {MAX_SLUG_LEN} bytes"
                    ));
                }
            }
            let regex = eros_engine_llm::byok::compile_pattern(&r.pattern)
                .map_err(|e| format!("{at}.pattern: {e}"))?;
            // Rules run in sequence on each other's output, so a rule that
            // could lengthen a reply would compound (spec §4.2).
            if !eros_engine_llm::byok::replacement_is_non_growing(&r.pattern, &replacement) {
                return Err(format!(
                    "{at}.replacement: must contain no `$` and be no longer in bytes \
                     than the shortest text the pattern can match"
                ));
            }
            Ok(CompiledRegexRule {
                models: r.models.clone().unwrap_or_else(|| chain_ids.to_vec()),
                regex,
                replacement,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    const KEY: &str = "sk-SECRET-123";
    const HOST: &str = "api.example.com";

    fn cfg(allow_private: bool) -> ByokConfig {
        ByokConfig {
            caller_secret: Some("s".repeat(32)),
            allow_private_network: allow_private,
        }
    }

    fn base() -> serde_json::Value {
        json!({
            "providers": { "mine": { "chat": format!("https://{HOST}/v1/chat/completions"), "api_key": KEY } },
            "model": "gpt-x@mine",
        })
    }

    fn set(mut v: serde_json::Value, path: &[&str], value: serde_json::Value) -> serde_json::Value {
        let mut cur = &mut v;
        for p in &path[..path.len() - 1] {
            cur = cur.get_mut(*p).expect("path exists");
        }
        cur[path[path.len() - 1]] = value;
        v
    }

    fn parse(v: serde_json::Value) -> ByokRequest {
        serde_json::from_value(v).expect("parses")
    }

    fn try_prepare(v: serde_json::Value) -> Result<ByokTurn, String> {
        prepare(
            &parse(v),
            &cfg(false),
            &ByokRuntime::new(false),
            Uuid::new_v4(),
        )
    }

    fn rejects(v: serde_json::Value, needle: &str) {
        let err = try_prepare(v).expect_err("must be rejected");
        assert!(err.contains(needle), "expected `{needle}` in `{err}`");
        assert!(
            !err.contains(KEY) && !err.contains(HOST),
            "message leaks: {err}"
        );
    }

    #[test]
    fn cursors_are_per_user() {
        let rt = ByokRuntime::new(false);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        rt.cursor(a).fetch_add(1, Ordering::Relaxed);
        assert_eq!(rt.cursor(a).load(Ordering::Relaxed), 1);
        assert_eq!(rt.cursor(b).load(Ordering::Relaxed), 0);
    }

    #[test]
    fn admit_requires_the_secret() {
        let secret = "s".repeat(32);
        let on = ByokConfig {
            caller_secret: Some(secret.clone()),
            allow_private_network: false,
        };
        let mut h = HeaderMap::new();
        assert!(admit(&ByokConfig::default(), &h).is_err(), "feature off");
        assert!(admit(&on, &h).is_err(), "header missing");
        h.insert(CALLER_SECRET_HEADER, "wrong".parse().unwrap());
        assert!(admit(&on, &h).is_err(), "header wrong");
        h.insert(CALLER_SECRET_HEADER, secret.parse().unwrap());
        assert!(admit(&on, &h).is_ok());
    }

    #[test]
    fn a_minimal_request_prepares() {
        let turn = try_prepare(base()).expect("valid");
        assert_eq!(turn.select_slugs(), vec!["gpt-x@mine".to_string()]);
        assert!(!turn.fallback_to_platform);
    }

    #[test]
    fn slugs_must_name_a_byok_provider() {
        for slug in ["gpt-x", "gpt-x@openrouter", "gpt-x@other", "@mine", ""] {
            rejects(set(base(), &["model"], json!(slug)), "@<name>");
        }
        rejects(set(base(), &["fallback"], json!(["b"])), "byok.fallback");
    }

    #[test]
    fn provider_names_are_restricted() {
        for name in ["Mine", "openrouter", "a-b", &"a".repeat(33)] {
            let v = json!({
                "providers": { name: { "chat": format!("https://{HOST}/v1"), "api_key": KEY } },
                "model": format!("gpt-x@{name}"),
            });
            rejects(v, "byok.providers");
        }
        rejects(set(base(), &["providers"], json!({})), "byok.providers");
        let five: serde_json::Map<String, serde_json::Value> = (0..5)
            .map(|i| {
                (
                    format!("p{i}"),
                    json!({ "chat": format!("https://{HOST}/v1"), "api_key": KEY }),
                )
            })
            .collect();
        rejects(
            set(base(), &["providers"], serde_json::Value::Object(five)),
            "byok.providers",
        );
    }

    #[test]
    fn urls_must_be_https_public_and_credential_free() {
        let long = format!("https://{HOST}/{}", "a".repeat(2048));
        for url in [
            "http://api.example.com/v1",
            "ftp://api.example.com/v1",
            "not a url",
            "https://user:pw@api.example.com/v1",
            "https://10.0.0.1/v1",
            "https://127.0.0.1/v1",
            "https://[fd00::1]/v1",
            "https://[::ffff:10.0.0.1]/v1",
            "https://0x7f.0.0.1/v1",
            "https://2130706433/v1",
            long.as_str(),
        ] {
            rejects(
                set(base(), &["providers", "mine", "chat"], json!(url)),
                ".chat",
            );
        }
    }

    #[test]
    fn private_network_opt_in_allows_http_and_private_hosts() {
        let v = set(
            base(),
            &["providers", "mine", "chat"],
            json!("http://127.0.0.1:8080/v1"),
        );
        assert!(prepare(
            &parse(v),
            &cfg(true),
            &ByokRuntime::new(true),
            Uuid::new_v4()
        )
        .is_ok());
    }

    #[test]
    fn keys_and_headers_are_checked() {
        rejects(
            set(base(), &["providers", "mine", "api_key"], json!("")),
            ".api_key",
        );
        rejects(
            set(
                base(),
                &["providers", "mine", "api_key"],
                json!("k".repeat(1025)),
            ),
            ".api_key",
        );
        rejects(
            set(base(), &["providers", "mine", "api_key"], json!("sk\nx")),
            ".api_key",
        );
        rejects(
            set(
                base(),
                &["providers", "mine", "headers"],
                json!({"Authorization": "x"}),
            ),
            "engine-owned",
        );
        let nine: serde_json::Map<String, serde_json::Value> =
            (0..9).map(|i| (format!("x-h{i}"), json!("v"))).collect();
        rejects(
            set(
                base(),
                &["providers", "mine", "headers"],
                serde_json::Value::Object(nine),
            ),
            ".headers",
        );
        rejects(
            set(
                base(),
                &["providers", "mine", "headers"],
                json!({"x-h": "v".repeat(1025)}),
            ),
            ".headers",
        );
    }

    #[test]
    fn model_shapes_and_limits() {
        assert!(try_prepare(set(base(), &["model"], json!(["a@mine", "b@mine"]))).is_ok());
        assert!(try_prepare(set(base(), &["model"], json!({"a@mine": 3, "b@mine": 1}))).is_ok());
        rejects(set(base(), &["model"], json!([])), "byok.model");
        let nine: Vec<String> = (0..9).map(|i| format!("m{i}@mine")).collect();
        rejects(set(base(), &["model"], json!(nine)), "byok.model");
        rejects(set(base(), &["model"], json!({"a@mine": 0.0})), "weights");
        rejects(set(base(), &["model"], json!({"a@mine": -1.0})), "weights");
        rejects(
            set(
                base(),
                &["model"],
                json!({"a@mine": 1e308, "b@mine": 1e308}),
            ),
            "weights",
        );
        let ok = try_prepare(set(base(), &["model"], json!({"a@mine": 3, "b@mine": 1}))).unwrap();
        assert_eq!(ok.select_slugs().len(), 1);
    }

    #[test]
    fn unknown_fields_are_refused() {
        for field in [
            "allow_traits",
            "temperature",
            "model_name_display_override",
            "max_tokens",
        ] {
            let v = set(base(), &[field], json!(1));
            assert!(
                serde_json::from_value::<ByokRequest>(v).is_err(),
                "{field} must be refused"
            );
        }
        let v = set(base(), &["providers", "mine", "body"], json!({}));
        assert!(serde_json::from_value::<ByokRequest>(v).is_err());
    }

    #[test]
    fn regex_rules_are_bounded_and_compiled() {
        let seventeen: Vec<serde_json::Value> = (0..17).map(|_| json!({"pattern": "x"})).collect();
        rejects(
            set(base(), &["output_regex"], json!(seventeen)),
            "output_regex",
        );
        rejects(
            set(base(), &["output_regex"], json!([{"pattern": ""}])),
            ".pattern",
        );
        rejects(
            set(
                base(),
                &["output_regex"],
                json!([{"pattern": "x".repeat(513)}]),
            ),
            ".pattern",
        );
        rejects(
            set(base(), &["output_regex"], json!([{"pattern": "("}])),
            ".pattern",
        );
        rejects(
            set(
                base(),
                &["output_regex"],
                json!([{"pattern": r"(?:\w{1000}){1000}"}]),
            ),
            ".pattern",
        );
        rejects(
            set(
                base(),
                &["output_regex"],
                json!([{"pattern": "x", "replacement": "y".repeat(513)}]),
            ),
            ".replacement",
        );

        let turn = try_prepare(set(
            base(),
            &["output_regex"],
            json!([{"pattern": "a"}, {"pattern": "b", "models": ["other"]}]),
        ))
        .unwrap();
        assert_eq!(turn.rules()[0].models, vec!["gpt-x".to_string()]);
        assert_eq!(turn.rules()[1].models, vec!["other".to_string()]);
    }

    #[test]
    fn regex_rules_cannot_lengthen_a_reply() {
        let rule = |pattern: &str, replacement: &str| {
            set(
                base(),
                &["output_regex"],
                json!([{"pattern": pattern, "replacement": replacement}]),
            )
        };
        rejects(rule("(?:)", "x"), "byok.output_regex[0].replacement");
        rejects(rule("abcdef", "$0"), "byok.output_regex[0].replacement");
        rejects(rule("(abcdef)", "$1"), "byok.output_regex[0].replacement");
        rejects(rule("x", "yy"), "byok.output_regex[0].replacement");
        assert!(try_prepare(rule("ab", "x")).is_ok());
        assert!(try_prepare(rule("(?:)", "")).is_ok());
        let err = try_prepare(rule("x", "ECHOED-REPLACEMENT")).expect_err("grows");
        assert!(!err.contains("ECHOED-REPLACEMENT"), "{err}");
    }

    #[test]
    fn regex_rule_models_are_bounded() {
        let models = |m: serde_json::Value| {
            set(
                base(),
                &["output_regex"],
                json!([{"pattern": "x", "models": m}]),
            )
        };
        let nine: Vec<String> = (0..9).map(|i| format!("m{i}")).collect();
        rejects(models(json!(nine)), "byok.output_regex[0].models");
        rejects(
            models(json!(["m".repeat(257)])),
            "byok.output_regex[0].models",
        );
        assert!(try_prepare(models(json!(["m".repeat(256)]))).is_ok());
    }

    #[test]
    fn slugs_are_at_most_256_bytes() {
        let slug = |len: usize| format!("{}@mine", "a".repeat(len - "@mine".len()));
        assert!(try_prepare(set(base(), &["model"], json!(slug(256)))).is_ok());
        rejects(set(base(), &["model"], json!(slug(257))), "byok.model");
        rejects(
            set(base(), &["fallback"], json!([slug(257)])),
            "byok.fallback",
        );
    }

    #[test]
    fn round_robin_rotates_per_user() {
        let rt = ByokRuntime::new(false);
        let req = parse(set(base(), &["model"], json!(["a@mine", "b@mine"])));
        let (u1, u2) = (Uuid::new_v4(), Uuid::new_v4());
        let first = |u| prepare(&req, &cfg(false), &rt, u).unwrap().select_slugs()[0].clone();
        assert_eq!(first(u1), "a@mine");
        assert_eq!(first(u1), "b@mine");
        assert_eq!(first(u2), "a@mine");
    }

    #[test]
    fn select_slugs_drops_the_primary_and_truncates() {
        let v = set(
            set(base(), &["model"], json!("a@mine")),
            &["fallback"],
            json!(["a@mine", "b@mine", "c@mine", "d@mine"]),
        );
        let turn = try_prepare(set(v, &["retry_depth"], json!(2))).unwrap();
        assert_eq!(turn.select_slugs(), vec!["a@mine", "b@mine", "c@mine"]);
    }

    #[test]
    fn reply_chain_lays_out_the_hops() {
        let req = ChatRequest {
            model: "p/x".into(),
            fallback_model: vec!["p/y".into()],
            ..Default::default()
        };
        let none = reply_chain(&req, None);
        assert_eq!(
            none.iter()
                .map(|h| (h.slug.as_str(), h.audit.as_str(), h.byok.is_some()))
                .collect::<Vec<_>>(),
            vec![("p/x", "p/x", false), ("p/y", "p/y", false)]
        );

        let only = Arc::new(try_prepare(base()).unwrap());
        let hops = reply_chain(&req, Some(&only));
        assert_eq!(
            hops.iter()
                .map(|h| (h.slug.as_str(), h.audit.as_str(), h.byok.is_some()))
                .collect::<Vec<_>>(),
            vec![("gpt-x@mine", "gpt-x@byok", true)]
        );

        let then =
            Arc::new(try_prepare(set(base(), &["fallback_to_platform"], json!(true))).unwrap());
        let hops = reply_chain(&req, Some(&then));
        assert_eq!(
            hops.iter()
                .map(|h| (h.slug.as_str(), h.byok.is_some()))
                .collect::<Vec<_>>(),
            vec![("gpt-x@mine", true), ("p/x", false), ("p/y", false)]
        );
    }

    #[test]
    fn debug_never_prints_secrets() {
        let v = set(
            base(),
            &["providers", "mine", "headers"],
            json!({"x-org": "HEADER-SECRET"}),
        );
        let req = parse(v.clone());
        let turn = try_prepare(v).unwrap();
        for dbg in [format!("{req:?}"), format!("{turn:?}")] {
            assert!(
                !dbg.contains(KEY) && !dbg.contains(HOST) && !dbg.contains("HEADER-SECRET"),
                "{dbg}"
            );
        }
    }
}
