use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, EntityTrait, Set};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::{AppEvent, AppState};
use crate::routes::private::readings;
use crate::error::AppResult;
use crate::routes::private::data_streams::services::get_or_create_api_stream;

/// How to handle readings that collide with an existing (stream_id, time, replicate_index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConflictMode {
    /// Keep the existing row, drop the incoming one.
    #[default]
    Skip,
    /// Replace the stored values with the incoming ones.
    Overwrite,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BatchReadingsRequest {
    pub readings: Vec<ReadingInput>,
    /// Behaviour on (stream_id, time, replicate_index) collisions. Defaults to `skip`.
    #[serde(default)]
    pub conflict: ConflictMode,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReadingInput {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub raw_value: f64,
    pub calibrated_value: Option<f64>,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    #[serde(default)]
    pub replicate_index: Option<i16>,
    #[serde(default)]
    pub sample_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BatchReadingsResponse {
    pub inserted: usize,
    /// Existing rows replaced because `conflict = overwrite`. Always 0 in `skip` mode.
    pub overwritten: usize,
}

const BATCH_SIZE: usize = 1000;

/// Build the `ON CONFLICT` clause for the readings PK. In `skip` mode collisions are dropped;
/// in `overwrite` mode the value columns are replaced from the incoming row.
pub(crate) fn readings_on_conflict(mode: ConflictMode) -> sea_orm::sea_query::OnConflict {
    let mut clause = sea_orm::sea_query::OnConflict::columns([
        readings::Column::StreamId,
        readings::Column::Time,
        readings::Column::ReplicateIndex,
    ]);
    match mode {
        ConflictMode::Skip => {
            clause.do_nothing();
        }
        ConflictMode::Overwrite => {
            clause.update_columns([
                readings::Column::RawValue,
                readings::Column::CalibratedValue,
            ]);
        }
    }
    clause.to_owned()
}

/// Batch insert readings keyed by (site_id, parameter_id). Auto-creates "api" streams when
/// a (site, parameter) pair has none. 10MB body limit. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/readings/batch",
    request_body = BatchReadingsRequest,
    responses(
        (status = 200, description = "Inserted count", body = BatchReadingsResponse),
        (status = 400, description = "Timestamp outside [-10 years, +1 day] window"),
        (status = 413, description = "Body exceeds 10MB limit"),
    ),
    tag = "ingestion"
)]
pub async fn insert_batch_readings(
    State(state): State<AppState>,
    Json(payload): Json<BatchReadingsRequest>,
) -> AppResult<Json<BatchReadingsResponse>> {
    // Validate timestamps are within reasonable bounds
    let now = chrono::Utc::now();
    let min_time = now - chrono::Duration::days(365 * 10); // 10 years ago
    let max_time = now + chrono::Duration::days(1); // 1 day in the future
    for r in &payload.readings {
        if r.time < min_time || r.time > max_time {
            return Err(crate::error::AppError::BadRequest(format!(
                "Reading timestamp {} is outside valid range ({} to {})",
                r.time.to_rfc3339(),
                min_time.to_rfc3339(),
                max_time.to_rfc3339(),
            )));
        }
    }

    // Collect unique (site_id, parameter_id) pairs and resolve stream_ids
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();

    for r in &payload.readings {
        let key = (r.site_id, r.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, r.site_id, r.parameter_id).await?;
            entry.insert(stream_id);
        }
    }

    // Collect unique (site_id, time) pairs for derived auto-compute
    let site_timestamps_for_derived: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = {
        let mut map: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            map.entry(r.site_id).or_default().push(r.time);
        }
        for timestamps in map.values_mut() {
            timestamps.sort();
            timestamps.dedup();
        }
        map
    };

    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .map(|r| {
            let stream_id = stream_cache[&(r.site_id, r.parameter_id)];
            readings::ActiveModel {
                stream_id: Set(stream_id),
                site_id: Set(Some(r.site_id)),
                parameter_id: Set(Some(r.parameter_id)),
                time: Set(r.time.into()),
                replicate_index: Set(r.replicate_index.unwrap_or(0)),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(r.calibrated_value),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(r.calibration_id),
                deployment_id: Set(r.deployment_id),
                logged: Set(Some(true)),
                measurement_type: Set(Some("continuous".to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(r.sample_id),
            }
        })
        .collect();

    let total = models.len();
    let mut inserted = 0usize;
    let mut overwritten = 0usize;
    let conflict = payload.conflict;

    for chunk in models.chunks(BATCH_SIZE) {
        // In overwrite mode `rows_affected` counts both inserts and updates, so the count of
        // keys already present (looked up before the write) tells us how many were replaced.
        let pre_existing = if conflict == ConflictMode::Overwrite {
            count_existing(&state.db, chunk).await?
        } else {
            0
        };

        match readings::Entity::insert_many(chunk.to_vec())
            .on_conflict(readings_on_conflict(conflict))
            .exec_without_returning(&state.db)
            .await
        {
            Ok(rows) => {
                let affected = rows as usize;
                inserted += affected.saturating_sub(pre_existing);
                overwritten += pre_existing;
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("None of the records") {
                    // All duplicates in this chunk
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(crate::error::AppError::Database(e));
                }
            }
        }
    }

    tracing::info!(total, inserted, overwritten, "Batch readings insert complete");

    // Emit DataIngested events per unique (site_id, parameter_id) pair
    if inserted > 0 || overwritten > 0 {
        for ((site_id, parameter_id), &stream_id) in &stream_cache {
            let _ = state.events.send(AppEvent::DataIngested {
                site_id: Some(*site_id),
                parameter_id: Some(*parameter_id),
                stream_id,
                count: inserted + overwritten,
            });
        }
    }

    // Invalidate response cache and auto-compute derived parameters for all affected sites.
    // Cascade also runs when rows were overwritten, since their downstream values are now stale.
    if inserted > 0 || overwritten > 0 {
        let affected_site_ids: std::collections::HashSet<Uuid> =
            stream_cache.keys().map(|(site_id, _)| *site_id).collect();
        for site_id in &affected_site_ids {
            crate::common::cache::invalidate_prefix(&state, &format!("readings:{site_id}")).await;
            crate::common::cache::invalidate_prefix(&state, &format!("aggregates:{site_id}")).await;
        }

        // Auto-compute derived values for affected sites and refresh aggregates, tracked as a job.
        let total_ts: usize = site_timestamps_for_derived.values().map(Vec::len).sum();
        let earliest = site_timestamps_for_derived
            .values()
            .flatten()
            .min()
            .copied();
        let job_id = Uuid::new_v4();
        let job_total = i32::try_from(total_ts).unwrap_or(i32::MAX);
        state
            .db
            .execute(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status, total, progress) \
                 VALUES ($1, NULL, 'batch_derived', NULL, 'pending', $2, 0)",
                [job_id.into(), job_total.into()],
            ))
            .await?;
        let _ = state.events.send(AppEvent::JobCreated { job_id });

        let db_clone = state.db.clone();
        let events = state.events.clone();
        tokio::spawn(async move {
            let _ = db_clone
                .execute(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                    [job_id.into()],
                ))
                .await;

            let mut progress = 0i32;
            for (site_id, timestamps) in site_timestamps_for_derived {
                for time in timestamps {
                    if let Err(e) =
                        crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                            &db_clone, site_id, time,
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            site_id = %site_id,
                            time = %time,
                            "Failed to auto-compute derived values after batch insert"
                        );
                    }
                    progress += 1;
                    if progress % 500 == 0 {
                        let _ = db_clone
                            .execute(sea_orm::Statement::from_sql_and_values(
                                sea_orm::DatabaseBackend::Postgres,
                                "UPDATE reprocessing_jobs SET progress = $1 WHERE id = $2",
                                [progress.into(), job_id.into()],
                            ))
                            .await;
                        let _ = events.send(AppEvent::JobProgress {
                            job_id,
                            status: "running".to_string(),
                            progress: Some(progress),
                            total: Some(job_total),
                        });
                    }
                }
            }

            crate::common::sync_state::refresh_continuous_aggregates(&db_clone, earliest).await;

            let _ = db_clone
                .execute(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'completed', progress = total, \
                     completed_at = NOW() WHERE id = $1",
                    [job_id.into()],
                ))
                .await;
            let _ = events.send(AppEvent::JobCompleted {
                job_id,
                status: "completed".to_string(),
                readings_updated: None,
                error_message: None,
            });
        });
    }

    Ok(Json(BatchReadingsResponse {
        inserted,
        overwritten,
    }))
}

