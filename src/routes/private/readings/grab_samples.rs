use axum::{Json, extract::State};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::routes::private::{data_streams, readings, samples, site_parameters, sites};
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
pub struct GrabSampleRequest {
    pub site_id: Uuid,
    pub created_by: Option<String>,
    pub readings: Vec<GrabSampleReading>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GrabSampleReading {
    pub parameter_id: Uuid,
    pub sensor_id: Option<Uuid>,
    pub value: f64,
    pub time: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GrabSampleResponse {
    pub inserted: usize,
    pub samples_created: usize,
}

/// Get or create a "grab_sample" stream for a given (site_id, parameter_id) pair.
async fn get_or_create_grab_stream(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    site_parameter_id: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let source_key = format!("{site_id}:{parameter_id}");

    if let Some(stream) = data_streams::Entity::find()
        .filter(data_streams::Column::SourceSystem.eq("grab_sample"))
        .filter(data_streams::Column::SourceKey.eq(&source_key))
        .one(db)
        .await?
    {
        // Auto-pair existing unpaired stream
        if stream.site_parameter_id.is_none()
            && let Some(sp_id) = site_parameter_id
        {
            let mut active: data_streams::ActiveModel = stream.clone().into();
            active.site_parameter_id = Set(Some(sp_id));
            active.paired_at = Set(Some(chrono::Utc::now().into()));
            active.updated_at = Set(chrono::Utc::now().into());
            active.update(db).await?;
        }
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
        site_parameter_id: Set(site_parameter_id),
        sensor_id: Set(None),
        is_active: Set(true),
        discovered_at: Set(now.into()),
        paired_at: Set(site_parameter_id.map(|_| now.into())),
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

/// Group readings by (parameter_id, time) and auto-create `samples` rows for
/// groups with 2+ readings. Returns a map from group key to sample_id.
async fn auto_create_samples(
    txn: &sea_orm::DatabaseTransaction,
    readings: &[GrabSampleReading],
    site_id: Uuid,
    created_by: Option<&str>,
) -> Result<HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Uuid>, AppError> {
    let mut groups: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), usize> = HashMap::new();
    for r in readings {
        *groups.entry((r.parameter_id, r.time)).or_default() += 1;
    }

    let mut sample_map = HashMap::new();
    for ((parameter_id, time), count) in groups {
        if count < 2 {
            continue;
        }
        let sample = samples::ActiveModel {
            id: Set(Uuid::new_v4()),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            collected_at: Set(time),
            label: Set(None),
            notes: Set(None),
            created_by: Set(created_by.map(String::from)),
            created_at: Set(Some(chrono::Utc::now())),
            mean: Set(None),
            stdev: Set(None),
            n: Set(0),
            min_value: Set(None),
            max_value: Set(None),
            updated_at: Set(None),
        };
        let sample = sample.insert(txn).await?;
        sample_map.insert((parameter_id, time), sample.id);
    }

    Ok(sample_map)
}

/// Insert field-collected grab sample readings (manual measurements with replicate sets).
/// Each request creates one Sample aggregate per parameter and uses dedicated "grab_sample"
/// streams. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/grab_samples",
    request_body = GrabSampleRequest,
    responses(
        (status = 200, description = "Counts of inserted readings and created Sample rows", body = GrabSampleResponse),
        (status = 400, description = "Empty readings, parameter not configured for site, or other validation"),
    ),
    tag = "ingestion"
)]
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
    let sp_lookup: HashMap<Uuid, Uuid> = site_params
        .iter()
        .map(|sp| (sp.parameter_id, sp.id))
        .collect();

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
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(r.parameter_id) {
            let sp_id = sp_lookup.get(&r.parameter_id).copied();
            let stream_id =
                get_or_create_grab_stream(&state.db, payload.site_id, r.parameter_id, sp_id)
                    .await?;
            entry.insert(stream_id);
        }
    }

    let txn = state.db.begin().await?;

    // Auto-create samples for replicate groups (2+ readings per parameter+time)
    let sample_map = auto_create_samples(
        &txn,
        &payload.readings,
        payload.site_id,
        payload.created_by.as_deref(),
    )
    .await?;

    let samples_created = sample_map.len();

    // Window-aware attribution for grabs that name a sensor: resolve calibration/deployment from the
    // sensor's windows at the grab time (site-fixed to payload.site_id), instead of writing NULL.
    // Grabs without a sensor_id keep NULL cal/deployment (manual lab values with no instrument).
    let grab_slots = {
        use crate::routes::private::sensors::operations::{resolve_windows_for_times, ResolvedSlot};
        let mut times_by_sensor: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            if let Some(sid) = r.sensor_id {
                times_by_sensor.entry(sid).or_default().push(r.time);
            }
        }
        let mut slots: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), ResolvedSlot> = HashMap::new();
        for (sid, times) in &times_by_sensor {
            let resolved =
                resolve_windows_for_times(&state.db, *sid, Some(payload.site_id), times)
                    .await
                    .unwrap_or_default();
            for (t, slot) in resolved {
                slots.insert((*sid, t), slot);
            }
        }
        slots
    };

    // Track replicate_index per (parameter_id, time) group for auto-assignment
    let mut index_counters: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), i16> = HashMap::new();

    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .map(|r| {
            let stream_id = stream_cache[&r.parameter_id];
            let group_key = (r.parameter_id, r.time);

            let sample_id = sample_map.get(&group_key).copied();

            // Auto-assign replicate_index: if part of a sample, start at 1
            let replicate_index = if let Some(idx) = r.replicate_index {
                idx
            } else if sample_id.is_some() {
                let counter = index_counters.entry(group_key).or_insert(0);
                *counter += 1;
                *counter
            } else {
                0
            };

            readings::ActiveModel {
                stream_id: Set(stream_id),
                site_id: Set(Some(payload.site_id)),
                parameter_id: Set(Some(r.parameter_id)),
                time: Set(r.time.into()),
                replicate_index: Set(replicate_index),
                raw_value: Set(r.value),
                calibrated_value: Set(None),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(r.sensor_id.and_then(|sid| grab_slots.get(&(sid, r.time)).and_then(|s| s.calibration_id))),
                deployment_id: Set(r.sensor_id.and_then(|sid| grab_slots.get(&(sid, r.time)).and_then(|s| s.deployment_id))),
                logged: Set(Some(true)),
                measurement_type: Set(Some("spot".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(sample_id),
            }
        })
        .collect();

    let total = models.len();

    let inserted = match readings::Entity::insert_many(models)
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
        Ok(rows) => rows as usize,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("None of the records") {
                0
            } else {
                return Err(AppError::Database(e));
            }
        }
    };

    txn.commit().await?;

    tracing::info!(total, inserted, samples_created, site = %site.name, "Grab samples inserted");
    Ok(Json(GrabSampleResponse {
        inserted,
        samples_created,
    }))
}
