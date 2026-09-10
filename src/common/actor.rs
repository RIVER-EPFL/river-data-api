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
#[path = "tests/actor.rs"]
mod tests;
