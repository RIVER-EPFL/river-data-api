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
    <R::EntityType as sea_orm::EntityTrait>::Model: sea_orm::IntoActiveModel<R::ActiveModelType>,
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
#[path = "tests/provenance.rs"]
mod tests;
