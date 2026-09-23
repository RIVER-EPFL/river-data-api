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

mod token_shape {
    use super::super::{mint_api_token, split_api_token};

    const PREFIX: &str = "0123456789abcdef";

    fn secret() -> String {
        "a".repeat(64)
    }

    #[test]
    fn test_a_minted_token_splits_into_its_prefix_and_secret() {
        let minted = mint_api_token();
        let (prefix, secret) = split_api_token(&minted.raw_token).expect("well formed");
        assert_eq!(prefix, minted.token_prefix);
        assert_eq!(secret.len(), 64);
    }

    #[test]
    fn test_a_token_of_another_scheme_is_not_an_api_token() {
        assert_eq!(split_api_token("eyJhbGciOiJSUzI1NiJ9.e30.sig"), None);
        assert_eq!(split_api_token(&format!("xyz_{PREFIX}_{}", secret())), None);
        assert_eq!(split_api_token(&format!("rvd_{PREFIX}{}", secret())), None);
    }

    #[test]
    fn test_a_wrong_length_part_is_refused() {
        assert_eq!(
            split_api_token(&format!("rvd_{}_{}", &PREFIX[..15], secret())),
            None
        );
        assert_eq!(
            split_api_token(&format!("rvd_{PREFIX}0_{}", secret())),
            None
        );
        assert_eq!(
            split_api_token(&format!("rvd_{PREFIX}_{}", "a".repeat(63))),
            None
        );
        assert_eq!(
            split_api_token(&format!("rvd_{PREFIX}_{}", "a".repeat(65))),
            None
        );
    }

    #[test]
    fn test_upper_case_or_non_hex_is_refused() {
        assert_eq!(
            split_api_token(&format!("rvd_{}_{}", PREFIX.to_uppercase(), secret())),
            None
        );
        assert_eq!(
            split_api_token(&format!("rvd_{PREFIX}_{}", "g".repeat(64))),
            None
        );
        assert_eq!(
            split_api_token(&format!("rvd_{PREFIX}_{}", "A".repeat(64))),
            None
        );
    }

    #[test]
    fn test_a_well_formed_token_splits_at_the_first_separator() {
        assert_eq!(
            split_api_token(&format!("rvd_{PREFIX}_{}", secret())),
            Some((PREFIX, secret().as_str()))
        );
    }
}

mod requested_roles {
    use super::super::{river_role_names, validate_requested_roles};
    use crate::routes::private::api_tokens::models::KeycloakRole;

    fn realm() -> Vec<KeycloakRole> {
        ["riverdata-admin", "riverdata-river", "offline_access"]
            .iter()
            .map(|name| KeycloakRole {
                id: format!("id-{name}"),
                name: (*name).to_string(),
            })
            .collect()
    }

    #[test]
    fn test_a_river_role_the_realm_holds_is_accepted() {
        assert!(validate_requested_roles(&["riverdata-river".to_string()], &realm()).is_ok());
        assert!(validate_requested_roles(&[], &realm()).is_ok());
    }

    #[test]
    fn test_a_role_that_is_not_a_river_access_level_is_refused() {
        let err = validate_requested_roles(&["offline_access".to_string()], &realm())
            .expect_err("refused");
        assert!(
            err.to_string()
                .contains("Not a river access role: offline_access"),
            "{err}"
        );
    }

    #[test]
    fn test_a_river_role_the_realm_does_not_hold_is_refused() {
        let err = validate_requested_roles(&["riverdata-manager".to_string()], &realm())
            .expect_err("refused");
        assert!(
            err.to_string()
                .contains("Unknown realm role(s): riverdata-manager"),
            "{err}"
        );
    }

    #[test]
    fn test_only_river_role_names_are_read_from_a_role_listing() {
        let listing = [
            serde_json::json!({ "name": "riverdata-intern" }),
            serde_json::json!({ "name": "default-roles-epfl" }),
            serde_json::json!({ "id": "no-name" }),
            serde_json::json!({ "name": "riverdata-admin" }),
        ];
        assert_eq!(
            river_role_names(&listing),
            vec![
                "riverdata-intern".to_string(),
                "riverdata-admin".to_string()
            ]
        );
    }
}