/// Count how many of the chunk's (stream_id, time, replicate_index) keys already exist, so the
/// caller can split `rows_affected` into inserts vs overwrites in `overwrite` mode.
async fn count_existing(
    db: &sea_orm::DatabaseConnection,
    chunk: &[readings::ActiveModel],
) -> AppResult<usize> {
    use sea_orm::{ColumnTrait, Condition, QueryFilter, QuerySelect, sea_query::Expr};

    if chunk.is_empty() {
        return Ok(0);
    }

    let mut condition = Condition::any();
    for m in chunk {
        let (sea_orm::ActiveValue::Set(stream_id), sea_orm::ActiveValue::Set(time), sea_orm::ActiveValue::Set(rep)) =
            (m.stream_id.clone(), m.time.clone(), m.replicate_index.clone())
        else {
            continue;
        };
        condition = condition.add(
            Condition::all()
                .add(readings::Column::StreamId.eq(stream_id))
                .add(readings::Column::Time.eq(time))
                .add(readings::Column::ReplicateIndex.eq(rep)),
        );
    }

    let count = readings::Entity::find()
        .select_only()
        .column_as(Expr::col(readings::Column::StreamId).count(), "n")
        .filter(condition)
        .into_tuple::<i64>()
        .one(db)
        .await?
        .unwrap_or(0);

    Ok(usize::try_from(count).unwrap_or(0))
}
