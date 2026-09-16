//! Where a registered row's provenance comes from.
//!
//! `(source_system, source_key)` is the identity a source holds its own rows by, and every
//! reconciliation rule in the project rests on it. The caller names the system in the request.

use crate::error::{AppError, AppResult};

/// The source system this registration is written under, as the request names it.
///
/// # Errors
///
/// `BadRequest` where the request names none.
pub fn source_system(claimed: &str) -> AppResult<String> {
    let claimed = claimed.trim();
    if claimed.is_empty() {
        return Err(AppError::BadRequest(
            "source_system must not be empty".to_string(),
        ));
    }
    Ok(claimed.to_string())
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
