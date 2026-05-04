use axum::{Json, extract::State};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::common::AppState;
use crate::entity::{data_streams, field_trips, readings, samples, site_parameters, sites};
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
    #[serde(default)]
    pub replicate_index: Option<i16>,
    #[serde(default)]
    pub sample_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct FieldTripBatchResponse {
    pub field_trip_id: Uuid,
    pub total_inserted: usize,
    pub stations_count: usize,
    pub samples_created: usize,
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

    let txn = state.db.begin().await?;

    // Create the field trip record
    let field_trip = field_trips::ActiveModel {
        id: Set(Uuid::new_v4()),
        date: Set(payload.date),
        participants: Set(payload.participants),
        notes: Set(payload.notes.clone()),
        created_by: Set(payload.created_by.clone()),
        created_at: Set(chrono::Utc::now()),
    };

    let field_trip = field_trip.insert(&txn).await?;
    let field_trip_id = field_trip.id;

    // Resolve stream_ids
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    let stations_count = payload.stations.len();

    for station in &payload.stations {
        for r in &station.readings {
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
        }
    }

    // Auto-create samples per station for replicate groups
    let mut total_samples_created = 0usize;
    let mut all_models: Vec<readings::ActiveModel> = Vec::new();

    for station in payload.stations {
        // Group this station's readings by (parameter_id, time)
        let mut groups: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Vec<SampleReading>> =
            HashMap::new();
        for r in station.readings {
            groups.entry((r.parameter_id, r.time)).or_default().push(r);
        }

        // Create samples for groups with 2+ readings
        let mut station_sample_map: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Uuid> =
            HashMap::new();
        for (key, group) in &groups {
            let all_have_sample = group.iter().all(|r| r.sample_id.is_some());
            if group.len() >= 2 && !all_have_sample {
                let sample = samples::ActiveModel {
                    id: Set(Uuid::new_v4()),
                    site_id: Set(station.site_id),
                    parameter_id: Set(key.0),
                    collected_at: Set(key.1),
                    label: Set(None),
                    notes: Set(None),
                    field_trip_id: Set(Some(field_trip_id)),
                    created_by: Set(payload.created_by.clone()),
                    created_at: Set(Some(chrono::Utc::now())),
                    mean: Set(None),
                    stdev: Set(None),
                    n: Set(0),
                    min_value: Set(None),
                    max_value: Set(None),
                    updated_at: Set(None),
                };
                let sample = sample.insert(&txn).await?;
                station_sample_map.insert(*key, sample.id);
                total_samples_created += 1;
            }
        }

        // Build reading models with auto-assigned sample_id and replicate_index
        for (key, group) in groups {
            let mut index_counter: i16 = 0;
            for r in group {
                let stream_id = stream_cache[&(station.site_id, r.parameter_id)];
                let sample_id =
                    r.sample_id.or_else(|| station_sample_map.get(&key).copied());

                let replicate_index = if let Some(idx) = r.replicate_index {
                    idx
                } else if sample_id.is_some() {
                    index_counter += 1;
                    index_counter
                } else {
                    0
                };

                all_models.push(readings::ActiveModel {
                    stream_id: Set(stream_id),
                    site_id: Set(Some(station.site_id)),
                    parameter_id: Set(Some(r.parameter_id)),
                    time: Set(r.time.into()),
                    replicate_index: Set(replicate_index),
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
                    sample_id: Set(sample_id),
                });
            }
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
            .exec_without_returning(&txn)
            .await
        {
            Ok(rows) => {
                tracing::info!(
                    total,
                    inserted = rows,
                    stations_count,
                    samples_created = total_samples_created,
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

    txn.commit().await?;

    Ok(Json(FieldTripBatchResponse {
        field_trip_id,
        total_inserted: total,
        stations_count,
        samples_created: total_samples_created,
    }))
}
