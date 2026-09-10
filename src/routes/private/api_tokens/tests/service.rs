use super::*;

fn token(expires_at: Option<chrono::DateTime<Utc>>) -> model::Model {
    model::Model {
        id: uuid::Uuid::nil(),
        name: "test".to_string(),
        description: None,
        token_hash: String::new(),
        token_prefix: "abcdefgh".to_string(),
        project_scope: None,
        permissions: serde_json::json!({}),
        is_active: true,
        rate_limit_per_second: None,
        created_at: None,
        expires_at,
        last_used_at: None,
        created_by: None,
        token: None,
    }
}

#[test]
fn test_is_expired_rejects_a_past_expiry() {
    assert!(is_expired(&token(Some(
        Utc::now() - chrono::Duration::seconds(1)
    ))));
}

#[test]
fn test_is_expired_admits_a_future_expiry() {
    assert!(!is_expired(&token(Some(
        Utc::now() + chrono::Duration::hours(1)
    ))));
}

/// No expiry is not an expiry that has passed: a token without one never ages out.
#[test]
fn test_a_token_with_no_expiry_never_expires() {
    assert!(!is_expired(&token(None)));
}
