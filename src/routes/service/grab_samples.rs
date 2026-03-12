use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{readings, site_parameters, sites};
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct GrabSampleRequest {
    pub site_id: Uuid,
    pub readings: Vec<GrabSampleReading>,
}

#[derive(Debug, Deserialize)]
pub struct GrabSampleReading {
    pub parameter_id: Uuid,
    pub sensor_id: Option<Uuid>,
    pub value: f64,
    pub time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct GrabSampleResponse {
    pub inserted: usize,
}

pub async fn insert_grab_samples(
    State(state): State<AppState>,
    Json(payload): Json<GrabSampleRequest>,
) -> AppResult<Json<GrabSampleResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings provided".to_string()));
    }

    // Validate site exists
    let site = sites::Entity::find_by_id(payload.site_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {} not found", payload.site_id)))?;

    // Validate all parameter_ids exist for this site
    let param_ids: Vec<Uuid> = payload.readings.iter().map(|r| r.parameter_id).collect();
    let site_params = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::ParameterId.is_in(param_ids.clone()))
        .all(&state.db)
        .await?;

    let valid_param_ids: std::collections::HashSet<Uuid> =
        site_params.iter().map(|sp| sp.parameter_id).collect();

    for r in &payload.readings {
        if !valid_param_ids.contains(&r.parameter_id) {
            return Err(AppError::BadRequest(format!(
                "Parameter {} is not configured for site {}",
                r.parameter_id, site.name
            )));
        }
    }

    // Build readings with measurement_type = 'spot'
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .map(|r| {
            use sea_orm::Set;
            readings::ActiveModel {
                site_id: Set(payload.site_id),
                parameter_id: Set(r.parameter_id),
                time: Set(r.time.into()),
                raw_value: Set(r.value),
                calibrated_value: Set(None),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(None),
                deployment_id: Set(None),
                logged: Set(Some(true)),
                measurement_type: Set(Some("spot".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                field_trip_id: Set(None),
            }
        })
        .collect();

    let total = models.len();

    match readings::Entity::insert_many(models)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                readings::Column::SiteId,
                readings::Column::ParameterId,
                readings::Column::Time,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec(&state.db)
        .await
    {
        Ok(_) => {
            tracing::info!(total, site = %site.name, "Grab samples inserted");
            Ok(Json(GrabSampleResponse { inserted: total }))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("None of the records") {
                Ok(Json(GrabSampleResponse { inserted: 0 }))
            } else {
                Err(AppError::Database(e))
            }
        }
    }
}
