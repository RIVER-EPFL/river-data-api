use super::{current, label, scoped};
use crate::common::middleware::AuthContext;
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

fn keycloak(email: Option<&str>) -> AuthContext {
    AuthContext::Keycloak {
        roles: Vec::new(),
        sub: "sub-1".to_string(),
        email: email.map(str::to_string),
        email_verified: email.is_some(),
        grants: Arc::new(HashSet::new()),
    }
}

/// One label for one caller, whatever route recorded it: four copies of this used to disagree,
/// two writing the literal "keycloak" where the other two wrote the subject, so the same person
/// appeared under two names depending on which endpoint they used. A sync service is named for
/// the source it speaks for, which is what its registrations already write into `created_by`.
#[test]
fn every_caller_has_exactly_one_name() {
    assert_eq!(label(&keycloak(Some("evan@epfl.ch"))), "evan@epfl.ch");
    assert_eq!(
        label(&keycloak(None)),
        "sub-1",
        "no email is still a person, named by the id that identifies them"
    );
    let token_id = Uuid::new_v4();
    assert_eq!(
        label(&AuthContext::ApiToken {
            token_id,
            permissions: crate::common::middleware::TokenPermissions::default(),
            project_scope: None,
            rate_limit_per_second: None,
        }),
        format!("token:{token_id}")
    );
    let service_id = Uuid::new_v4();
    assert_eq!(
        label(&AuthContext::SyncService {
            service_id,
            source_system: Some("metalp".to_string()),
        }),
        "sync:metalp"
    );
    assert_eq!(
        label(&AuthContext::SyncService {
            service_id,
            source_system: None,
        }),
        format!("token:{service_id}"),
        "an undeclared service is still named by the identity it authenticated with"
    );
}

/// What a caller writes is answered by the caller, not by the route it reached: a sync service
/// correcting a value through the same handler a person uses is recorded as sync.
#[test]
fn a_caller_says_what_its_writes_are_recorded_as() {
    use crate::routes::private::readings::models::Origin;
    assert_eq!(keycloak(Some("evan@epfl.ch")).origin(), Origin::Manual);
    assert_eq!(
        AuthContext::SyncService {
            service_id: Uuid::new_v4(),
            source_system: Some("cnet".to_string()),
        }
        .origin(),
        Origin::Sync
    );
}

/// Only an enrolled service speaks for a source, and only for the one it enrolled with.
#[test]
fn the_source_system_belongs_to_the_service_and_to_nobody_else() {
    assert_eq!(keycloak(Some("evan@epfl.ch")).source_system(), None);
    assert_eq!(
        AuthContext::ApiToken {
            token_id: Uuid::new_v4(),
            permissions: crate::common::middleware::TokenPermissions::default(),
            project_scope: None,
            rate_limit_per_second: None,
        }
        .source_system(),
        None
    );
    assert_eq!(
        AuthContext::SyncService {
            service_id: Uuid::new_v4(),
            source_system: Some("metalp".to_string()),
        }
        .source_system(),
        Some("metalp")
    );
}

#[tokio::test]
async fn the_actor_is_readable_below_the_handler_and_nowhere_else() {
    assert_eq!(current(), None, "outside a request nobody is writing");
    let seen = scoped("evan@epfl.ch".to_string(), async {
        // Anything the request calls, however deep, reads the same answer.
        async { current() }.await
    })
    .await;
    assert_eq!(seen.as_deref(), Some("evan@epfl.ch"));
    assert_eq!(current(), None, "and the scope ends with the request");
}
