use super::{
    AccessScope, Capability, Role, TokenAccess, TokenBit, TokenPermissions, crud_gate,
    keycloak_allows, token_allows,
};
use axum::http::Method;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

const CAPABILITIES: [Capability; 8] = [
    Capability::ReadMetadata,
    Capability::ReadData,
    Capability::EnterFieldData,
    Capability::WriteData,
    Capability::WriteFieldMetadata,
    Capability::ManageSensors,
    Capability::WriteCatalog,
    Capability::Admin,
];

/// The policy table written out by hand, so a change to `min_role` has to be made twice.
fn expected_min_level(cap: Capability) -> u8 {
    match cap {
        Capability::ReadMetadata | Capability::ReadData | Capability::EnterFieldData => 1,
        Capability::WriteData | Capability::WriteFieldMetadata => 2,
        Capability::ManageSensors | Capability::WriteCatalog => 3,
        Capability::Admin => 4,
    }
}

fn levels() -> Vec<Role> {
    vec![
        Role::Unknown("offline_access".to_string()),
        Role::Intern,
        Role::River,
        Role::Manager,
        Role::Administrator,
    ]
}

#[test]
fn the_lattice_admits_a_capability_exactly_from_its_minimum_level_up() {
    for cap in CAPABILITIES {
        for role in levels() {
            let want = role.level() > 0 && role.level() >= expected_min_level(cap);
            assert_eq!(
                keycloak_allows(std::slice::from_ref(&role), cap),
                want,
                "{role} against {cap}"
            );
        }
    }
}

#[test]
fn a_login_with_no_riverdata_role_holds_nothing() {
    let unknown = [Role::Unknown("default-roles-river".to_string())];
    for cap in CAPABILITIES {
        assert!(!keycloak_allows(&unknown, cap), "{cap} reached at level 0");
    }
    assert!(!keycloak_allows(&[], Capability::ReadMetadata));
}

#[test]
fn the_highest_role_decides_when_several_are_held() {
    let held = [
        Role::Intern,
        Role::Manager,
        Role::Unknown("uma_authorization".to_string()),
    ];
    assert!(keycloak_allows(&held, Capability::WriteCatalog));
    assert!(!keycloak_allows(&held, Capability::Admin));
}

#[test]
fn every_role_name_that_admits_a_user_parses_and_grants_access() {
    for name in super::RIVER_ROLE_NAMES {
        let role = Role::from(name.to_string());
        assert!(role.grants_access(), "{name} parsed to {role:?}");
        assert_eq!(role.to_string(), name, "{name} does not round-trip");
    }
    assert!(!Role::from("admin".to_string()).grants_access());
}

fn only(bit: TokenBit) -> TokenPermissions {
    TokenPermissions {
        read_metadata: bit == TokenBit::ReadMetadata,
        read_data: bit == TokenBit::ReadData,
        write_metadata: bit == TokenBit::WriteMetadata,
        write_data: bit == TokenBit::WriteData,
    }
}

fn all_bits() -> TokenPermissions {
    TokenPermissions {
        read_metadata: true,
        read_data: true,
        write_metadata: true,
        write_data: true,
    }
}

#[test]
fn a_token_under_the_default_rule_needs_exactly_the_capabilitys_own_bit() {
    for cap in CAPABILITIES {
        for bit in [
            TokenBit::ReadMetadata,
            TokenBit::ReadData,
            TokenBit::WriteMetadata,
            TokenBit::WriteData,
        ] {
            let want = cap.default_token_bit() == Some(bit);
            assert_eq!(
                token_allows(&only(bit), cap, TokenAccess::Same),
                want,
                "{cap} with only {bit:?}"
            );
        }
    }
}

