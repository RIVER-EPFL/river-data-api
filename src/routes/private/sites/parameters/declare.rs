//! Slot-level sd-estimator declaration outside the audit-hold flow.
//!
//! `sd_estimator` is excluded from CRUD update because changing the declaration must also
//! recompute the slot's stored samples; this endpoint is the one path, writing the column and
//! enqueueing the tracked `sd_estimator_retag` in the same breath, exactly as the audit
//! resolution's slot scope does.

use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareSdEstimatorRequest {
    /// 'sample' (divisor n-1), 'population' (divisor n), or null to clear the declaration.
    /// Clearing leaves the slot undeclared: new statistics fall back to sample recorded as
    /// 'default', and stored samples keep the estimator they were computed with.
    pub estimator: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeclareSdEstimatorResponse {
    pub site_parameter_id: Uuid,
    pub estimator: Option<String>,
    pub previous: Option<String>,
    /// Samples the retag will recompute; 0 when clearing or nothing disagrees.
    pub samples_affected: i64,
    /// The tracked `sd_estimator_retag` job, present when a recompute was enqueued.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
}

#[utoipa::path(
    post,
    path = "/site_parameters/{id}/declare_sd_estimator",
    request_body = DeclareSdEstimatorRequest,
    responses(
        (status = 200, body = DeclareSdEstimatorResponse),
        (status = 400, description = "Estimator is not 'sample', 'population' or null"),
        (status = 404, description = "No site parameter with this id"),
    ),
    tag = "site_parameters"
)]
pub async fn declare_sd_estimator(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<DeclareSdEstimatorRequest>,
) -> AppResult<Json<DeclareSdEstimatorResponse>> {
    let estimator = match payload.estimator.as_deref() {
        None | Some("sample" | "population") => payload.estimator.clone(),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "estimator must be 'sample', 'population' or null, not '{other}'"
            )));
        }
    };

    let (previous, affected) = state
        .db
        .transaction::<_, (Option<String>, i64), sea_orm::DbErr>(|txn| {
            let estimator = estimator.clone();
            Box::pin(async move {
                let row = txn
                    .query_one(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT site_id, parameter_id, sd_estimator FROM site_parameters
                         WHERE id = $1 FOR UPDATE",
                        [id.into()],
                    ))
                    .await?
                    .ok_or_else(|| sea_orm::DbErr::RecordNotFound(id.to_string()))?;
                let site_id: Uuid = row.try_get("", "site_id")?;
                let parameter_id: Uuid = row.try_get("", "parameter_id")?;
                let previous: Option<String> = row.try_get("", "sd_estimator")?;

                txn.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE site_parameters SET sd_estimator = $2 WHERE id = $1",
                    [id.into(), estimator.clone().into()],
                ))
                .await?;

                // Counted inside the transaction the declaration lands in, so the number
                // reported is the one the retag will act on. A cleared declaration recomputes
                // nothing: stored samples keep the estimator they were computed with.
                let affected = if let Some(est) = &estimator {
                    txn.query_one(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT COUNT(*)::bigint AS n FROM samples
                         WHERE site_id = $1 AND parameter_id = $2
                           AND sd_estimator IS DISTINCT FROM $3
                           AND sd_estimator_source <> 'sample'",
                        [site_id.into(), parameter_id.into(), est.clone().into()],
                    ))
                    .await?
                    .map_or(Ok(0_i64), |row| row.try_get::<i64>("", "n"))?
                } else {
                    0
                };
                Ok((previous, affected))
            })
        })
        .await
        .map_err(|e| match e {
            sea_orm::TransactionError::Transaction(sea_orm::DbErr::RecordNotFound(_)) => {
                AppError::NotFound(format!("site parameter {id} not found"))
            }
            sea_orm::TransactionError::Connection(db) => AppError::from(db),
            sea_orm::TransactionError::Transaction(db) => AppError::from(db),
        })?;

    let job_id = if let Some(est) = &estimator
        && affected > 0
    {
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "sd_estimator_retag",
            None,
            None,
            &serde_json::json!({
                "estimator": est,
                "site_parameter_ids": [id],
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(DeclareSdEstimatorResponse {
        site_parameter_id: id,
        estimator,
        previous,
        samples_affected: affected,
        job_id,
    }))
}
