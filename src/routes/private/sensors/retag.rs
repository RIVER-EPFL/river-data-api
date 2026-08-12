//! Bulk data-frequency reclassification: set `data_frequency` on a set of sensors and (optionally)
//! retag their existing readings via the tracked `measurement_retag` job, which also refreshes
//! continuous aggregates over the affected window.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::scope;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
pub struct RetagFrequencyRequest {
    pub sensor_ids: Vec<Uuid>,
    /// 'high' (field stream → continuous) or 'low' (lab/campaign → spot).
    pub data_frequency: String,
    /// Also retag the sensors' existing readings and refresh aggregates (tracked job).
    /// When false only future ingestion is affected.
    #[serde(default)]
    pub retag_existing: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetagFrequencyResponse {
    pub sensors_updated: u64,
    pub data_frequency: String,
    /// The tracked `measurement_retag` job, when `retag_existing` was requested.
    pub job_id: Option<Uuid>,
}

/// Classify sensors as low- or high-frequency in bulk. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sensors/retag_frequency",
    request_body = RetagFrequencyRequest,
    responses(
        (status = 200, description = "Sensors reclassified", body = RetagFrequencyResponse),
        (status = 400, description = "Invalid data_frequency or empty sensor list"),
    ),
    tag = "sensors"
)]
pub async fn retag_frequency(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<RetagFrequencyRequest>,
) -> AppResult<Json<RetagFrequencyResponse>> {
    if req.sensor_ids.is_empty() {
        return Err(AppError::BadRequest("sensor_ids must not be empty".to_string()));
    }
    // A never-deployed instrument belongs to no project, so it stays reachable as inventory;
    // one deployed only outside the caller's grants does not.
    for sensor_id in &req.sensor_ids {
        scope::require_target_in_scope(
            &scope,
            &scope::project_of_sensor(&state.db, *sensor_id).await?,
            scope::Unowned::Allow,
            "Sensor",
        )?;
    }
    if !matches!(req.data_frequency.as_str(), "high" | "low") {
        return Err(AppError::BadRequest(format!(
            "invalid data_frequency '{}' (expected high or low)",
            req.data_frequency
        )));
    }

    let sensors_updated = state
        .db
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sensors SET data_frequency = $1 WHERE id = ANY($2)",
            [req.data_frequency.clone().into(), req.sensor_ids.clone().into()],
        ))
        .await?
        .rows_affected();

    let job_id = if req.retag_existing {
        let target = if req.data_frequency == "low" { "spot" } else { "continuous" };
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "measurement_retag",
            None,
            None,
            &serde_json::json!({
                "sensor_ids": req.sensor_ids,
                "target": target,
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(RetagFrequencyResponse {
        sensors_updated,
        data_frequency: req.data_frequency,
        job_id,
    }))
}
