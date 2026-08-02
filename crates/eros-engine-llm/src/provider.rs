// SPDX-License-Identifier: AGPL-3.0-only
//! Model-slug → provider routing primitives.
//!
//! Grammar (spec §2, applied AFTER TOML parsing): at most one unescaped `@`
//! splits `<model id>@<provider name>`; the two-byte sequence `\@` escapes a
//! literal `@` inside a model id. No `@` ⇒ built-in OpenRouter.

use std::fmt;

/// One custom OpenAI-compatible endpoint from the `[providers]` block.
/// `base_url` is the complete chat-completions URL, posted verbatim;
/// `api_key` comes from the `<NAME>_API_KEY` env var at boot. `headers` is
/// the entry's config-declared `headers` table, applied per-request.
///
/// `Eq` is dropped (not derived) because `HeaderMap` doesn't implement it —
/// `PartialEq` is enough for the existing equality-based tests.
#[derive(Clone, PartialEq)]
pub struct ProviderEndpoint {
    pub base_url: String,
    pub api_key: String,
    /// Config-declared custom headers, applied per-request. Empty by default.
    pub headers: reqwest::header::HeaderMap,
    /// `[[providers.<name>.body]]` rules, applied per-request to chat bodies.
    /// Empty by default.
    pub body_rules: Vec<crate::model_config::BodyRule>,
}

impl fmt::Debug for ProviderEndpoint {
    /// Manual impl (not `derive`): `api_key` must never land in a log line or
    /// a panic message via `{:?}`, so it's redacted here rather than relying
    /// on every caller to remember not to print the field directly. Header
    /// VALUES may be sensitive too (e.g. a proxy auth token), so the whole
    /// `headers` map is redacted rather than printed key-by-key. `params`
    /// are deployer config, not secrets, so `body_rules` is kept quiet as a
    /// count rather than redacted outright.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderEndpoint")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("headers", &"<redacted>")
            .field("body_rules", &self.body_rules.len())
            .finish()
    }
}

/// Why a model slug failed to parse. Carries the offending slug so boot
/// errors and `LlmError::Config` messages are self-locating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    /// More than one unescaped `@`.
    MultipleAt { slug: String },
    /// Nothing before the separator (`@venice`).
    EmptyModelId { slug: String },
    /// Nothing after the separator (`x@`).
    EmptyProvider { slug: String },
}

impl fmt::Display for SlugError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SlugError::MultipleAt { slug } => write!(
                f,
                "model slug `{slug}` contains more than one unescaped `@` — a literal `@` \
                 inside a model id must be escaped as `\\@` (in a TOML double-quoted \
                 string, write `\"\\\\@\"`)"
            ),
            SlugError::EmptyModelId { slug } => {
                write!(
                    f,
                    "model slug `{slug}` has an empty model id before `@` — if the model id \
                     itself is meant to start with a literal `@` (e.g. an OpenRouter preset \
                     like `@preset/roleplay`), escape it as `\\@` (in a TOML double-quoted \
                     string, write `\"\\\\@\"`)"
                )
            }
            SlugError::EmptyProvider { slug } => {
                write!(
                    f,
                    "model slug `{slug}` has an empty provider name after `@` — if the trailing \
                     `@` is meant to be a literal character in the model id, escape it as `\\@` \
                     (in a TOML double-quoted string, write `\"\\\\@\"`)"
                )
            }
        }
    }
}

impl std::error::Error for SlugError {}

/// Split a config model slug into (unescaped model id, optional provider
/// name). `None` provider ⇒ the built-in OpenRouter client. Only the two-byte
/// sequence `\@` is an escape; every other backslash is an ordinary model-id
/// byte, so a trailing lone `\` stays in the id.
pub fn split_model_slug(slug: &str) -> Result<(String, Option<&str>), SlugError> {
    // `@` and `\` are ASCII, so byte scanning is UTF-8-safe and the split
    // index below is always a char boundary.
    let bytes = slug.as_bytes();
    let mut ats = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'@' && (i == 0 || bytes[i - 1] != b'\\') {
            ats.push(i);
        }
    }
    let unescape = |s: &str| s.replace("\\@", "@");
    match ats.as_slice() {
        [] => Ok((unescape(slug), None)),
        [i] => {
            let (id_raw, rest) = slug.split_at(*i);
            let provider = &rest[1..];
            if id_raw.is_empty() {
                return Err(SlugError::EmptyModelId {
                    slug: slug.to_string(),
                });
            }
            if provider.is_empty() {
                return Err(SlugError::EmptyProvider {
                    slug: slug.to_string(),
                });
            }
            Ok((unescape(id_raw), Some(provider)))
        }
        _ => Err(SlugError::MultipleAt {
            slug: slug.to_string(),
        }),
    }
}

/// Escape literal `@` bytes in a model id so the result round-trips through
/// [`split_model_slug`] when a `@provider` suffix is appended. Inverse of the
/// unescape performed by `split_model_slug`; identity for ids without `@`.
pub fn escape_model_id(id: &str) -> String {
    id.replace('@', "\\@")
}

