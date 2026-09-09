//! Where a registered row's provenance comes from.
//!
//! `(source_system, source_key)` is the identity a source holds its own rows by, and every
//! reconciliation rule in the project rests on it meaning what it says. An enrolled sync service
//! declares the system it speaks for once, on the credential it enrolls with (M167), and that
//! declaration is what its registrations are written under: a service cannot register another
//! source's rows, whatever its request says.
//!
//! A caller that speaks for no source, an administrator registering by hand, or a service on a
//! credential minted before the declaration existed, still names the system in the request. That
//! is the only remaining path by which the value comes from the body.

use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};

/// The source system this registration is written under.
///
/// A caller that speaks for a source decides which one it may write; where its request spells the
/// same name, that spelling is what is stored, because the rows the source already holds are keyed
/// under it.
///
/// # Errors
///
/// `BadRequest` where nobody names one, and `Forbidden` where the caller speaks for one source and
/// the request claims another.
pub fn source_system(auth: &AuthContext, claimed: &str) -> AppResult<String> {
    let claimed = claimed.trim();
    match auth.source_system() {
        Some(declared) if claimed.is_empty() => Ok(declared.to_string()),
        Some(declared) if claimed.eq_ignore_ascii_case(declared) => Ok(claimed.to_string()),
        Some(declared) => Err(AppError::Forbidden(format!(
            "this service is enrolled for {declared} and cannot register rows as {claimed}"
        ))),
        None if claimed.is_empty() => Err(AppError::BadRequest(
            "source_system must not be empty".to_string(),
        )),
        None => Ok(claimed.to_string()),
    }
}

/// Register a row under its provenance key, reporting what the write did.
///
/// The retry is B213: [`crudcrate::upsert`] reads the key and inserts where that finds nothing, so
/// two registrations of one key arriving together both read absent and the loser meets the unique
/// index. Its second attempt reads the row the winner stored and takes the comparison path, which
/// is the outcome the route promises. It comes out when the primitive itself conflicts.
///
/// # Errors
///
/// Whatever the registration returns, once the concurrent case is spent.
pub async fn register<R>(
    db: &sea_orm::DatabaseConnection,
    active: R::ActiveModelType,
) -> AppResult<(R, crudcrate::UpsertStatus)>
where
    R: crudcrate::CRUDResource,
    <R::EntityType as sea_orm::EntityTrait>::Model:
        sea_orm::IntoActiveModel<R::ActiveModelType>,
{
    match crudcrate::upsert::<R, _>(db, active.clone()).await {
        Err(e) if lost_the_insert(&e) => Ok(crudcrate::upsert::<R, _>(db, active).await?),
        other => Ok(other?),
    }
}

/// Whether a registration failed because another one stored the same key first.
fn lost_the_insert(error: &crudcrate::ApiError) -> bool {
    let crudcrate::ApiError::Database { internal, .. } = error else {
        return false;
    };
    let reported = internal.to_string();
    reported.contains("23505") || reported.contains("duplicate key value")
}

#[cfg(test)]
mod tests {
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
        assert_eq!(source_system(&service(Some("metalp")), "").unwrap(), "metalp");
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
        let unrelated = crudcrate::ApiError::database(sea_orm::DbErr::Custom(
            "connection closed".to_string(),
        ));
        assert!(!super::lost_the_insert(&unrelated));
        assert!(!super::lost_the_insert(&crudcrate::ApiError::bad_request(
            "23505".to_string()
        )));
    }
}
