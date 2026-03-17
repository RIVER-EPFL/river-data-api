use axum::{Json, extract::State};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, field_trips, readings, site_parameters, sites};
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize)]
pub struct FieldTripBatchRequest {
    pub date: chrono::NaiveDate,
    pub participants: Option<String>,
    pub notes: Option<String>,
    pub created_by: Option<String>,
    pub stations: Vec<StationSamples>,
}

#[derive(Debug, Deserialize)]
pub struct StationSamples {
    pub site_id: Uuid,
    pub readings: Vec<SampleReading>,
}

#[derive(Debug, Deserialize)]
pub struct SampleReading {
    pub parameter_id: Uuid,
    pub sensor_id: Option<Uuid>,
    pub value: f64,
    pub time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct FieldTripBatchResponse {
    pub field_trip_id: Uuid,
    pub total_inserted: usize,
    pub stations_count: usize,
}

/// Get or create a "field_trip" stream for a given (site_id, parameter_id) pair.
async fn get_or_create_field_trip_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("field_trip"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        return Ok(stream.id);
    }

    let now = chrono::Utc::now();
    let model = data_streams::ActiveModel {
        id: Set(Uuid::new_v4()),
        source_system: Set("field_trip".to_string()),
        source_key: Set(source_key),
        source_name: Set(Some("Field trip sample".to_string())),
        source_path: Set(None),
        metadata: Set(serde_json::json!({})),
        site_parameter_id: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(None),
        last_data_time: Set(None),
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
        .filter(data_streams::Column::SourceSystem.eq("field_trip"))
        .filter(data_streams::Column::SourceKey.eq(format!("{site_id}:{parameter_id}")))
        .one(db)
        .await?
        .ok_or_else(|| AppError::Internal("Failed to create field trip stream".to_string()))?;

    Ok(stream.id)
}

pub async fn create_field_trip_batch(
    State(state): State<AppState>,
    Json(payload): Json<FieldTripBatchRequest>,
) -> AppResult<Json<FieldTripBatchResponse>> {
    if payload.stations.is_empty() {
        return Err(AppError::BadRequest("No stations provided".to_string()));
    }

    // Validate all sites exist
    let site_ids: Vec<Uuid> = payload.stations.iter().map(|s| s.site_id).collect();
    let found_sites = sites::Entity::find()
        .filter(sites::Column::Id.is_in(site_ids.clone()))
        .all(&state.db)
        .await?;

    if found_sites.len() != site_ids.len() {
        let found_ids: std::collections::HashSet<Uuid> = found_sites.iter().map(|s| s.id).collect();
        let missing: Vec<_> = site_ids.iter().filter(|id| !found_ids.contains(id)).collect();
        return Err(AppError::BadRequest(format!(
            "Sites not found: {missing:?}"
        )));
    }

    // Validate parameters for each site
    for station in &payload.stations {
        if station.readings.is_empty() {
            continue;
        }
        let param_ids: Vec<Uuid> = station.readings.iter().map(|r| r.parameter_id).collect();
        let site_params = site_parameters::Entity::find()
            .filter(site_parameters::Column::SiteId.eq(station.site_id))
            .filter(site_parameters::Column::ParameterId.is_in(param_ids.clone()))
            .all(&state.db)
            .await?;

        let valid_ids: std::collections::HashSet<Uuid> =
            site_params.iter().map(|sp| sp.parameter_id).collect();

        for r in &station.readings {
            if !valid_ids.contains(&r.parameter_id) {
                let site_name = found_sites
                    .iter()
                    .find(|s| s.id == station.site_id)
                    .map_or("unknown", |s| s.name.as_str());
                return Err(AppError::BadRequest(format!(
                    "Parameter {} is not configured for site {}",
                    r.parameter_id, site_name
                )));
            }
        }
    }

    // Create the field trip record
    let field_trip = field_trips::ActiveModel {
        id: Set(Uuid::new_v4()),
        date: Set(payload.date),
        participants: Set(payload.participants),
        notes: Set(payload.notes),
        created_by: Set(payload.created_by),
        created_at: Set(chrono::Utc::now()),
    };

    let field_trip = field_trip.insert(&state.db).await?;
    let field_trip_id = field_trip.id;

    // Resolve stream_ids and insert readings
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    let mut all_models: Vec<readings::ActiveModel> = Vec::new();
    let stations_count = payload.stations.len();

    for station in payload.stations {
        for r in station.readings {
            let key = (station.site_id, r.parameter_id);
            if !stream_cache.contains_key(&key) {
                let stream_id = get_or_create_field_trip_stream(
                    &state.db,
                    station.site_id,
                    r.parameter_id,
                )
                .await?;
                stream_cache.insert(key, stream_id);
            }
            let stream_id = stream_cache[&key];

            all_models.push(readings::ActiveModel {
                stream_id: Set(stream_id),
                site_id: Set(Some(station.site_id)),
                parameter_id: Set(Some(r.parameter_id)),
                time: Set(r.time.into()),
                replicate_index: Set(0),
                raw_value: Set(r.value),
                calibrated_value: Set(None),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(None),
                deployment_id: Set(None),
                logged: Set(Some(true)),
                measurement_type: Set(Some("spot".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                field_trip_id: Set(Some(field_trip_id)),
            });
        }
    }

    let total = all_models.len();

    if !all_models.is_empty() {
        match readings::Entity::insert_many(all_models)
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
                tracing::info!(
                    total,
                    inserted = rows,
                    stations_count,
                    field_trip_id = %field_trip_id,
                    "Field trip batch inserted"
                );
            }
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("None of the records") {
                    return Err(AppError::Database(e));
                }
            }
        }
    }

    Ok(Json(FieldTripBatchResponse {
        field_trip_id,
        total_inserted: total,
        stations_count,
    }))
}
