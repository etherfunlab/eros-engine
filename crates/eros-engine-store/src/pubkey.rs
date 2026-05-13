// SPDX-License-Identifier: AGPL-3.0-only
//! Base58-encoded Solana pubkey validation.
//!
//! Every public input — `asset_id`, `wallet_pubkey`, `owner_wallet` — must
//! decode to exactly 32 bytes. Rejecting non-canonical strings at the API
//! boundary keeps the data plane normalized so a single key cannot present
//! as two distinct rows.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PubkeyError {
    #[error("invalid base58")]
    InvalidBase58,
    #[error("wrong length: expected 32 bytes, got {0}")]
    WrongLength(usize),
}

/// Validate a base58-encoded Solana pubkey (32 bytes), returning the
/// canonical re-encoded form. The returned string is the only form stored
/// in `engine.*` tables so non-canonical input encodings cannot create
/// logical duplicates of the same key.
pub fn validate_solana_pubkey(s: &str) -> Result<String, PubkeyError> {
    let bytes = bs58::decode(s)
        .into_vec()
        .map_err(|_| PubkeyError::InvalidBase58)?;
    if bytes.len() != 32 {
        return Err(PubkeyError::WrongLength(bytes.len()));
    }
    Ok(bs58::encode(&bytes).into_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_32_byte_pubkey() {
        let canonical = "11111111111111111111111111111111";
        let out = validate_solana_pubkey(canonical).expect("valid");
        assert_eq!(out, canonical);
    }

    #[test]
    fn rejects_empty_string() {
        let err = validate_solana_pubkey("").expect_err("empty must fail");
        assert!(matches!(err, PubkeyError::WrongLength(0)));
    }

    #[test]
    fn rejects_non_base58() {
        let err = validate_solana_pubkey("0OIl/+=").expect_err("invalid b58 must fail");
        assert!(matches!(err, PubkeyError::InvalidBase58));
    }

    #[test]
    fn rejects_wrong_length() {
        // 33-byte payload, base58-encoded.
        let too_long = bs58::encode([0u8; 33]).into_string();
        let err = validate_solana_pubkey(&too_long).expect_err("33 bytes must fail");
        assert!(matches!(err, PubkeyError::WrongLength(33)));
    }
}
