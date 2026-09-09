//! Who is writing, for the duration of one request.
//!
//! The change-audit triggers read the actor from `river.actor`
//! (`migration/src/m20260910_000015_entity_change_audit.rs`), a transaction-local setting the
//! database cannot know on its own. The auth middleware puts the caller's label in a task-local for
//! the request it authenticated, and every transaction that may fire one of those triggers declares
//! it on the way in. A write with no request behind it (a job, a sync sweep, a migration) declares
//! nothing, and the trail records that as an absent actor rather than a wrong one.

use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use crate::common::middleware::AuthContext;

tokio::task_local! {
    static ACTOR: String;
}

/// The caller's label. One spelling, [`AuthContext::label`], reached from here because most
/// writers hold the context rather than the identity.
#[must_use]
pub fn label(auth: &AuthContext) -> String {
    auth.label()
}

/// Run `work` with `actor` as the current one.
pub async fn scoped<F: Future>(actor: String, work: F) -> F::Output {
    ACTOR.scope(actor, work).await
}

/// The current actor, or `None` outside a request.
#[must_use]
pub fn current() -> Option<String> {
    ACTOR.try_with(Clone::clone).ok()
}

/// Tell `conn`'s transaction who is writing. `set_config(..., true)` is `SET LOCAL` in a form that
/// takes a parameter, so the label is bound rather than interpolated. Outside a request there is
/// nobody to name and nothing is set.
pub async fn declare<C: ConnectionTrait>(conn: &C) -> Result<(), sea_orm::DbErr> {
    if let Some(actor) = current() {
        conn.execute_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT set_config('river.actor', $1, true)",
            [actor.into()],
        ))
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        use crate::routes::private::readings::decisions::Origin;
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
}