#[test]
fn no_token_reaches_admin_under_the_default_rule() {
    // Admin has no bit of its own, so `require_admin` (TokenAccess::Deny) and any gate left on
    // the default rule refuse every token. Only an explicit Bit override admits one, which is
    // the frozen sync-service surface asserted below.
    assert_eq!(Capability::Admin.default_token_bit(), None);
    assert!(!token_allows(
        &all_bits(),
        Capability::Admin,
        TokenAccess::Same
    ));
    assert!(!token_allows(
        &all_bits(),
        Capability::Admin,
        TokenAccess::Deny
    ));
}

#[test]
fn an_explicit_bit_overrides_the_capability_and_deny_admits_nobody() {
    // Stream registration is Administrator on the human side and keeps the historical
    // write_metadata bit for the sync services.
    assert!(token_allows(
        &only(TokenBit::WriteMetadata),
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata)
    ));
    assert!(!token_allows(
        &only(TokenBit::WriteData),
        Capability::Admin,
        TokenAccess::Bit(TokenBit::WriteMetadata)
    ));
    for cap in CAPABILITIES {
        assert!(
            !token_allows(&all_bits(), cap, TokenAccess::Deny),
            "{cap} passed a denied token rule"
        );
    }
}

#[test]
fn the_field_entry_capability_travels_on_the_data_write_bit() {
    // An intern holds EnterFieldData on the human side; no token bit was minted for it, so it
    // rides write_data rather than widening the token surface.
    assert_eq!(
        Capability::EnterFieldData.default_token_bit(),
        Some(TokenBit::WriteData)
    );
    assert!(keycloak_allows(&[Role::Intern], Capability::EnterFieldData));
    assert!(!keycloak_allows(&[Role::Intern], Capability::WriteData));
}

#[test]
fn token_permissions_default_to_read_only_and_survive_a_partial_json() {
    let parsed = TokenPermissions::from_json(&serde_json::json!({ "write_data": true }));
    assert!(parsed.read_metadata && parsed.read_data && parsed.write_data);
    assert!(!parsed.write_metadata);
    let broken = TokenPermissions::from_json(&serde_json::json!("not an object"));
    assert!(!broken.write_metadata && !broken.write_data);
}

#[test]
fn only_get_and_head_reach_a_crud_route_s_read_capability() {
    let gate = |method: Method| {
        crud_gate(
            &method,
            Capability::ReadMetadata,
            Capability::WriteCatalog,
            TokenAccess::Bit(TokenBit::WriteMetadata),
        )
    };
    for read in [Method::GET, Method::HEAD] {
        assert_eq!(
            gate(read.clone()),
            (Capability::ReadMetadata, TokenAccess::Same),
            "{read} is not gated as a read"
        );
    }
    for write in [
        Method::POST,
        Method::PUT,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
        Method::TRACE,
        Method::CONNECT,
    ] {
        assert_eq!(
            gate(write.clone()),
            (
                Capability::WriteCatalog,
                TokenAccess::Bit(TokenBit::WriteMetadata)
            ),
            "{write} is not gated as a write"
        );
    }
}

#[test]
fn a_restricted_scope_is_fail_closed_on_an_empty_set_and_on_a_project_less_row() {
    let project = Uuid::new_v4();
    let other = Uuid::new_v4();
    let one = AccessScope::one(project);
    assert!(one.is_restricted());
    assert!(one.allows_project(project));
    assert!(!one.allows_project(other));
    assert!(!one.allows_project_opt(None));
    assert_eq!(one.project_ids().map(|ids| ids.len()), Some(1));

    let empty = AccessScope::Projects(Arc::new(HashSet::new()));
    assert!(!empty.allows_project(project));
    assert_eq!(empty.project_ids(), Some(vec![]));

    let unrestricted = AccessScope::Unrestricted;
    assert!(!unrestricted.is_restricted());
    assert!(unrestricted.allows_project(other));
    assert!(unrestricted.allows_project_opt(None));
    assert!(unrestricted.project_ids().is_none());
    assert!(unrestricted.sql_project_array().is_none());
}
