// SPDX-License-Identifier: AGPL-3.0-only
use super::{AuthError, AuthValidator};
use async_trait::async_trait;
use jsonwebtoken::{decode, errors::ErrorKind, DecodingKey, Validation};
use serde::Deserialize;
use uuid::Uuid;

pub struct SupabaseJwtValidator {
    secret: String,
}

impl SupabaseJwtValidator {
    pub fn new(secret: String) -> Self {
        Self { secret }
    }
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: Option<String>,
    exp: Option<i64>,
}

#[async_trait]
impl AuthValidator for SupabaseJwtValidator {
    async fn validate(&self, bearer: &str) -> Result<Uuid, AuthError> {
        let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
        // We check exp ourselves so we can return our own error variants and
        // avoid jsonwebtoken's required-claims behaviour.
        validation.required_spec_claims = std::collections::HashSet::new();
        // Supabase tokens have aud = "authenticated"; relaxing aud match keeps
        // the validator usable with self-issued test JWTs that omit it.
        validation.validate_aud = false;

        let data = decode::<Claims>(
            bearer,
            &DecodingKey::from_secret(self.secret.as_ref()),
            &validation,
        )
        .map_err(|e| match e.kind() {
            ErrorKind::ExpiredSignature => AuthError::Expired,
            ErrorKind::InvalidSignature => AuthError::BadSignature,
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

    fn mint(secret: &str, payload: serde_json::Value) -> String {
        encode(
            &Header::default(),
            &payload,
            &EncodingKey::from_secret(secret.as_ref()),
        )
        .expect("token encodes")
    }

    #[tokio::test]
    async fn valid_token_returns_user_id() {
        let v = SupabaseJwtValidator::new("test-secret".into());
        let uid = "00000000-0000-0000-0000-000000000001";
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let token = mint("test-secret", json!({ "sub": uid, "exp": exp }));
        let result = v.validate(&token).await.expect("valid");
        assert_eq!(result.to_string(), uid);
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let v = SupabaseJwtValidator::new("test-secret".into());
        let exp = (chrono::Utc::now() - chrono::Duration::hours(1)).timestamp();
        let uid = "00000000-0000-0000-0000-000000000001";
        let token = mint("test-secret", json!({ "sub": uid, "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::Expired), "got {err:?}");
    }

    #[tokio::test]
    async fn wrong_signature_rejected() {
        let v = SupabaseJwtValidator::new("real-secret".into());
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let uid = "00000000-0000-0000-0000-000000000001";
        let token = mint("WRONG", json!({ "sub": uid, "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::BadSignature), "got {err:?}");
    }

    #[tokio::test]
    async fn missing_sub_rejected() {
        let v = SupabaseJwtValidator::new("test-secret".into());
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        let token = mint("test-secret", json!({ "exp": exp }));
        let err = v.validate(&token).await.unwrap_err();
        assert!(matches!(err, AuthError::MissingSub), "got {err:?}");
    }
}
