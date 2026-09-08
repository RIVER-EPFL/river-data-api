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
            let removed = res.rows_affected();
            outcome.revoked += removed as usize;
            if removed > 0 {
                // An access change is the one thing this sweep does that somebody may need to
                // read back, and it belongs in the entity trail rather than the reading ledger
                // (Q57, M164).
                db.execute_raw(Statement::from_sql_and_values(
                    PG,
                    "INSERT INTO change_audit (subject, change, old_value, new_value, changed_by) \
                     VALUES ($1, 'access_revoked', $2::jsonb, NULL, 'system')",
                    [
                        format!("push_subscriptions:{sub}").into(),
                        serde_json::json!({ "removed": removed }).to_string().into(),
                    ],
                ))
                .await?;
            }
            tracing::info!(sub = %sub, "push_reconcile: pruned subscriptions for revoked user");
        }
    }

    Ok(outcome)
}
