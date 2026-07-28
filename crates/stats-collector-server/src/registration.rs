//! Registration handshake for issuing per-`telemetry_subject_id` ingestion tokens.
//!
//! See `docs/server-node-hardware-reliability-telemetry-strategy.md` Sections 5.2/8: this resolves
//! the "abuse protection without identity" open question. The issued token proves only "this
//! caller previously completed a registration handshake for this specific pseudonymous subject
//! id" — it is never linkable to a real node/cluster/operator, carries no structure or embedded
//! identity, and is a pure bearer secret shared between one node and the collector.

use rand::RngCore;

/// Upper bound on an accepted `telemetry_subject_id` length for the registration path parameter —
/// a sanity/DoS guard against absurdly large path segments, not a format check. A real
/// `telemetry_subject_id` is a 64-char hex-encoded HMAC-SHA256 (see `server-node-sdk`), so this
/// leaves ample headroom for future formats without accepting unbounded input.
const MAX_SUBJECT_ID_LEN: usize = 256;

/// Number of random bytes in a generated ingestion token (256 bits), hex-encoded to 64 chars.
const TOKEN_BYTES: usize = 32;

/// Generates a random opaque ingestion token: bytes from the process CSPRNG, hex-encoded.
/// Unguessable and structureless - purely a shared secret between one node and the collector,
/// scoped to one `telemetry_subject_id` by the caller (see `storage::IngestStorage`).
pub fn generate_ingestion_token() -> String {
    let mut bytes = [0_u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

/// Why a registration request was rejected outright, before ever touching rate limits/storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    EmptySubjectId,
    SubjectIdTooLong(usize),
}

impl RegistrationError {
    pub fn message(&self) -> String {
        match self {
            RegistrationError::EmptySubjectId => {
                "telemetry_subject_id must not be empty".to_string()
            }
            RegistrationError::SubjectIdTooLong(len) => {
                format!("telemetry_subject_id is too long ({len} chars, max {MAX_SUBJECT_ID_LEN})")
            }
        }
    }
}

/// Validates (and trims) the `telemetry_subject_id` path parameter for
/// `POST /v1/register/{telemetry_subject_id}`.
pub fn validate_subject_id_for_registration(raw: &str) -> Result<String, RegistrationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(RegistrationError::EmptySubjectId);
    }
    if trimmed.len() > MAX_SUBJECT_ID_LEN {
        return Err(RegistrationError::SubjectIdTooLong(trimmed.len()));
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_64_hex_chars_and_differ() {
        let a = generate_ingestion_token();
        let b = generate_ingestion_token();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two generated tokens must not collide in practice");
    }

    #[test]
    fn rejects_empty_subject_id() {
        assert_eq!(
            validate_subject_id_for_registration("   "),
            Err(RegistrationError::EmptySubjectId)
        );
    }

    #[test]
    fn rejects_too_long_subject_id() {
        let long = "a".repeat(MAX_SUBJECT_ID_LEN + 1);
        assert_eq!(
            validate_subject_id_for_registration(&long),
            Err(RegistrationError::SubjectIdTooLong(MAX_SUBJECT_ID_LEN + 1))
        );
    }

    #[test]
    fn trims_and_accepts_plausible_subject_id() {
        assert_eq!(
            validate_subject_id_for_registration("  abc123  ").unwrap(),
            "abc123"
        );
    }
}
