// SPDX-License-Identifier: AGPL-3.0-only
//! Bring-your-own-character (spec
//! docs/superpowers/specs/2026-10-16-byoc-design.md): admission, the init
//! ticket, and opening a sealed `eros-character` card
//! (docs/eros-character-v1.md).
//!
//! No error built here quotes the sealed string, the key or any card text.
//!
//! TODO(byoc routes): card opening and `LoadedInstance::into_persona` stay
//! unused until the routes that take a card call them. Allow-dead-code lifts
//! once they have a caller.
#![allow(dead_code)]

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use axum::http::{HeaderMap, StatusCode};
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

use eros_engine_core::persona::{merge_items, CompanionPersona, PersonaGenome, PersonaInstance};
use eros_engine_store::persona::{ByocInstance, PersonaRepo};

use crate::byok::{constant_time_eq, pre_error};
use crate::error::AppError;
use crate::state::{AppState, ByocConfig};

/// Request header carrying the deployment's caller secret (spec §4).
pub(crate) const CALLER_SECRET_HEADER: &str = "x-byoc-caller-secret";

/// Longest sealed string accepted (96 KiB).
const MAX_SEALED_LEN: usize = 98_304;
/// Longest card plaintext accepted (64 KiB).
const MAX_PLAINTEXT_LEN: usize = 65_536;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Domain separator of the init ticket (spec §5.1).
const TICKET_CONTEXT: &[u8] = b"eros-byoc-init-v1";

/// URL-safe alphabet, no padding on encode, padding optional on decode.
const B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub(crate) fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub(crate) fn b64_decode(s: &str) -> Option<Vec<u8>> {
    B64.decode(s).ok()
}

fn invalid_payload(message: impl Into<String>) -> AppError {
    pre_error(StatusCode::BAD_REQUEST, "invalid_payload", message.into())
}

fn forbidden(message: &str) -> AppError {
    pre_error(StatusCode::FORBIDDEN, "byoc_forbidden", message.into())
}

/// Spec §4: BYOC is served only to a caller holding the deployment's secret.
pub(crate) fn admit(cfg: &ByocConfig, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(secret) = cfg.caller_secret.as_deref() else {
        return Err(forbidden("byoc is not enabled on this deployment"));
    };
    let sent = headers
        .get(CALLER_SECRET_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if constant_time_eq(sent, secret.as_bytes()) {
        Ok(())
    } else {
        Err(forbidden("byoc caller secret missing or wrong"))
    }
}

/// A route that cannot serve a BYOC instance.
pub(crate) fn unsupported(route: &'static str) -> AppError {
    pre_error(
        StatusCode::BAD_REQUEST,
        "byoc_unsupported",
        format!("{route} does not serve a BYOC instance"),
    )
}

/// A fresh instance key (spec §5.1).
pub(crate) fn mint_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    key
}

fn ticket_mac(secret: &str, user_id: Uuid, key: &[u8]) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(TICKET_CONTEXT);
    mac.update(user_id.as_bytes());
    mac.update(key);
    mac
}

/// `HMAC-SHA256(secret, "eros-byoc-init-v1" ‖ user_id ‖ key)` (spec §5.1).
pub(crate) fn ticket(secret: &str, user_id: Uuid, key: &[u8; 32]) -> [u8; 32] {
    ticket_mac(secret, user_id, key)
        .finalize()
        .into_bytes()
        .into()
}

/// Constant-time check of a presented ticket.
pub(crate) fn verify_ticket(secret: &str, user_id: Uuid, key: &[u8], ticket: &[u8]) -> bool {
    ticket_mac(secret, user_id, key)
        .verify_slice(ticket)
        .is_ok()
}

/// An opened `eros-character` v1 card: the four genome fields.
pub(crate) struct Card {
    pub name: String,
    pub system_prompt: String,
    pub tip_personality: Option<String>,
    pub art_metadata: Value,
}

impl std::fmt::Debug for Card {
    /// Card text never reaches a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Card").finish_non_exhaustive()
    }
}

fn non_blank<'a>(data: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a str> {
    data.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// Validate a card (docs/eros-character-v1.md) on a `serde_json::Value`.
