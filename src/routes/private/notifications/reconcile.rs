//! Push subscription reconciliation. Runs on a schedule to prune subscriptions for users whose
//! Keycloak account is revoked or disabled.

use sea_orm::{ConnectionTrait, Statement};

use super::access::RoleResolution;
use crate::common::AppState;

const PG: sea_orm::DatabaseBackend = sea_orm::DatabaseBackend::Postgres;

#[derive(Default)]
pub struct SweepOutcome {
    pub revoked: usize,
}

impl SweepOutcome {
    pub fn total(&self) -> usize {
        self.revoked
    }
}

pub async fn sweep(state: &AppState) -> Result<SweepOutcome, sea_orm::DbErr> {
    let db = &state.db;
    let mut outcome = SweepOutcome::default();

    let subs = db
        .query_all_raw(Statement::from_string(
            PG,
            "SELECT DISTINCT keycloak_sub FROM web_push_subscriptions".to_string(),
        ))
        .await?;

    for row in subs {
        let sub: String = row.try_get("", "keycloak_sub")?;
        let resolution = state.authorizer.resolve(state, &sub).await;
        if matches!(resolution, Some(RoleResolution::Revoked)) {
            let res = db
                .execute_raw(Statement::from_sql_and_values(
                    PG,
                    "DELETE FROM web_push_subscriptions WHERE keycloak_sub = $1",
                    [sub.clone().into()],
                ))
                .await?;
            outcome.revoked += res.rows_affected() as usize;
            tracing::info!(sub = %sub, "push_reconcile: pruned subscriptions for revoked user");
        }
    }

    Ok(outcome)
}
