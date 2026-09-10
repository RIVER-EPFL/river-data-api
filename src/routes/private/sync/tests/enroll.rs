use super::*;
use crate::routes::private::api_tokens::service::hash_token;

fn credential(secret: &str, revoked: bool) -> credentials::Model {
    credentials::Model {
        id: Uuid::nil(),
        client_id: "rvd-sync-1".to_string(),
        client_secret_hash: hash_token(secret),
        service_type: "vaisala".to_string(),
        source_system: Some("vaisala".to_string()),
        service_id: None,
        revoked,
        created_at: Utc::now().into(),
    }
}

#[test]
fn test_check_credential_admits_the_right_secret() {
    let cred = credential("s3cret", false);
    assert_eq!(
        check_credential(Some(&cred), "s3cret"),
        Ok(SecretFormat::LegacyDigest)
    );
}

fn argon2_credential(secret: &str) -> credentials::Model {
    let mut cred = credential(secret, false);
    cred.client_secret_hash = crate::routes::private::api_tokens::service::hash_api_secret(secret);
    cred
}

#[test]
fn test_an_argon2_credential_admits_its_own_secret_and_no_other() {
    let cred = argon2_credential("s3cret");
    assert_eq!(
        check_credential(Some(&cred), "s3cret"),
        Ok(SecretFormat::Argon2)
    );
    assert_eq!(
        check_credential(Some(&cred), "wrong"),
        Err(EnrollDenial::BadSecret)
    );
}

// Scenario: the same secret hashed twice.
// Expected behaviour: argon2 salts, so the two stored hashes differ and a stolen table
// cannot be attacked with one pass over a wordlist the way the unsalted digest could.
#[test]
fn test_two_credentials_sharing_a_secret_store_different_hashes() {
    let a = argon2_credential("s3cret");
    let b = argon2_credential("s3cret");
    assert_ne!(a.client_secret_hash, b.client_secret_hash);
    assert!(a.client_secret_hash.starts_with("$argon2id$"));
}

#[test]
fn test_the_stored_format_decides_which_verifier_runs() {
    assert_eq!(
        secret_format(&crate::routes::private::api_tokens::service::hash_api_secret("s")),
        SecretFormat::Argon2
    );
    assert_eq!(secret_format(&hash_token("s")), SecretFormat::LegacyDigest);
    assert_eq!(secret_format(""), SecretFormat::LegacyDigest);
}

// A revoked credential is refused before either verifier runs, whichever format it holds.
#[test]
fn test_a_revoked_argon2_credential_is_refused_with_its_own_secret() {
    let mut cred = argon2_credential("s3cret");
    cred.revoked = true;
    assert_eq!(
        check_credential(Some(&cred), "s3cret"),
        Err(EnrollDenial::Revoked)
    );
}

// Scenario: a probe walks client ids against /sync/enroll.
// Expected behaviour: an unknown id, a revoked credential and a wrong secret are refused
// with one message, so the response says nothing about which ids exist.
#[test]
fn test_every_refusal_answers_with_the_same_message() {
    let cred = credential("s3cret", false);
    let revoked = credential("s3cret", true);
    let refusals = [
        check_credential(None, "s3cret"),
        check_credential(Some(&revoked), "s3cret"),
        check_credential(Some(&cred), "wrong"),
    ];
    for r in &refusals {
        let denial = r.as_ref().expect_err("must be refused");
        assert_eq!(denial.public_message(), ENROLL_DENIED);
    }
    // The reason is still distinguishable server-side, for the log.
    assert!(matches!(refusals[0], Err(EnrollDenial::UnknownClient)));
    assert!(matches!(refusals[1], Err(EnrollDenial::Revoked)));
    assert!(matches!(refusals[2], Err(EnrollDenial::BadSecret)));
}

#[test]
fn test_check_credential_rejects_a_secret_whose_hash_is_only_a_prefix() {
    let mut cred = credential("s3cret", false);
    cred.client_secret_hash.truncate(16);
    assert!(matches!(
        check_credential(Some(&cred), "s3cret"),
        Err(EnrollDenial::BadSecret)
    ));
}

#[test]
fn test_check_credential_rejects_an_empty_secret() {
    let cred = credential("s3cret", false);
    assert!(matches!(
        check_credential(Some(&cred), ""),
        Err(EnrollDenial::BadSecret)
    ));
}

// A revoked credential is refused even when the secret is right, and the check does not
// depend on the order the two are tested in.
#[test]
fn test_revocation_beats_a_correct_secret() {
    let cred = credential("s3cret", true);
    assert!(matches!(
        check_credential(Some(&cred), "s3cret"),
        Err(EnrollDenial::Revoked)
    ));
}