/// serde's own error text quotes the input, so it is never surfaced: every
/// `Err` is one of the fixed messages below.
pub(crate) fn parse_card_v1(plaintext: &[u8]) -> Result<Card, String> {
    let doc: Value = serde_json::from_slice(plaintext).map_err(|_| "not valid JSON")?;
    let Value::Object(top) = doc else {
        return Err("not a JSON object".into());
    };
    if top.get("spec").and_then(Value::as_str) != Some("eros-character") {
        return Err("spec must be \"eros-character\"".into());
    }
    if top.get("spec_version").and_then(Value::as_str) != Some("1") {
        return Err("unsupported spec_version".into());
    }
    if top
        .keys()
        .any(|k| !matches!(k.as_str(), "spec" | "spec_version" | "data"))
    {
        return Err("unknown top-level field".into());
    }
    let Some(Value::Object(data)) = top.get("data") else {
        return Err("data must be an object".into());
    };
    if data.keys().any(|k| {
        !matches!(
            k.as_str(),
            "name" | "system_prompt" | "tip_personality" | "art_metadata"
        )
    }) {
        return Err("unknown field in data".into());
    }
    let name = non_blank(data, "name").ok_or("data.name must be a non-blank string")?;
    let system_prompt =
        non_blank(data, "system_prompt").ok_or("data.system_prompt must be a non-blank string")?;
    let tip_personality = match data.get("tip_personality") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("data.tip_personality must be a string or null".into()),
    };
    let art_metadata = match data.get("art_metadata") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err("data.art_metadata must be an object or null".into()),
    };
    Ok(Card {
        name: name.into(),
        system_prompt: system_prompt.into(),
        tip_personality,
        art_metadata,
    })
}

/// Open a sealed card (spec §6.1): `base64url(nonce[12] ‖ ciphertext ‖ tag[16])`,
/// AES-256-GCM, no associated data.
pub(crate) fn open_card(key: &[u8], sealed: &str) -> Result<Card, AppError> {
    let malformed =
        || invalid_payload("character must be a base64url sealed card of at most 96 KiB");
    if sealed.len() > MAX_SEALED_LEN {
        return Err(malformed());
    }
    let raw = b64_decode(sealed)
        .filter(|raw| raw.len() >= NONCE_LEN + TAG_LEN)
        .ok_or_else(malformed)?;
    let (nonce, ciphertext) = raw
        .split_first_chunk::<NONCE_LEN>()
        .expect("length checked above");
    // A key that is not 32 bytes cannot open anything; it reads as a rejection.
    let plaintext = Aes256Gcm::new_from_slice(key)
        .ok()
        .and_then(|cipher| cipher.decrypt(&Nonce::from(*nonce), ciphertext).ok())
        .ok_or_else(|| {
            pre_error(
                StatusCode::FORBIDDEN,
                "byoc_card_rejected",
                "character does not open under this instance's key".into(),
            )
        })?;
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(invalid_payload("character plaintext exceeds 64 KiB"));
    }
    parse_card_v1(&plaintext).map_err(|msg| invalid_payload(format!("character: {msg}")))
}

/// What a session's `instance_id` points at: a genome persona, ready for a
/// turn, or a BYOC instance that needs its card first.
pub(crate) enum LoadedInstance {
    Genome(CompanionPersona),
    Byoc(ByocInstance),
}

impl LoadedInstance {
    pub(crate) fn owner_uid(&self) -> Uuid {
        match self {
            Self::Genome(p) => p.instance.owner_uid,
            Self::Byoc(b) => b.owner_uid,
        }
    }

    /// The persona a turn runs with (spec §6.3). A genome instance takes no
    /// card; a BYOC instance takes exactly one, opened under its own key.
    pub(crate) fn into_persona(
        self,
        character: Option<&str>,
    ) -> Result<CompanionPersona, AppError> {
        match (self, character) {
            (Self::Genome(persona), None) => Ok(persona),
            (Self::Genome(_), Some(_)) => Err(invalid_payload(
                "character is only accepted for a BYOC instance",
            )),
            (Self::Byoc(_), None) => {
                Err(invalid_payload("character is required for a BYOC instance"))
            }
            (Self::Byoc(b), Some(sealed)) => {
                let card = open_card(&b.key, sealed)?;
                Ok(CompanionPersona {
                    instance_id: b.instance_id,
                    items: merge_items(&card.art_metadata, b.items),
                    genome: PersonaGenome {
                        id: None,
                        name: card.name,
                        system_prompt: card.system_prompt,
                        tip_personality: card.tip_personality,
                        art_metadata: card.art_metadata,
                    },
                    instance: PersonaInstance {
                        id: b.instance_id,
                        genome_id: None,
                        owner_uid: b.owner_uid,
                        status: "active".into(),
                    },
                })
            }
        }
    }
}

