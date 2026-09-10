use super::*;
use crate::routes::private::api_tokens::service::hash_token;

#[test]
fn generated_tokens_are_unique_hex() {
    let a = generate_token();
    let b = generate_token();
    assert_ne!(a, b);
    assert_eq!(a.len(), 64);
    assert!(
        a.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
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
