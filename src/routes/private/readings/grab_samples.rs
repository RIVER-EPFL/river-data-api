use axum::{Json, extract::State};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::batch::{
    CurveClaim, Replace, admission, admit_standard_curves, readings_upsert,
};
use crate::routes::private::{
    data_streams, readings, readings::sample_groups, readings::samples, sensors::calibrations,
    sites, sites::parameters as site_parameters,
};

/// Grabs are spot measurements by definition: a bottle, not a logger cadence.
const GRAB_MEASUREMENT_TYPE: &str = "spot";

/// Posting to this endpoint is the declaration that a collection event happened.
const GRAB_IS_A_COLLECTION_EVENT: bool = true;

#[derive(Debug, Deserialize, ToSchema)]
pub struct GrabSampleRequest {
    pub site_id: Uuid,
    pub created_by: Option<String>,
    /// Stamped onto the samples rows this request creates or reuses.
    pub label: Option<String>,
    pub notes: Option<String>,
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
    /// The standard curve the operator fitted for this measurement, typically per microplate. It is
    /// applied on top of the instrument's base calibration, which the server resolves from the
    /// sensor's windows at `time`. The stored row carries the measured `raw_value`, both curve
    /// references and the value they produce together, so a recorded identity base and an
    /// unrecorded one stay distinguishable.
    #[serde(default)]
    pub standard_curve_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GrabSampleResponse {
    pub inserted: usize,
    pub samples_created: usize,
    /// The samples this request created, as opposed to reused. A caller that wants to act on
    /// exactly the rows it just wrote (the bot flags field submissions for review) can key on these
    /// rather than re-selecting by slot and time, which would also catch a concurrent write.
    #[serde(default)]
    pub created_sample_ids: Vec<Uuid>,
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
        measurement_type: Set(Some("spot".to_string())),
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

/// The samples row for this collection event, created if it is not already there, with its label and
/// notes refreshed when the request carries them.
///
/// Returns the row's id and whether this call is the one that created it.
///
/// The insert yields to a concurrent one rather than testing for the row first: two field entries
/// for the same (site, parameter, time) both see nothing, both insert, and the unique index on
/// those three columns then fails one of them, losing an entire grab to a 500. `DO NOTHING` is what
/// every other writer of `samples` does, and the read below picks up whichever row won.
async fn find_or_create_sample(
    txn: &sea_orm::DatabaseTransaction,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    created_by: Option<&str>,
    label: Option<&str>,
    notes: Option<&str>,
) -> Result<(Uuid, bool), AppError> {
    let candidate = samples::ActiveModel {
        id: Set(Uuid::new_v4()),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        collected_at: Set(time),
        label: Set(label.map(String::from)),
        notes: Set(notes.map(String::from)),
        created_by: Set(created_by.map(String::from)),
        created_at: Set(Some(chrono::Utc::now())),
        mean: Set(None),
        stdev: Set(None),
        n: Set(0),
        min_value: Set(None),
        max_value: Set(None),
        updated_at: Set(None),
    };
    let inserted = match samples::Entity::insert(candidate)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                samples::Column::SiteId,
                samples::Column::ParameterId,
                samples::Column::CollectedAt,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(txn)
        .await
    {
        Ok(rows) => rows > 0,
        // A conflict that inserted nothing is the expected outcome of re-posting a grab, not a
        // failure.
        Err(sea_orm::DbErr::RecordNotInserted) => false,
        Err(e) => return Err(AppError::Database(e)),
    };

    let existing = samples::Entity::find()
        .filter(samples::Column::SiteId.eq(site_id))
        .filter(samples::Column::ParameterId.eq(parameter_id))
        .filter(samples::Column::CollectedAt.eq(time))
        .one(txn)
        .await?
        .ok_or_else(|| {
            AppError::Internal("Failed to record the sample for this grab".to_string())
        })?;
    let sample_id = existing.id;

    if !inserted && (label.is_some() || notes.is_some()) {
        let mut active: samples::ActiveModel = existing.into();
        if let Some(l) = label {
            active.label = Set(Some(l.to_string()));
        }
        if let Some(n) = notes {
            active.notes = Set(Some(n.to_string()));
        }
        active.updated_at = Set(Some(chrono::Utc::now()));
        active.update(txn).await?;
    }

    Ok((sample_id, inserted))
}

/// Create a `samples` row per (parameter_id, time) group. Returns the group-to-sample map and how
/// many rows were created.
async fn auto_create_samples(
    txn: &sea_orm::DatabaseTransaction,
    readings: &[GrabSampleReading],
    site_id: Uuid,
    created_by: Option<&str>,
    label: Option<&str>,
    notes: Option<&str>,
) -> Result<
    (
        HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Uuid>,
        Vec<Uuid>,
    ),
    AppError,
> {
    let mut groups: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), usize> = HashMap::new();
    for r in readings {
        *groups.entry((r.parameter_id, r.time)).or_default() += 1;
    }

    let mut sample_map = HashMap::new();
    let mut created: Vec<Uuid> = Vec::new();
    for ((parameter_id, time), count) in groups {
        // A grab request is an operator recording a collection event, so every group is a sample
        // whether or not it was measured twice. Views that read grabs, the sensor-vs-grab export
        // and the curve filter among them, join through `samples`, so a grab without a row there
        // is invisible to them.
        if !sample_groups::forms_sample(GRAB_IS_A_COLLECTION_EVENT, count) {
            continue;
        }
        // Re-posting the same grab must reuse its sample, not accumulate empty duplicates.
        let (sample_id, is_new) =
            find_or_create_sample(txn, site_id, parameter_id, time, created_by, label, notes)
                .await?;
        sample_map.insert((parameter_id, time), sample_id);
        if is_new {
            created.push(sample_id);
        }
    }

    Ok((sample_map, created))
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
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<GrabSampleRequest>,
) -> AppResult<Json<GrabSampleResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings provided".to_string()));
    }

    // A project-scoped token may only write to a site within its project.
    enforce_project_scope_for_sites(&state.db, &scope, &[payload.site_id]).await?;

    for r in &payload.readings {
        admission::admit(r.time, r.value, Some(GRAB_MEASUREMENT_TYPE))?;
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
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(r.parameter_id)
        {
            let sp_id = sp_lookup.get(&r.parameter_id).copied();
            let stream_id =
                get_or_create_grab_stream(&state.db, payload.site_id, r.parameter_id, sp_id)
                    .await?;
            entry.insert(stream_id);
        }
    }

    let txn = state.db.begin().await?;

    // One samples row per (parameter, time) group in the request.
    let (sample_map, created_sample_ids) = auto_create_samples(
        &txn,
        &payload.readings,
        payload.site_id,
        payload.created_by.as_deref(),
        payload.label.as_deref(),
        payload.notes.as_deref(),
    )
    .await?;

    // Window-aware attribution for grabs that name a sensor: which deployment the instrument was on
    // at the grab time (site-fixed to payload.site_id), instead of writing NULL. Grabs without a
    // sensor_id keep NULL deployment (manual lab values with no instrument).
    let grab_slots = {
        use crate::routes::private::sensors::operations::{
            ResolvedSlot, resolve_windows_for_times,
        };
        let mut times_by_sensor: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            if let Some(sid) = r.sensor_id {
                times_by_sensor.entry(sid).or_default().push(r.time);
            }
        }
        let mut slots: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), ResolvedSlot> =
            HashMap::new();
        for (sid, times) in &times_by_sensor {
            let resolved = resolve_windows_for_times(&state.db, *sid, Some(payload.site_id), times)
                .await
                .unwrap_or_default();
            for (t, slot) in resolved {
                slots.insert((*sid, t), slot);
            }
        }
        slots
    };

    // Per-parameter time windows for the alarm episode reconstruction below (computed up front
    // because the readings vec is consumed building the insert models).
    let mut alarm_windows: HashMap<
        Uuid,
        (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
    > = HashMap::new();
    for r in &payload.readings {
        alarm_windows
            .entry(r.parameter_id)
            .and_modify(|(lo, hi)| {
                *lo = (*lo).min(r.time);
                *hi = (*hi).max(r.time);
            })
            .or_insert((r.time, r.time));
    }

    // The chosen standard curves, admitted by the one rule every writer of `standard_curve_id`
    // uses, so the correction can be applied server-side below. A grab is spot by construction, so
    // the only claims this path can be refused for are an unknown id, a curve fitted on another
    // instrument, and a curve on a grab that names no instrument at all.
    let claims: Vec<CurveClaim<'_>> = payload
        .readings
        .iter()
        .filter_map(|r| {
            r.standard_curve_id.map(|id| CurveClaim {
                standard_curve_id: id,
                sensor_id: r.sensor_id,
                measurement_type: GRAB_MEASUREMENT_TYPE,
            })
        })
        .collect();
    let standard_curves = admit_standard_curves(&state.db, &claims).await?;

    // The base calibration covering each grab that names an instrument, ranked by the one resolver
    // the ingest and reprocess paths use. Resolving it here is what lets the row carry both the id
    // and the value that id produced: a stamped calibration the stored value was never corrected by
    // is provenance that reads as true and is not.
    let base_curves = {
        let requests: Vec<(Uuid, Option<Uuid>, chrono::DateTime<chrono::Utc>)> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id.map(|sid| (sid, Some(r.parameter_id), r.time)))
            .collect();
        calibrations::resolver::resolve_many(&state.db, &requests).await?
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

            // Auto-assign replicate_index from 0 within the (parameter, time) group so every
            // group has an index-0 row and default (replicate_index = 0) queries see one point
            // per grab.
            let replicate_index = if let Some(idx) = r.replicate_index {
                idx
            } else {
                let counter = index_counters.entry(group_key).or_insert(0);
                let idx = *counter;
                *counter += 1;
                idx
            };

            // Both corrections, in the one order the arithmetic is defined in: the instrument's
            // base calibration, then the operator's standard curve on that result. A grab that
            // resolves neither is stored uncorrected, and `calibrated_value` stays NULL so a null
            // still means "no curve was applied" rather than "a curve happened to be identity".
            let base = r
                .sensor_id
                .and_then(|sid| base_curves.get(&(sid, Some(r.parameter_id), r.time)))
                .copied();
            let standard = r.standard_curve_id.map(|cid| {
                let c = &standard_curves[&cid];
                calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            let calibrated_value = (base.is_some() || standard.is_some())
                .then(|| calibrations::service::apply_curves(r.value, base, standard));

            readings::ActiveModel {
                standard_curve_id: Set(standard.map(|c| c.id)),
                stream_id: Set(stream_id),
                site_id: Set(Some(payload.site_id)),
                parameter_id: Set(Some(r.parameter_id)),
                time: Set(r.time.into()),
                replicate_index: Set(replicate_index),
                raw_value: Set(r.value),
                calibrated_value: Set(calibrated_value),
                sensor_id: Set(r.sensor_id),
                calibration_id: Set(base.map(|c| c.id)),
                deployment_id: Set(r
                    .sensor_id
                    .and_then(|sid| grab_slots.get(&(sid, r.time)).and_then(|s| s.deployment_id))),
                logged: Set(Some(true)),
                measurement_type: Set(Some(GRAB_MEASUREMENT_TYPE.to_string())),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(sample_id),
            }
        })
        .collect();

    let total = models.len();

    let inserted = match readings::Entity::insert_many(models)
        .on_conflict(readings_upsert(Replace::Nothing))
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

    // Readings the insert skipped on conflict still belong to the group's sample; linking them
    // fires the aggregate trigger so the stats cover every replicate. Scoped to spot readings:
    // a sonde reading sharing the grab's snapped timestamp must not be adopted into the sample,
    // or the trigger folds sensor data into the grab statistics.
    for ((parameter_id, time), sample_id) in &sample_map {
        txn.execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE readings SET sample_id = $1
              WHERE site_id = $2 AND parameter_id = $3 AND time = $4 AND sample_id IS NULL
                AND measurement_type = 'spot'",
            [
                (*sample_id).into(),
                payload.site_id.into(),
                (*parameter_id).into(),
                (*time).into(),
            ],
        ))
        .await?;
    }

    txn.commit().await?;

    // Event-driven open-alarm reconcile for the sampled slots (error-safe; backstop covers it),
    // plus historical episode reconstruction per slot so back-dated grabs land in alarm_events
    // like the batch/import paths. Inline rather than a tracked job to avoid one
    // reprocessing_jobs row per field campaign entry.
    if inserted > 0 {
        let alarm_slots: Vec<(Uuid, Uuid)> = stream_cache
            .keys()
            .map(|pid| (payload.site_id, *pid))
            .collect();
        crate::routes::private::alarms::sweeper::reconcile_and_notify(
            &state.db,
            &state.events,
            &alarm_slots,
        )
        .await;

        for (pid, (lo, hi)) in alarm_windows {
            if let Err(e) = crate::routes::private::alarms::episodes::evaluate_alarm_episodes(
                &state.db,
                payload.site_id,
                pid,
                lo,
                hi,
            )
            .await
            {
                tracing::warn!(error = %e, site_id = %payload.site_id, parameter_id = %pid, "alarm episode reconstruction failed");
            }
        }
    }

    if inserted > 0 {
        let site_id = payload.site_id;
        crate::common::cache::invalidate_prefix(&state, &format!("readings:{site_id}")).await;
        crate::common::cache::invalidate_prefix(&state, &format!("aggregates:{site_id}")).await;
    }

    let samples_created = created_sample_ids.len();
    tracing::info!(total, inserted, samples_created, site = %site.name, "Grab samples inserted");
    Ok(Json(GrabSampleResponse {
        inserted,
        samples_created,
        created_sample_ids,
    }))
}
