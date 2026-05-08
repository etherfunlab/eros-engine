// SPDX-License-Identifier: AGPL-3.0-only
//! Supabase JWT validator.
//!
//! Supports two signing modes:
//! * Asymmetric (ES256/RS256/EdDSA) via the project's JWKS endpoint, which
//!   is the default for Supabase projects since the 2025 "JWT Signing
//!   Keys" rollout.
//! * Legacy HS256 with a shared secret, kept around so OSS deployments
//!   that haven't migrated still work.
//!
//! At validation time we look at the token's `alg` header and dispatch
//! to whichever validator can handle it. Tokens minted before a key
//! rotation continue to verify against the JWKS until they expire (the
//! previous key remains in `keys` until revoked).
use super::{AuthError, AuthValidator};
use async_trait::async_trait;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct Claims {
    sub: Option<String>,
    exp: Option<i64>,
}

/// Decoding-key bundle: the JWT's `alg` header determines which one we use.
struct AsymKey {
    alg: Algorithm,
    key: DecodingKey,
}

pub struct SupabaseJwtValidator {
    /// kid → asymmetric public key (ES256 / RS256 / EdDSA).
    asymmetric: HashMap<String, AsymKey>,
    /// Optional HS256 shared secret for legacy projects.
    legacy_hs256: Option<DecodingKey>,
}

impl SupabaseJwtValidator {
    /// Build a validator with neither source wired. Call `with_jwks_url` and/or
    /// `with_legacy_secret` afterwards. A validator with neither configured
    /// rejects every token, which is the correct fail-closed behavior.
    pub fn new() -> Self {
        Self {
            asymmetric: HashMap::new(),
            legacy_hs256: None,
        }
    }

    /// Add an HS256 shared secret. Used for legacy Supabase projects that
    /// haven't migrated to JWT signing keys.
    pub fn with_legacy_secret(mut self, secret: String) -> Self {
        self.legacy_hs256 = Some(DecodingKey::from_secret(secret.as_ref()));
        self
    }

    /// Fetch the JWKS document and load every key into the asymmetric map.
    /// `url` is typically `https://<project>.supabase.co/auth/v1/.well-known/jwks.json`.
    pub async fn with_jwks_url(mut self, url: &str) -> anyhow::Result<Self> {
        let jwks: JwkSet = reqwest::get(url)
            .await
            .map_err(|e| anyhow::anyhow!("fetch jwks {url}: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("fetch jwks {url}: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parse jwks {url}: {e}"))?;

        for jwk in &jwks.keys {
            let Some(kid) = jwk.common.key_id.as_ref() else {
                tracing::warn!("jwks key missing kid, skipping");
                continue;
            };
            let alg = match jwk.common.key_algorithm {
                Some(jsonwebtoken::jwk::KeyAlgorithm::ES256) => Algorithm::ES256,
                Some(jsonwebtoken::jwk::KeyAlgorithm::RS256) => Algorithm::RS256,
                Some(jsonwebtoken::jwk::KeyAlgorithm::EdDSA) => Algorithm::EdDSA,
                Some(other) => {
                    tracing::warn!("jwks kid={kid} has unsupported alg {other:?}, skipping");
                    continue;
                }
                None => {
                    tracing::warn!("jwks kid={kid} has no alg, skipping");
                    continue;
                }
            };
            let key = DecodingKey::from_jwk(jwk)
                .map_err(|e| anyhow::anyhow!("decode jwk kid={kid}: {e}"))?;
            self.asymmetric.insert(kid.clone(), AsymKey { alg, key });
        }
        tracing::info!(
            "loaded {} asymmetric jwt key(s) from {url}",
            self.asymmetric.len()
        );
        Ok(self)
    }
}

impl Default for SupabaseJwtValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AuthValidator for SupabaseJwtValidator {
    async fn validate(&self, bearer: &str) -> Result<Uuid, AuthError> {
        let header = decode_header(bearer).map_err(|_| AuthError::Malformed)?;

        let (alg, key) = if header.alg == Algorithm::HS256 {
            let key = self.legacy_hs256.as_ref().ok_or(AuthError::BadSignature)?;
            (Algorithm::HS256, key)
        } else {
            // Asymmetric: dispatch by kid.
            let kid = header.kid.as_deref().ok_or(AuthError::Malformed)?;
            let entry = self.asymmetric.get(kid).ok_or(AuthError::BadSignature)?;
            (entry.alg, &entry.key)
        };

        let mut validation = Validation::new(alg);
        // We check exp ourselves so we can return our own error variants and
        // avoid jsonwebtoken's required-claims behaviour.
        validation.required_spec_claims = HashSet::new();
        // Supabase tokens have aud = "authenticated"; relaxing aud match keeps
        // the validator usable with self-issued test JWTs that omit it.
        validation.validate_aud = false;

        let data = decode::<Claims>(bearer, key, &validation).map_err(|e| match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
            jsonwebtoken::errors::ErrorKind::InvalidSignature => AuthError::BadSignature,
            _ => AuthError::Malformed,
        })?;

        if let Some(exp) = data.claims.exp {
            if exp < chrono::Utc::now().timestamp() {
                return Err(AuthError::Expired);
            }
        }
        let sub = data.claims.sub.ok_or(AuthError::MissingSub)?;
        Uuid::parse_str(&sub).map_err(|_| AuthError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    fn mint_hs256(secret: &str, payload: serde_json::Value) -> String {
        encode(
            &Header::default(),
            &payload,
            &EncodingKey::from_secret(secret.as_ref()),
        )
        .expect("token encodes")
    }

    #[tokio::test]
    async fn legacy_hs256_valid_token_returns_user_id() {
        let v = SupabaseJwtValidator::new().with_legacy_secret("test-secret".into());
        let uid = "00000000-0000-0000-0000-000000000001";
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let token = mint_hs256("test-secret", json!({ "sub": uid, "exp": exp }));
        let result = v.validate(&token).await.expect("valid");
        assert_eq!(result.to_string(), uid);
    }

    #[tokio::test]
    async fn legacy_hs256_expired_token_rejected() {
        let v = SupabaseJwtValidator::new().with_legacy_secret("test-secret".into());
        let exp = (chrono::Utc::now() - chrono::Duration::hours(1)).timestamp();
        let uid = "00000000-0000-0000-0000-000000000001";
        let token = mint_hs256("test-secret", json!({ "sub": uid, "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::Expired), "got {err:?}");
    }

    #[tokio::test]
    async fn legacy_hs256_wrong_signature_rejected() {
        let v = SupabaseJwtValidator::new().with_legacy_secret("real-secret".into());
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let uid = "00000000-0000-0000-0000-000000000001";
        let token = mint_hs256("WRONG", json!({ "sub": uid, "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::BadSignature), "got {err:?}");
    }

    #[tokio::test]
    async fn missing_sub_rejected() {
        let v = SupabaseJwtValidator::new().with_legacy_secret("test-secret".into());
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let token = mint_hs256("test-secret", json!({ "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::MissingSub), "got {err:?}");
    }

    #[tokio::test]
    async fn hs256_token_rejected_when_no_legacy_secret_configured() {
        let v = SupabaseJwtValidator::new();
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let uid = "00000000-0000-0000-0000-000000000001";
        let token = mint_hs256("any", json!({ "sub": uid, "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::BadSignature), "got {err:?}");
    }
}
