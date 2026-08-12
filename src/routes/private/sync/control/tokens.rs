//! Session and credential token generation for the sync control plane.
//!
//! Hashing is deliberately not implemented here: `api_tokens::service::hash_token` is the one
//! SHA-256 used for both the sync session lookup in `common/middleware.rs` and the credential
//! secret, and every service enrolled in the field holds a token hashed by it.

/// 32 random bytes as lowercase hex. Used for sync session tokens and for the two halves of an
/// enrollment credential.
///
/// This must not be replaced by `api_tokens::service`'s minting, which prefixes `rvd_`. The dual
/// auth middleware routes any `rvd_`-prefixed bearer down the argon2 API-token path before the
/// sync session fallback runs, so an `rvd_`-prefixed session token would be rejected.
#[must_use]
pub fn generate_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::private::api_tokens::service::hash_token;

    #[test]
    fn generated_tokens_are_unique_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn generated_tokens_carry_no_api_token_prefix() {
        assert!(!generate_token().starts_with("rvd_"));
    }

    /// A fixed vector, not just determinism: swapping the algorithm would still be deterministic
    /// but would invalidate every credential and session token already issued.
    #[test]
    fn session_hashing_is_sha256() {
        assert_eq!(
            hash_token("test-token"),
            "4c5dc9b7708905f77f5e5d16316b5dfb425e68cb326dcd55a860e90a7707031e"
        );
    }
}
