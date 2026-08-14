//! Periodic identity reconciliation, the anti-backdoor backstop.
//!
//! Re-resolves every active linked identity against Keycloak and deactivates any whose user is gone,
//! disabled, or no longer holds a riverdata role. This bounds the revocation window even for users
//! who never issue another command (a command-active user is already caught within the authz cache
//! TTL; an alert-only user would otherwise keep receiving alerts indefinitely). Keycloak-unavailable
//! resolutions are skipped, never deactivated, so an outage can't mass-unlink.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use crate::common::AppState;

use super::authz::RoleResolution;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

/// One reconciliation pass, driven by the scheduled `identity_reconcile` job. Returns how many
/// identities were deactivated.
pub async fn sweep(state: &AppState) -> Result<usize, sea_orm::DbErr> {
    let rows = state
        .db
        .query_all(Statement::from_string(
            PG,
            "SELECT id, linked_keycloak_sub FROM telegram_identities WHERE is_active".to_string(),
        ))
        .await?;

    let mut deactivated = 0;
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        let sub: String = row.try_get("", "linked_keycloak_sub")?;
        if let Some(RoleResolution::Revoked) = state.authorizer.resolve(state, &sub).await {
            deactivate(&state.db, id).await?;
            state.authorizer.invalidate(&sub).await;
            deactivated += 1;
        }
    }
    Ok(deactivated)
}

async fn deactivate(db: &DatabaseConnection, id: Uuid) -> Result<(), sea_orm::DbErr> {
    db.execute(Statement::from_sql_and_values(
        PG,
        "UPDATE telegram_identities SET is_active = FALSE, updated_at = NOW() WHERE id = $1",
        [id.into()],
    ))
    .await?;
    Ok(())
}