/// The active instance `instance_id` names, of either kind; `None` when there
/// is no such active instance.
pub(crate) async fn load_instance(
    state: &AppState,
    instance_id: Uuid,
) -> Result<Option<LoadedInstance>, AppError> {
    let repo = PersonaRepo { pool: &state.pool };
    if let Some(persona) = repo.load_companion(instance_id).await? {
        return Ok(Some(LoadedInstance::Genome(persona)));
    }
    Ok(repo
        .load_byoc_instance(instance_id)
        .await?
        .map(LoadedInstance::Byoc))
}

/// Seal `plaintext` the way a downstream does.
#[cfg(test)]
pub(crate) fn seal(key: &[u8; 32], plaintext: &[u8]) -> String {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = Aes256Gcm::new_from_slice(key)
        .expect("32-byte key")
        .encrypt(&Nonce::from(nonce), plaintext)
        .expect("AES-GCM encrypts");
    b64_encode(&[nonce.as_slice(), ciphertext.as_slice()].concat())
}

/// A v1 card document.
#[cfg(test)]
pub(crate) fn card_json(name: &str, system_prompt: &str, art: Value) -> Vec<u8> {
    json!({
        "spec": "eros-character",
        "spec_version": "1",
        "data": {"name": name, "system_prompt": system_prompt, "art_metadata": art},
    })
    .to_string()
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byoc_config_parses_like_byok() {
        use crate::state::parse_byoc_config;
        assert!(parse_byoc_config(None).unwrap().caller_secret.is_none());
        assert!(parse_byoc_config(Some("   "))
            .unwrap()
            .caller_secret
            .is_none());
        assert!(parse_byoc_config(Some("a".repeat(31).as_str())).is_err());
        assert_eq!(
            parse_byoc_config(Some(format!(" {} ", "a".repeat(32)).as_str()))
                .unwrap()
                .caller_secret,
            Some("a".repeat(32))
        );
    }

    fn code_of(e: AppError) -> (u16, &'static str) {
        let AppError::StreamPre(e) = e else {
            panic!("expected a coded error")
        };
        (e.status.as_u16(), e.code)
    }

    #[test]
    fn admit_needs_the_secret() {
        let on = ByocConfig {
            caller_secret: Some("s".repeat(32)),
        };
        let mut h = HeaderMap::new();
        assert_eq!(
            code_of(admit(&ByocConfig::default(), &h).unwrap_err()),
            (403, "byoc_forbidden")
        );
        assert_eq!(
            code_of(admit(&on, &h).unwrap_err()),
            (403, "byoc_forbidden")
        );
        h.insert(CALLER_SECRET_HEADER, "t".repeat(32).parse().unwrap());
        assert_eq!(
            code_of(admit(&on, &h).unwrap_err()),
            (403, "byoc_forbidden")
        );
        h.insert(CALLER_SECRET_HEADER, "s".repeat(32).parse().unwrap());
        assert!(admit(&on, &h).is_ok());
    }

    #[test]
    fn ticket_binds_secret_user_and_key() {
        let (u, k) = (Uuid::new_v4(), [3u8; 32]);
        let t = ticket("s".repeat(32).as_str(), u, &k);
        assert!(verify_ticket(&"s".repeat(32), u, &k, &t));
        assert!(!verify_ticket(&"s".repeat(32), Uuid::new_v4(), &k, &t));
        assert!(!verify_ticket(&"s".repeat(32), u, &[4u8; 32], &t));
        assert!(!verify_ticket(&"t".repeat(32), u, &k, &t));
        assert!(!verify_ticket(&"s".repeat(32), u, &k, &t[..31]));
    }

    #[test]
    fn a_sealed_v1_card_opens() {
        let key = [7u8; 32];
        let card = open_card(
            &key,
            &seal(
                &key,
                &card_json(
                    "Aria",
                    "You are Aria.",
                    serde_json::json!({"appearance": "auburn hair"}),
                ),
            ),
        )
        .unwrap();
        assert_eq!(
            (card.name.as_str(), card.system_prompt.as_str()),
            ("Aria", "You are Aria.")
        );
        assert_eq!(card.art_metadata["appearance"], "auburn hair");
        assert_eq!(card.tip_personality, None);
    }

    #[test]
    fn a_card_sealed_under_another_key_is_rejected() {
        let sealed = seal(&[1u8; 32], &card_json("A", "p", serde_json::json!({})));
        assert_eq!(
            code_of(open_card(&[2u8; 32], &sealed).unwrap_err()),
            (403, "byoc_card_rejected")
        );
    }

    #[test]
    fn malformed_sealed_strings_are_400() {
        let cases = [
            String::new(),
            "!!!not-base64!!!".to_string(),
            b64_encode(&[0u8; 27]),
            "A".repeat(98_308),
        ];
        for s in &cases {
            assert_eq!(
                code_of(open_card(&[1u8; 32], s).unwrap_err()),
                (400, "invalid_payload")
            );
        }
    }

    #[test]
    fn padded_base64url_is_accepted() {
        // Review Focus 2
        let key = [7u8; 32];
        let mut sealed = seal(&key, &card_json("A", "p", serde_json::json!({})));
        while !sealed.len().is_multiple_of(4) {
            sealed.push('=');
        }
        assert!(open_card(&key, &sealed).is_ok());
        assert!(b64_decode(&format!("{}=", b64_encode(&[1, 2]))).is_some());
    }

    #[test]
    fn plaintext_limit_is_64_kib() {
        // Review Focus 5
        let key = [7u8; 32];
        let base = card_json("A", "", serde_json::json!({})).len();
        let at = card_json("A", &"x".repeat(65_536 - base), serde_json::json!({}));
        assert_eq!(at.len(), 65_536);
        assert!(open_card(&key, &seal(&key, &at)).is_ok());
        let over = card_json("A", &"x".repeat(65_537 - base), serde_json::json!({}));
        assert_eq!(
            code_of(open_card(&key, &seal(&key, &over)).unwrap_err()),
            (400, "invalid_payload")
        );
    }

    #[test]
    fn parse_card_v1_rules() {
        let ok = |v: serde_json::Value| parse_card_v1(v.to_string().as_bytes());
        let d = |data: serde_json::Value| serde_json::json!({"spec":"eros-character","spec_version":"1","data":data});
        assert_eq!(
            ok(d(serde_json::json!({"name":"A","system_prompt":"p"})))
                .unwrap()
                .art_metadata,
            serde_json::json!({})
        );
        assert_eq!(
            ok(d(
                serde_json::json!({"name":"A","system_prompt":"p","art_metadata":null})
            ))
            .unwrap()
            .art_metadata,
            serde_json::json!({})
        ); // Review Focus 1
        assert!(ok(
            serde_json::json!({"spec":"chara_card_v2","spec_version":"1","data":{"name":"A","system_prompt":"p"}})
        )
        .is_err());
        assert!(ok(
            serde_json::json!({"spec":"eros-character","spec_version":"2","data":{"name":"A","system_prompt":"p"}})
        )
        .is_err());
        assert!(ok(
            serde_json::json!({"spec":"eros-character","spec_version":"1","data":{"name":"A","system_prompt":"p"},"x":1})
        )
        .is_err());
        assert!(ok(d(
            serde_json::json!({"name":"A","system_prompt":"p","lorebook":[]})
        ))
        .is_err());
        assert!(ok(d(serde_json::json!({"name":"  ","system_prompt":"p"}))).is_err());
        assert!(ok(d(serde_json::json!({"name":"A"}))).is_err());
        assert!(ok(d(
            serde_json::json!({"name":"A","system_prompt":"p","art_metadata":[]})
        ))
        .is_err());
        assert!(ok(d(
            serde_json::json!({"name":"A","system_prompt":"p","tip_personality":3})
        ))
        .is_err());
    }

    #[test]
    fn errors_never_quote_card_text() {
        let key = [7u8; 32];
        let marker = "CARD-TEXT-MARKER";
        for pt in [
            format!("{{\"spec\":\"{marker}\"").into_bytes(),
            serde_json::json!({"spec":"eros-character","spec_version":"1",
                "data":{"name":marker,"system_prompt":"p",(marker):1}})
            .to_string()
            .into_bytes(),
        ] {
            let AppError::StreamPre(e) = open_card(&key, &seal(&key, &pt)).unwrap_err() else {
                panic!()
            };
            assert!(!e.message.contains(marker) && !e.user_message.contains(marker));
        }
    }

    mod instance {
        use super::*;
        use crate::routes::companion::test_state;
        use crate::routes::companion::testutil::{
            sealed_card, seed_byoc_instance_with_key, seed_persona_instance,
        };
        use eros_engine_core::types::ItemKind;
        use sqlx::PgPool;

        #[sqlx::test(migrations = "../eros-engine-store/migrations")]
        async fn into_persona_builds_the_turn_persona_from_the_card(pool: PgPool) {
            let owner = Uuid::new_v4();
            let (id, key) = seed_byoc_instance_with_key(&pool, owner).await;
            PersonaRepo { pool: &pool }
                .insert_item(id, ItemKind::Item, "摩托车")
                .await
                .unwrap();
            let sealed = sealed_card(
                &key,
                "Aria",
                "You are Aria.",
                json!({"possessions": ["吉他"]}),
            );

            let loaded = load_instance(&test_state(pool), id)
                .await
                .unwrap()
                .expect("an active BYOC instance loads");
            assert_eq!(loaded.owner_uid(), owner);
            let persona = loaded.into_persona(Some(&sealed)).unwrap();

            assert_eq!(persona.genome.name, "Aria");
            assert_eq!(persona.genome.system_prompt, "You are Aria.");
            assert_eq!(persona.genome.id, None);
            assert_eq!(persona.instance.genome_id, None);
            assert_eq!(persona.instance.id, id);
            assert_eq!(persona.instance_id, id);
            assert_eq!(persona.instance.owner_uid, owner);
            assert_eq!(persona.instance.status, "active");
            let names: Vec<&str> = persona.items.iter().map(|i| i.name.as_str()).collect();
            assert!(names.contains(&"吉他"), "{names:?}");
            assert!(names.contains(&"摩托车"), "{names:?}");
        }

        #[sqlx::test(migrations = "../eros-engine-store/migrations")]
        async fn into_persona_enforces_card_presence_by_kind(pool: PgPool) {
            let owner = Uuid::new_v4();
            let state = test_state(pool.clone());
            let genome = seed_persona_instance(&pool, owner).await;
            let (byoc, key) = seed_byoc_instance_with_key(&pool, owner).await;
            let sealed = sealed_card(&key, "A", "p", json!({}));

            let loaded = load_instance(&state, genome).await.unwrap().unwrap();
            assert!(matches!(loaded, LoadedInstance::Genome(_)));
            assert_eq!(loaded.owner_uid(), owner);
            assert_eq!(
                code_of(loaded.into_persona(Some(&sealed)).unwrap_err()),
                (400, "invalid_payload")
            );
            let loaded = load_instance(&state, genome).await.unwrap().unwrap();
            assert!(loaded.into_persona(None).is_ok());

            let loaded = load_instance(&state, byoc).await.unwrap().unwrap();
            assert!(matches!(loaded, LoadedInstance::Byoc(_)));
            assert_eq!(
                code_of(loaded.into_persona(None).unwrap_err()),
                (400, "invalid_payload")
            );
        }

        #[sqlx::test(migrations = "../eros-engine-store/migrations")]
        async fn a_card_sealed_under_another_instances_key_is_rejected(pool: PgPool) {
            let owner = Uuid::new_v4();
            let (id, _) = seed_byoc_instance_with_key(&pool, owner).await;
            let (_, other_key) = seed_byoc_instance_with_key(&pool, owner).await;
            let sealed = sealed_card(&other_key, "A", "p", json!({}));
            let loaded = load_instance(&test_state(pool), id).await.unwrap().unwrap();
            assert_eq!(
                code_of(loaded.into_persona(Some(&sealed)).unwrap_err()),
                (403, "byoc_card_rejected")
            );
        }

        #[sqlx::test(migrations = "../eros-engine-store/migrations")]
        async fn an_unknown_or_archived_instance_does_not_load(pool: PgPool) {
            let owner = Uuid::new_v4();
            let state = test_state(pool.clone());
            assert!(load_instance(&state, Uuid::new_v4())
                .await
                .unwrap()
                .is_none());
            let (id, _) = seed_byoc_instance_with_key(&pool, owner).await;
            sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            assert!(load_instance(&state, id).await.unwrap().is_none());
        }
    }
}
