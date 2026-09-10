use super::source_system;
use crate::common::middleware::{AuthContext, TokenPermissions};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

fn service(declared: Option<&str>) -> AuthContext {
    AuthContext::SyncService {
        service_id: Uuid::new_v4(),
        source_system: declared.map(str::to_string),
    }
}

fn administrator() -> AuthContext {
    AuthContext::Keycloak {
        roles: Vec::new(),
        sub: "sub-1".to_string(),
        email: Some("evan@epfl.ch".to_string()),
        email_verified: true,
        grants: Arc::new(HashSet::new()),
    }
}

#[test]
fn a_service_writes_under_the_system_it_enrolled_for() {
    assert_eq!(
        source_system(&service(Some("metalp")), "").unwrap(),
        "metalp"
    );
    assert_eq!(
        source_system(&service(Some("METALP")), "metalp").unwrap(),
        "metalp",
        "a spelling the source's own keys use is not overwritten by the operator's"
    );
}

/// The forgery this exists to refuse: one portal's service registering under another's key
/// collides with the real rows on `(source_system, source_key)` and overwrites them.
#[test]
fn a_service_cannot_register_another_sources_rows() {
    let refused = source_system(&service(Some("metalp")), "cnet").unwrap_err();
    assert!(
        matches!(refused, crate::error::AppError::Forbidden(_)),
        "{refused:?}"
    );
}

#[test]
fn a_caller_that_speaks_for_no_source_names_one_in_the_request() {
    assert_eq!(source_system(&administrator(), "cnet").unwrap(), "cnet");
    assert!(source_system(&administrator(), "  ").is_err());
    assert_eq!(
        source_system(&service(None), "cnet").unwrap(),
        "cnet",
        "a credential minted before the declaration existed still registers"
    );
    let _ = TokenPermissions::default();
}

#[test]
fn a_lost_insert_is_told_apart_from_a_real_database_failure() {
    let duplicate = crudcrate::ApiError::database(sea_orm::DbErr::Custom(
        "error returned from database: 23505 duplicate key value violates unique constraint"
            .to_string(),
    ));
    assert!(super::lost_the_insert(&duplicate));
    let unrelated =
        crudcrate::ApiError::database(sea_orm::DbErr::Custom("connection closed".to_string()));
    assert!(!super::lost_the_insert(&unrelated));
    assert!(!super::lost_the_insert(&crudcrate::ApiError::bad_request(
        "23505".to_string()
    )));
}
