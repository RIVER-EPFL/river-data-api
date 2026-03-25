use axum::{Json, extract::State};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, readings, site_parameters, sites};
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct GrabSampleRequest {
    pub site_id: Uuid,
    pub field_trip_id: Option<Uuid>,
    pub readings: Vec<GrabSampleReading>,
}

#[derive(Debug, Deserialize)]
pub struct GrabSampleReading {
    pub parameter_id: Uuid,
    pub sensor_id: Option<Uuid>,
    pub value: f64,
    pub time: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
}

#[derive(Debug, Serialize)]
pub struct GrabSampleResponse {
    pub inserted: usize,
}

/// Get or create a "grab_sample" stream for a given (site_id, parameter_id) pair.
async fn get_or_create_grab_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        return Ok(stream.id);
    }

    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let model = data_streams::ActiveModel {
        id: Set(id),
        source_system: Set("grab_sample".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some("Grab sample".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        site_parameter_id: Set(None),
        sensor_id: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
        pairing_plan_id: Set(None),
        created_at: Set(now.into()),
        updated_at: Set(now.into()),
    };

    data_streams::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                data_streams::Column::SourceSystem,
                data_streams::Column::SourceKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(db)
        .await
        .map_err(AppError::Database)?;

    let stream = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(format!("{site_id}:{parameter_id}")))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create grab sample stream".to_string()))?;

    Ok(stream.id)
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

    // Resolve stream_ids for each unique (site_id, parameter_id)
    let mut stream_cache: HashMap<Uuid, Uuid> = HashMap::new();
    for r in &payload.readings {
        if !stream_cache.contains_key(&r.parameter_id) {
            let stream_id =
                get_or_create_grab_stream(&state.db, payload.site_id, r.parameter_id).await?;
            stream_cache.insert(r.parameter_id, stream_id);
        }
    }

    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .map(|r| {
            let stream_id = stream_cache[&r.parameter_id];
            readings::ActiveModel {
                stream_id: Set(stream_id),
                site_id: Set(Some(payload.site_id)),
                parameter_id: Set(Some(r.parameter_id)),
                time: Set(r.time.into()),
                replicate_index: Set(r.replicate_index.unwrap_or(0)),
                raw_value: Set(r.value),
                calibrated_value: Set(None),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(None),
                deployment_id: Set(None),
                logged: Set(Some(true)),
                measurement_type: Set(Some("spot".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                field_trip_id: Set(payload.field_trip_id),
            }
        })
        .collect();

    let total = models.len();

    match readings::Entity::insert_many(models)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                readings::Column::StreamId,
                readings::Column::Time,
                readings::Column::ReplicateIndex,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(&state.db)
        .await
    {
        Ok(rows) => {
            tracing::info!(total, inserted = rows, site = %site.name, "Grab samples inserted");
            Ok(Json(GrabSampleResponse { inserted: rows as usize }))
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