/// The suffix-stripped, unescaped model id — what model-keyed config tables
/// (`display` / `output_regex` / `output_filter.trigger`) and the wire `model`
/// field use. Falls back to the raw string on invalid grammar: post-boot,
/// config-supplied slugs are always valid, so this only fires on
/// client-supplied strings, which were never routable anyway.
pub fn bare_model_id(slug: &str) -> String {
    match split_model_slug(slug) {
        Ok((id, _)) => id,
        Err(_) => slug.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_endpoint_debug_redacts_api_key() {
        let ep = ProviderEndpoint {
            base_url: "https://api.venice.ai/chat/completions".to_string(),
            api_key: "sk-live-SUPER-SECRET-KEY".to_string(),
            headers: reqwest::header::HeaderMap::new(),
            body_rules: Vec::new(),
        };
        let dbg = format!("{ep:?}");
        assert!(
            !dbg.contains("SUPER-SECRET-KEY"),
            "Debug output must not leak api_key: {dbg}"
        );
    }

    #[test]
    fn provider_endpoint_debug_redacts_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-proxy-secret",
            reqwest::header::HeaderValue::from_static("SUPER-SECRET-HEADER-VALUE"),
        );
        let ep = ProviderEndpoint {
            base_url: "https://api.venice.ai/chat/completions".to_string(),
            api_key: "sk-live-key".to_string(),
            headers,
            body_rules: Vec::new(),
        };
        let dbg = format!("{ep:?}");
        assert!(
            !dbg.contains("SUPER-SECRET-HEADER-VALUE"),
            "Debug output must not leak header values: {dbg}"
        );
    }

    #[test]
    fn no_at_is_openrouter_verbatim() {
        assert_eq!(
            split_model_slug("x-ai/grok-4.20").unwrap(),
            ("x-ai/grok-4.20".to_string(), None)
        );
    }

    #[test]
    fn one_at_splits_provider() {
        assert_eq!(
            split_model_slug("some-slug@venice").unwrap(),
            ("some-slug".to_string(), Some("venice"))
        );
    }

    #[test]
    fn escaped_at_is_literal_and_openrouter() {
        // Post-TOML string is weird\@vendor/m (single backslash).
        assert_eq!(
            split_model_slug("weird\\@vendor/m").unwrap(),
            ("weird@vendor/m".to_string(), None)
        );
    }

    #[test]
    fn escape_and_suffix_compose() {
        assert_eq!(
            split_model_slug("weird\\@vendor/m@venice").unwrap(),
            ("weird@vendor/m".to_string(), Some("venice"))
        );
    }

    #[test]
    fn two_unescaped_ats_error() {
        assert!(matches!(
            split_model_slug("a@b@venice"),
            Err(SlugError::MultipleAt { .. })
        ));
    }

    #[test]
    fn empty_model_id_errors() {
        assert!(matches!(
            split_model_slug("@venice"),
            Err(SlugError::EmptyModelId { .. })
        ));
    }

    #[test]
    fn empty_provider_errors() {
        assert!(matches!(
            split_model_slug("x@"),
            Err(SlugError::EmptyProvider { .. })
        ));
    }

    #[test]
    fn trailing_lone_backslash_is_part_of_the_id() {
        assert_eq!(
            split_model_slug("weird\\").unwrap(),
            ("weird\\".to_string(), None)
        );
    }

    #[test]
    fn escaped_then_unescaped_adjacent() {
        // a\@@venice: first @ escaped, second is the separator.
        assert_eq!(
            split_model_slug("a\\@@venice").unwrap(),
            ("a@".to_string(), Some("venice"))
        );
    }

    #[test]
    fn bare_model_id_strips_suffix_and_unescapes() {
        assert_eq!(bare_model_id("some-slug@venice"), "some-slug");
        assert_eq!(bare_model_id("weird\\@vendor/m"), "weird@vendor/m");
        assert_eq!(bare_model_id("plain/model"), "plain/model");
    }

    #[test]
    fn escape_model_id_escapes_literal_at() {
        assert_eq!(escape_model_id("weird@vendor/m"), "weird\\@vendor/m");
    }

    #[test]
    fn escape_model_id_is_identity_without_at() {
        assert_eq!(escape_model_id("plain/model"), "plain/model");
    }

    #[test]
    fn escape_model_id_round_trips_through_split_model_slug() {
        let escaped = escape_model_id("weird@vendor/m");
        let slug = format!("{escaped}@venice");
        assert_eq!(
            split_model_slug(&slug).unwrap(),
            ("weird@vendor/m".to_string(), Some("venice"))
        );
    }

    #[test]
    fn bare_model_id_falls_back_to_raw_on_invalid() {
        // Invalid grammar can only reach this fn via non-config strings
        // (post-boot everything from config is valid); fail open to the raw.
        assert_eq!(bare_model_id("a@b@c"), "a@b@c");
    }

    #[test]
    fn error_messages_name_the_slug_and_the_escape() {
        let msg = split_model_slug("a@b@venice").unwrap_err().to_string();
        assert!(msg.contains("a@b@venice"));
        assert!(
            msg.contains("\\@"),
            "message should teach the escape: {msg}"
        );
    }

    #[test]
    fn empty_model_id_message_teaches_the_escape() {
        // Finding 2: a preset-style slug like `@preset/roleplay` should point
        // the operator at the `\@` escape, not just say "empty".
        let msg = split_model_slug("@preset/roleplay")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("@preset/roleplay"));
        assert!(
            msg.contains("\\@"),
            "message should teach the escape: {msg}"
        );
    }

    #[test]
    fn empty_provider_message_teaches_the_escape() {
        let msg = split_model_slug("x@").unwrap_err().to_string();
        assert!(msg.contains("x@"));
        assert!(
            msg.contains("\\@"),
            "message should teach the escape: {msg}"
        );
    }
}
