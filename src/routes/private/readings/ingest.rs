use axum::{Json, extract::State};
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set, Statement,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::aggregates::{self, Window};
use crate::common::middleware::{IsSyncService, ProjectScope, enforce_project_scope_for_sites};
use crate::common::{AppEvent, AppState};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::batch::{Replace, admission, readings_upsert};
use crate::routes::private::sensors::calibrations::{
    self, resolver,
    service::{Curve, apply_curves},
};
use crate::routes::private::sensors::operations::{
    resolve_slot_owner_for_times, resolve_windows_for_times,
};
use crate::routes::private::sensors::standard_curves;
use crate::routes::private::{data_streams, readings, readings::status_events};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestReadingsRequest {
    pub stream_id: Uuid,
    pub readings: Vec<IngestReading>,
    /// Update existing rows at the same (stream, time, replicate) key instead of skipping
    /// them, so source-side corrections propagate on re-sync. Sync-service callers only;
    /// flag state and sample links on the existing row are preserved.
    #[serde(default)]
    pub overwrite: bool,
    /// The writer declaring these readings collection events: spot replicate groups sharing an
    /// instant form a `samples` row from the first reading, like `/grab_samples`. Sync-service
    /// callers only; without it a group still forms a sample once it carries two replicates.
    #[serde(default)]
    pub collection: bool,
    /// Per-instant expectations from the source portal's own precomputed statistics. Each audited
    /// group's mean/sd is recomputed over the values about to be stored and compared; a
    /// disagreeing group is admitted and recorded as a `replicate_audit_holds` row for review
    /// (`pending` when the stream is paired, `deferred` until pairing otherwise). Sync-service
    /// callers only.
    #[serde(default)]
    pub audit: Option<Vec<crate::routes::private::sync::replicate_audit::GroupAudit>>,
    /// A completeness claim: these readings are the source's complete content for this stream
    /// over `[from, to)`. The server diffs stored content against the payload and converges —
    /// new rows insert, changed values correct in place, rows absent at source are stamped
    /// withdrawn (never deleted). Sync-service callers only; spot streams only. Without it the
    /// request is a bare append, exactly the old semantics.
    #[serde(default)]
    pub window: Option<crate::routes::private::readings::reconcile::SourceWindow>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestReading {
    pub time: chrono::DateTime<Utc>,
    pub raw_value: f64,
    #[serde(default)]
    pub replicate_index: i16,
    pub sensor_id: Option<Uuid>,
    pub calibration_id: Option<Uuid>,
    pub deployment_id: Option<Uuid>,
    /// Per-reading override ('continuous' | 'spot' | 'derived'). Omit to resolve from the
    /// stream's measurement_type, then the owning sensor's data_frequency.
    #[serde(default)]
    pub measurement_type: Option<String>,
    /// The lab standard curve that corrects this reading, for sync services replaying portal
    /// measurements that carried one. Held to the grab rules: the reading must be a spot
    /// measurement on the instrument the curve was fitted on, and the stored value is recomputed
    /// from the curve's coefficients. An inadmissible claim skips the reading and is counted.
    #[serde(default)]
    pub standard_curve_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestResponse {
    pub inserted: usize,
    /// Readings dropped by admission (out-of-window timestamp, non-finite value, unknown
    /// measurement_type, unknown calibration_id). Reported rather than raised so a cursor-driven
    /// caller can advance past them, and counted so the loss is never silent.
    #[serde(default)]
    pub skipped: usize,
    /// One entry per kind of rejection with its count, never one per reading, so the response
    /// stays a fixed size however large the batch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_reasons: Vec<String>,
    /// Always 0: the replicate audit admits every group and records disagreements for review.
    /// Retained because the sync protocol (`river-data-core`) reports a held count per cycle.
    #[serde(default)]
    pub held: usize,
    /// Windowed diff: stored rows whose source value changed, corrected in place.
    #[serde(default)]
    pub changed: usize,
    /// Windowed diff: stored rows absent from the claimed window, stamped withdrawn.
    #[serde(default)]
    pub withdrawn: usize,
    /// Windowed diff: stored rows the payload re-sent unchanged.
    #[serde(default)]
    pub unchanged: usize,
    /// Windowed diff: stored rows the funnel refused or the backend dropped, left untouched.
    #[serde(default)]
    pub retained: usize,
    /// The window the server accepted, echoed so a connector can detect an API image that
    /// silently ignored the claim (which would downgrade the source to append mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_window: Option<crate::routes::private::readings::reconcile::SourceWindow>,
    pub stream_id: Uuid,
    pub paired: bool,
}

const BATCH_SIZE: usize = 1000;

/// Batched insert. With `overwrite`, conflicting rows are updated in place on the value
/// and attribution columns; operator state (is_flagged, flag_reason, sample_id) is never
/// touched so a correction cannot clear a flag or unlink a sample. Without it the conflict is
/// `Replace::Nothing`, so a resync of the same rows is a no-op: `replicate_index` is the source's
/// column position and nothing renumbers it, so a replayed reading carries the same primary key.
async fn insert_reading_chunks<C: ConnectionTrait>(
    conn: &C,
    models: &[readings::ActiveModel],
    replace: Replace,
) -> Result<usize, AppError> {
    let overwrite = replace != Replace::Nothing;
    let conflict = readings_upsert(replace);

    let mut inserted = 0usize;
    for chunk in models.chunks(BATCH_SIZE) {
        match readings::Entity::insert_many(chunk.to_vec())
            .on_conflict(conflict.clone())
            .exec_without_returning(conn)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if !overwrite && msg.contains("None of the records") {
                    // All duplicates in this chunk
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                    return Err(AppError::Database(e));
                }
            }
        }
    }
    Ok(inserted)
}

/// Stream-based data ingestion. Inserts readings keyed by `stream_id`. If the stream is
/// paired to a `site_parameter`, readings are stamped with `site_id`/`parameter_id`, and each is
/// corrected by whichever of the owning instrument's curves covers its own time; a reading no curve
/// covers is stored uncorrected. Unpaired streams insert with `site_id = NULL` (and won't show up
/// in continuous aggregates until paired). Requires `write_data`.
#[utoipa::path(
    post,
    path = "/ingest",
    request_body = IngestReadingsRequest,
    responses(
        (status = 200, description = "Inserted count and pairing state. Inadmissible readings (out-of-window timestamp, non-finite value, unknown measurement_type, unknown calibration_id) are skipped and counted in `skipped`, not refused", body = IngestResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_readings(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    IsSyncService(is_sync_service): IsSyncService,
    Json(mut payload): Json<IngestReadingsRequest>,
) -> AppResult<Json<IngestResponse>> {
    if payload.overwrite && !is_sync_service {
        return Err(AppError::Forbidden(
            "overwrite is restricted to sync services".to_string(),
        ));
    }
    if payload.collection && !is_sync_service {
        return Err(AppError::Forbidden(
            "collection is restricted to sync services; grab entry goes through /grab_samples"
                .to_string(),
        ));
    }
    if payload.audit.is_some() && !is_sync_service {
        return Err(AppError::Forbidden(
            "audit is restricted to sync services".to_string(),
        ));
    }
    if payload.window.is_some() && !is_sync_service {
        return Err(AppError::Forbidden(
            "window is restricted to sync services".to_string(),
        ));
    }
    if let Some(window) = &payload.window
        && window.from >= window.to
    {
        return Err(AppError::BadRequest(
            "window.from must be before window.to".to_string(),
        ));
    }

    // An empty payload without a claim is nothing to do. With a window it is a claim the source
    // holds nothing there, which the diff must judge against what is stored.
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(IngestResponse {
            inserted: 0,
            skipped: 0,
            skipped_reasons: Vec::new(),
            held: 0,
            changed: 0,
            withdrawn: 0,
            unchanged: 0,
            retained: 0,
            accepted_window: None,
            stream_id: payload.stream_id,
            paired: false,
        }));
    }

    let db = &state.db;

    // Look up stream to get pairing info
    let stream = data_streams::Entity::find_by_id(payload.stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let (site_id, parameter_id) = resolve_stream_slot(db, stream.site_parameter_id).await?;
    let paired = site_id.is_some();

    // Withdrawal is confined to spot rows by a database CHECK; the continuous aggregates exclude
    // spot, which is what keeps a retraction structurally unreachable by a rollup. A window on a
    // non-spot stream is therefore refused rather than half-honoured.
    if payload.window.is_some() && stream.measurement_type.as_deref() != Some("spot") {
        return Err(AppError::BadRequest(
            "A completeness window is only accepted for streams declared measurement_type \
             'spot'; continuous sources stay append-only"
                .to_string(),
        ));
    }

    // A project-scoped token may only ingest into a stream paired to a site within its project.
    // An unpaired stream has no project, so a scoped token is rejected outright.
    enforce_ingest_scope(&state.db, &scope, site_id).await?;

    // Admission is per reading here, not per request: the caller replays from `last_data_time`,
    // which only advances on success, so refusing a batch for one bad row stalls the stream.
    // The cursor is taken from the survivors, so a future timestamp cannot latch it.
    // Tallied by kind because the per-reading messages carry the offending value and cannot group.
    let submitted = payload.readings.len();
    let mut counts: Vec<(admission::RejectionKind, usize)> = Vec::new();
    // Keys the funnel refused, remembered so the diff classifies them `retained` rather than
    // withdrawn: a key the request carried and the funnel refused is not absent at source.
    let mut rejected_keys: std::collections::HashSet<(chrono::DateTime<Utc>, i16)> =
        std::collections::HashSet::new();
    let track_rejections = payload.window.is_some();
    let now = Utc::now();
    payload.readings.retain(|r| {
        match admission::rejection_kind_at(now, r.time, r.raw_value, r.measurement_type.as_deref())
        {
            None => true,
            Some(kind) => {
                record_rejection(&mut counts, kind);
                if track_rejections {
                    rejected_keys.insert((r.time, r.replicate_index));
                }
                false
            }
        }
    });

    // Under a completeness claim, two payload rows at one key would classify one submitted row
    // twice. The last occurrence wins (backends emit source-id order, so last is deterministic);
    // the losers are counted so the receipt arithmetic closes.
    if payload.window.is_some() {
        let mut seen: std::collections::HashSet<(chrono::DateTime<Utc>, i16)> =
            std::collections::HashSet::new();
        let mut keep = vec![false; payload.readings.len()];
        for (i, r) in payload.readings.iter().enumerate().rev() {
            keep[i] = seen.insert((r.time, r.replicate_index));
        }
        if keep.iter().any(|k| !k) {
            let mut i = 0;
            payload.readings.retain(|_| {
                let kept = keep[i];
                i += 1;
                if !kept {
                    record_rejection(&mut counts, admission::RejectionKind::DuplicateKey);
                }
                kept
            });
        }
    }

    // Everything downstream reads `payload.readings`, so a batch that is entirely inadmissible has
    // no work left: return before the window-resolution queries rather than run them over nothing.
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    // Window-aware attribution: resolve calibration/deployment/site per reading TIME from the
    // sensor's windows, agreeing with reprocess_sensor_readings. The stream's frozen sensor_id is the
    // owner; cal/deployment/site come from whichever window covers each timestamp.
    let resolved = if let Some(stream_sensor) = stream.sensor_id {
        let times: Vec<chrono::DateTime<Utc>> = payload.readings.iter().map(|r| r.time).collect();
        resolve_windows_for_times(db, stream_sensor, None, &times)
            .await
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    // Fallback when the stream carries no frozen sensor: attribute by the (site, parameter)
    // deployment timeline so readings still land owned when a deployment covers their time.
    let slot_owner = match (stream.sensor_id, site_id, parameter_id) {
        (None, Some(s), Some(p)) => {
            let times: Vec<chrono::DateTime<Utc>> =
                payload.readings.iter().map(|r| r.time).collect();
            resolve_slot_owner_for_times(db, s, p, &times)
                .await
                .unwrap_or_default()
        }
        _ => std::collections::HashMap::new(),
    };

    // The curve covering each reading's own time, ranked by the one resolver the set-based
    // reprocess UPDATEs use. A value stored here and the value a later reprocess recomputes are
    // therefore the same number, so a reading is correct the moment it lands rather than only
    // after the next reprocess.
    let curves = {
        let requests: Vec<(Uuid, Option<Uuid>, chrono::DateTime<Utc>)> = payload
            .readings
            .iter()
            .filter_map(|r| {
                let owner = slot_owner.get(&r.time);
                let sensor = r
                    .sensor_id
                    .or(stream.sensor_id)
                    .or_else(|| owner.and_then(|o| o.sensor_id))?;
                Some((sensor, parameter_id, r.time))
            })
            .collect();
        resolver::resolve_many(db, &requests).await?
    };

    // A caller that names its own calibration is taken at its word about which curve applies, but
    // the stored value is still computed from that curve's coefficients rather than trusted or left
    // empty. Reference and value come from one curve on every reading, so nothing can store a
    // correction its `calibration_id` did not produce.
    let declared_curves: HashMap<Uuid, Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.calibration_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            HashMap::new()
        } else {
            calibrations::Entity::find()
                .filter(calibrations::Column::Id.is_in(ids))
                .all(db)
                .await?
                .into_iter()
                .map(|c| {
                    (
                        c.id,
                        Curve {
                            id: c.id,
                            slope: c.slope,
                            intercept: c.intercept,
                        },
                    )
                })
                .collect()
        }
    };

    // A deleted calibration never reappears, so refusing the request here would stall the cursor
    // permanently. Second pass because deciding it needs the calibration rows queried above.
    payload.readings.retain(|r| match r.calibration_id {
        Some(id) if !declared_curves.contains_key(&id) => {
            record_rejection(&mut counts, admission::RejectionKind::UnknownCalibration);
            if track_rejections {
                rejected_keys.insert((r.time, r.replicate_index));
            }
            false
        }
        _ => true,
    });
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    // Sensor-frequency defaults for every sensor a reading could resolve to (explicit, stream, or
    // slot owner), fetched in one query. Applied when neither the reading nor the stream declares
    // a measurement_type.
    let sensor_types = {
        let mut candidate_sensors: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(stream.sensor_id)
            .chain(slot_owner.values().filter_map(|o| o.sensor_id))
            .collect();
        candidate_sensors.sort_unstable();
        candidate_sensors.dedup();
        crate::routes::private::readings::measurement::measurement_types_for_sensors(
            db,
            &candidate_sensors,
        )
        .await?
    };

    // Standard curve claims, held to the grab rules (fitted on the reading's instrument, spot
    // measurement) but skipped-and-counted rather than refused: a wrong claim stays wrong on
    // every retry, and refusing the batch would stall the stream's cursor behind it.
    let standard_curves_by_id: HashMap<Uuid, Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.standard_curve_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            HashMap::new()
        } else {
            let rows = standard_curves::Entity::find()
                .filter(standard_curves::Column::Id.is_in(ids))
                .all(db)
                .await?;
            let by_id: HashMap<Uuid, &standard_curves::Model> =
                rows.iter().map(|c| (c.id, c)).collect();
            payload.readings.retain(|r| {
                let Some(id) = r.standard_curve_id else {
                    return true;
                };
                let sensor_id = r
                    .sensor_id
                    .or(stream.sensor_id)
                    .or_else(|| slot_owner.get(&r.time).and_then(|o| o.sensor_id));
                let measurement_type =
                    crate::routes::private::readings::measurement::resolve_measurement_type(
                        r.measurement_type.as_deref(),
                        stream.measurement_type.as_deref(),
                        sensor_id,
                        &sensor_types,
                    );
                let admissible = by_id
                    .get(&id)
                    .is_some_and(|c| Some(c.sensor_id) == sensor_id)
                    && measurement_type == readings::sample_groups::SPOT;
                if !admissible {
                    record_rejection(&mut counts, admission::RejectionKind::InvalidStandardCurve);
                    if track_rejections {
                        rejected_keys.insert((r.time, r.replicate_index));
                    }
                }
                admissible
            });
            rows.into_iter()
                .map(|c| {
                    (
                        c.id,
                        Curve {
                            id: c.id,
                            slope: c.slope,
                            intercept: c.intercept,
                        },
                    )
                })
                .collect()
        }
    };
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    // Build reading models
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .iter()
        .map(|r| {
            let slot = resolved.get(&r.time);
            let owner = slot_owner.get(&r.time);
            let sensor_id = r
                .sensor_id
                .or(stream.sensor_id)
                .or_else(|| owner.and_then(|o| o.sensor_id));
            // The one curve this reading is stored against: the caller's if it named one, else
            // whichever window covers its own time. Both `calibration_id` and `calibrated_value`
            // are derived from it, so a later reprocess re-resolving the same windows recomputes
            // what is already stored instead of disagreeing with it. The deployment-derived slot
            // and slot owner resolve calibrations by time alone, blind to the reading's parameter,
            // so they are not consulted for a curve; they still answer for the deployment.
            //
            // No curve at all: `calibrated_value` stays NULL, which is what a reprocess over the
            // same windows would leave too. Consumers read COALESCE(calibrated_value, raw_value),
            // so the raw value is still what is served.
            let curve = r
                .calibration_id
                .and_then(|id| declared_curves.get(&id).copied())
                .or_else(|| {
                    sensor_id.and_then(|s| curves.get(&(s, parameter_id, r.time)).copied())
                });
            // A hand-picked lab curve composes on top of whatever base calibration covers the
            // reading, as on /readings/batch and /grab_samples: instrument correction first, the
            // curve on its result. Reference and value still move together.
            let standard = r
                .standard_curve_id
                .and_then(|id| standard_curves_by_id.get(&id).copied());
            readings::ActiveModel {
                standard_curve_id: Set(standard.map(|c| c.id)),
                collection_event_id: Set(None),
                withdrawn_at: Set(None),
                withdrawn_reason: Set(None),
                stream_id: Set(payload.stream_id),
                time: Set(r.time.into()),
                replicate_index: Set(r.replicate_index),
                // Pairing is what attributes a reading to a site, so an unpaired stream stores its
                // readings unattributed even when a deployment of its sensor covers their time.
                // Within a paired stream the deployment decides which site, since a sensor can move
                // between sites while the stream keeps pointing at one slot.
                site_id: Set(site_id.map(|paired| slot.and_then(|s| s.site_id).unwrap_or(paired))),
                parameter_id: Set(parameter_id),
                raw_value: Set(r.raw_value),
                calibrated_value: Set(match (curve, standard) {
                    (None, None) => None,
                    (base, standard) => Some(apply_curves(r.raw_value, base, standard)),
                }),
                sensor_id: Set(sensor_id),
                calibration_id: Set(curve.map(|c| c.id)),
                deployment_id: Set(r
                    .deployment_id
                    .or_else(|| slot.and_then(|s| s.deployment_id))
                    .or_else(|| owner.and_then(|o| o.deployment_id))),
                logged: Set(Some(true)),
                measurement_type: Set(Some(
                    crate::routes::private::readings::measurement::resolve_measurement_type(
                        r.measurement_type.as_deref(),
                        stream.measurement_type.as_deref(),
                        sensor_id,
                        &sensor_types,
                    ),
                )),
                is_flagged: Set(Some(false)),
                flag_reason: Set(None),
                sample_id: Set(None),
            }
        })
        .collect();

    // The replicate audit: recompute each audited group's statistics over the values about to be
    // stored, compare against the portal's claim, and admit the group either way. Served
    // statistics are trigger-computed from the stored replicates, so a disagreement questions the
    // portal's aggregate cells, not the data, and withholding would only hide measurements from
    // the people waiting on them. A mismatch records a hold for review (pending when paired,
    // deferred until pairing); a group that matches again at source supersedes its open hold; a
    // group an operator already ruled on (acknowledged or remediated) is left alone unless the
    // portal's expected statistics have moved since the ruling, which opens a fresh hold.
    if let Some(audits) = payload.audit.as_deref().filter(|a| !a.is_empty()) {
        use crate::routes::private::sync::replicate_audit as audit;

        let mut group_values: HashMap<chrono::DateTime<Utc>, Vec<audit::ReplicateValue>> =
            HashMap::new();
        for m in &models {
            if let (
                sea_orm::ActiveValue::Set(time),
                sea_orm::ActiveValue::Set(raw),
                sea_orm::ActiveValue::Set(index),
            ) = (&m.time, &m.raw_value, &m.replicate_index)
            {
                let value = match &m.calibrated_value {
                    sea_orm::ActiveValue::Set(Some(v)) => *v,
                    _ => *raw,
                };
                group_values
                    .entry(time.with_timezone(&Utc))
                    .or_default()
                    .push(audit::ReplicateValue {
                        index: *index,
                        value,
                    });
            }
        }

        let audit_times: Vec<chrono::DateTime<Utc>> = audits.iter().map(|a| a.time).collect();
        let holds_by_time: HashMap<chrono::DateTime<Utc>, audit::LatestHold> =
            audit::latest_holds(db, payload.stream_id, &audit_times)
                .await?
                .into_iter()
                .map(|h| (h.time, h))
                .collect();

        for a in audits {
            let values = group_values
                .get(&a.time)
                .map_or(&[] as &[audit::ReplicateValue], Vec::as_slice);
            let numbers: Vec<f64> = values.iter().map(|v| v.value).collect();
            let stats = audit::group_stats(&numbers);
            let agree = audit::stats_agree(a.expected_mean, stats.mean, audit::DEFAULT_REL_TOL)
                && audit::stats_agree_with(
                    a.expected_sd,
                    stats.sd,
                    audit::SD_REL_TOL,
                    audit::SD_ABS_TOL,
                )
                && a.expected_n
                    .is_none_or(|expected| i64::try_from(stats.n) == Ok(expected));
            let mismatch = audit::GroupMismatch {
                time: a.time,
                expected_mean: a.expected_mean,
                expected_sd: a.expected_sd,
                expected_n: a.expected_n,
                computed_mean: stats.mean,
                computed_sd: stats.sd,
                n: stats.n,
                values: values.to_vec(),
            };
            let hold_status = if paired { "pending" } else { "deferred" };
            match (agree, holds_by_time.get(&a.time)) {
                (true, Some(hold)) if matches!(hold.status.as_str(), "pending" | "deferred") => {
                    audit::close_hold(db, hold.id, "superseded").await?;
                }
                (true, _) => {}
                // The operator's decision stands against re-detection of the SAME disagreement.
                // A cycle whose expected statistics moved is new evidence the decision never
                // covered, so it opens a fresh hold beside the terminal one.
                (false, Some(hold))
                    if matches!(hold.status.as_str(), "acknowledged" | "remediated") =>
                {
                    if audit::expected_changed(&hold.expected, a) {
                        audit::upsert_hold(db, payload.stream_id, &mismatch, hold_status).await?;
                    }
                }
                (false, _) => {
                    audit::upsert_hold(db, payload.stream_id, &mismatch, hold_status).await?;
                }
            }
        }
    }
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    let total = models.len();

    // Spot readings on a paired stream can form samples: replicate groups sharing an instant get
    // a `samples` row (mean/stdev/n maintained by the readings triggers), which is what the
    // serving paths and the UI whiskers read. A group needs no index-0 row: `replicate_index` is
    // the source's column position and is never renumbered. Scoped to this batch's time window;
    // identity of a collection event is (site, parameter, instant), so pre-existing rows at the
    // slot join the group. Unpaired streams skip this (site_id is NULL, the pairing backfill
    // materialises).
    let sample_window = if paired {
        let spot_times = models
            .iter()
            .filter_map(|m| match (&m.measurement_type, &m.time) {
                (sea_orm::ActiveValue::Set(Some(t)), sea_orm::ActiveValue::Set(time))
                    if t == readings::sample_groups::SPOT =>
                {
                    Some(time.with_timezone(&Utc))
                }
                _ => None,
            });
        spot_times.clone().min().zip(spot_times.max())
    } else {
        None
    };

    // Overwrite updates existing rows, the windowed diff corrects and withdraws, and the sample
    // stamping UPDATE reaches back-dated groups; all can touch compressed chunks, so they run
    // inside one transaction with the decompression cap lifted, like every other back-dated
    // write path. The diff, the writes and the receipt commit together or not at all.
    let mut diff_outcome: Option<crate::routes::private::readings::reconcile::DiffOutcome> = None;
    let inserted = if payload.overwrite || sample_window.is_some() || payload.window.is_some() {
        let (n, diff) = crate::common::bulk_write::guarded(db, async |txn| {
            use crate::routes::private::readings::reconcile;
            let diff = match &payload.window {
                Some(window) => {
                    let admitted: Vec<reconcile::AdmittedRow> = payload
                        .readings
                        .iter()
                        .map(|r| ((r.time, r.replicate_index), r.raw_value, r.standard_curve_id))
                        .collect();
                    Some(
                        reconcile::run_windowed_diff(
                            txn,
                            payload.stream_id,
                            window,
                            &admitted,
                            &rejected_keys,
                        )
                        .await?,
                    )
                }
                None => None,
            };
            let replace = if payload.overwrite {
                Replace::ValuesAndAttribution
            } else if diff.as_ref().is_some_and(|d| d.apply_changed) {
                // The windowed path retires Replace::Nothing: a differing value is a correction
                // and it is applied, through the clause that never touches flags or sample links.
                Replace::ValuesAndAttribution
            } else {
                Replace::Nothing
            };
            let n = insert_reading_chunks(txn, &models, replace).await?;
            if let Some((lo, hi)) = sample_window {
                let binds: Vec<sea_orm::Value> = vec![
                    payload.stream_id.into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(lo).into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(hi).into(),
                ];
                readings::sample_groups::materialise_samples(
                    txn,
                    "r.stream_id = $1 AND r.time >= $2 AND r.time <= $3",
                    binds.clone(),
                    payload.collection,
                )
                .await?;
                // Each source row maps onto one collection event (D7). A sync service replaying
                // a portal row writes a portal_sync event; any other writer is a person.
                crate::routes::private::collection_events::attach::attach_collection_events(
                    txn,
                    "r.stream_id = $1 AND r.time >= $2 AND r.time <= $3",
                    binds,
                    if is_sync_service {
                        crate::routes::private::collection_events::attach::EventSource::PortalSync
                    } else {
                        crate::routes::private::collection_events::attach::EventSource::Manual
                    },
                )
                .await?;
            }
            if let (Some(window), Some(d)) = (&payload.window, &diff) {
                let rejected_total: usize = counts.iter().map(|(_, n)| n).sum();
                let rejected_json = serde_json::Value::Object(
                    counts
                        .iter()
                        .map(|(k, n)| (k.as_str().to_string(), serde_json::json!(n)))
                        .collect(),
                );
                reconcile::write_receipt(
                    txn,
                    payload.stream_id,
                    window,
                    submitted,
                    d,
                    rejected_total,
                    &rejected_json,
                )
                .await?;
            }
            Ok((n, diff))
        })
        .await?;
        diff_outcome = diff;
        n
    } else {
        insert_reading_chunks(db, &models, Replace::Nothing).await?
    };

    let diff_changed = diff_outcome.as_ref().map_or(0, |d| d.changed);
    let diff_withdrawn = diff_outcome.as_ref().map_or(0, |d| d.withdrawn);
    let diff_reinstated = diff_outcome.as_ref().map_or(0, |d| d.reinstated);
    // What this pass did to served content, the gate every post-write side effect reads: a
    // correction or a withdrawal with `inserted == 0` still rewrote history.
    let effect =
        inserted > 0 || diff_changed > 0 || diff_withdrawn > 0 || diff_reinstated > 0;

    // Sample formation retroactively changes served historical points (the group's mean replaces
    // the lone value), so bounded cached responses cannot be left to expire on TTL. Withdrawals
    // and reinstatements rewrite served history the same way.
    if effect && (sample_window.is_some() || payload.window.is_some()) {
        state.response_cache.invalidate_all();
    }

    // Corrections rewrite history that bounded queries may have cached, and replace values the
    // rollups have already materialised. A windowed pass that corrected values did exactly what
    // an overwrite does, whatever `inserted` says.
    if (payload.overwrite && inserted > 0) || diff_changed > 0 {
        state.response_cache.invalidate_all();

        // Best-effort, like the alarm reconstruction below it: the rows are committed and the
        // cursor is about to advance past them, so a refresh that loses a lock to the janitor must
        // not turn a successful write into a 500 that replays the same batch forever. The rollups
        // converge on the next scheduled refresh.
        let times = payload.readings.iter().map(|r| r.time);
        if let (Some(lo), Some(hi)) = (times.clone().min(), times.max()) {
            // The upsert leaves a hand-picked curve standing, and this correction resolved only a
            // base, so the value is recomposed from whichever curves the row ends up carrying.
            if let Err(e) = calibrations::service::recompose_from_own_curves(
                &state.db,
                "TRUE",
                "r.stream_id = $1 AND r.time >= $2 AND r.time <= $3",
                vec![
                    payload.stream_id.into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(lo).into(),
                    sea_orm::prelude::DateTimeWithTimeZone::from(hi).into(),
                ],
            )
            .await
            {
                tracing::warn!(error = %e, "recompose after overwrite failed");
            }
            if let Err(e) = aggregates::refresh(&state.db, Window::Range(lo, hi)).await {
                tracing::warn!(error = %e, %lo, %hi, "aggregate refresh after overwrite failed");
            }
        }
    }

    // Emit ingestion event
    if effect {
        let _ = state.events.send(AppEvent::DataIngested {
            site_id,
            parameter_id,
            stream_id: payload.stream_id,
            count: inserted,
        });

        // Event-driven open-alarm reconcile for this slot (error-safe; backstop still covers it),
        // plus historical episode reconstruction over the ingested window so back-dated breaches
        // land in alarm_events like the batch/import paths. Inline rather than a tracked job:
        // single ingest fires every sync cycle per stream and would spam reprocessing_jobs.
        if let (Some(s), Some(p)) = (site_id, parameter_id) {
            crate::routes::private::alarms::sweeper::reconcile_and_notify(
                &state.db,
                &state.events,
                &[(s, p)],
            )
            .await;

            let times = payload.readings.iter().map(|r| r.time);
            if let (Some(lo), Some(hi)) = (times.clone().min(), times.max())
                && let Err(e) = crate::routes::private::alarms::episodes::evaluate_alarm_episodes(
                    &state.db, s, p, lo, hi,
                )
                .await
            {
                tracing::warn!(error = %e, site_id = %s, parameter_id = %p, "alarm episode reconstruction failed");
            }
        }
    }

    // Update last_data_time on the stream. Every group is admitted (audit disagreements are
    // review records, not gates), so the cursor always advances to the batch's newest instant.
    if let Some(max_time) = payload.readings.iter().map(|r| r.time).max() {
        let should_update = stream
            .last_data_time
            .map(|t| max_time > t.with_timezone(&Utc))
            .unwrap_or(true);

        if should_update {
            let mut active: data_streams::ActiveModel = stream.into();
            active.last_data_time = Set(Some(max_time.into()));
            active.updated_at = Set(Utc::now().into());
            if let Err(e) = active.update(db).await {
                tracing::warn!(error = %e, "Failed to update stream last_data_time");
            }
        }
    }

    // Auto-compute derived parameters for newly ingested timestamps (batched), tracked as a job.
    // Spawn-guard: skip entirely when the site has no active derived parameter, the job would
    // compute nothing, and this is the dominant source of empty `ingest_derived` jobs.
    if paired
        && effect
        && let Some(sid) = site_id
        && crate::routes::private::parameters::derived::janitor::site_has_active_derived(db, sid)
            .await
            .unwrap_or(true)
    {
        let mut unique_timestamps: Vec<chrono::DateTime<Utc>> =
            payload.readings.iter().map(|r| r.time).collect();
        unique_timestamps.sort();
        unique_timestamps.dedup();
        let source_stream = payload.stream_id;

        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "ingest_derived",
            None,
            None,
            &serde_json::json!({
                "site_id": sid,
                "stream_id": source_stream,
                "timestamps": unique_timestamps,
            }),
            None,
        )
        .await?;
    }

    let mut outcome = ingest_outcome(payload.stream_id, paired, submitted, inserted, &counts);
    if let Some(d) = &diff_outcome {
        // Under a diff, `inserted` from the upsert counts updates too; the classification is the
        // accurate account.
        outcome.inserted = d.new_rows;
        outcome.changed = d.changed;
        outcome.withdrawn = d.withdrawn;
        outcome.unchanged = d.unchanged;
        outcome.retained = d.retained;
        outcome.accepted_window = payload.window.clone();
        if d.braked {
            tracing::warn!(stream_id = %payload.stream_id, changed = d.changed, withdrawn = d.withdrawn, "Windowed pass braked; corrections and withdrawals held for review");
        }
    }
    tracing::debug!(total, inserted = outcome.inserted, skipped = outcome.skipped, changed = outcome.changed, withdrawn = outcome.withdrawn, stream_id = %payload.stream_id, paired, "Ingest complete");
    Ok(Json(outcome))
}

/// Add one rejection to the per-kind tally.
fn record_rejection(
    counts: &mut Vec<(admission::RejectionKind, usize)>,
    kind: admission::RejectionKind,
) {
    match counts.iter_mut().find(|(seen, _)| *seen == kind) {
        Some((_, n)) => *n += 1,
        None => counts.push((kind, 1)),
    }
}

/// The response, with the skipped tally logged on the way out. Every return path builds it here so
/// a dropped reading is reported to the caller and to the operator by the same code.
fn ingest_outcome(
    stream_id: Uuid,
    paired: bool,
    submitted: usize,
    inserted: usize,
    counts: &[(admission::RejectionKind, usize)],
) -> IngestResponse {
    let skipped: usize = counts.iter().map(|(_, n)| n).sum();
    let skipped_reasons: Vec<String> = counts
        .iter()
        .map(|(kind, n)| format!("{} ({n})", kind.as_str()))
        .collect();
    if skipped > 0 {
        tracing::warn!(
            %stream_id,
            skipped,
            submitted,
            reasons = ?skipped_reasons,
            "Skipped inadmissible readings"
        );
    }
    IngestResponse {
        inserted,
        skipped,
        skipped_reasons,
        held: 0,
        changed: 0,
        withdrawn: 0,
        unchanged: 0,
        retained: 0,
        accepted_window: None,
        stream_id,
        paired,
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestStatusEventsRequest {
    pub stream_id: Uuid,
    pub events: Vec<IngestStatusEvent>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IngestStatusEvent {
    pub time: chrono::DateTime<Utc>,
    pub value: String,
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IngestStatusEventsResponse {
    pub inserted: usize,
    /// Events dropped because their timestamp is outside the admissible window. Counted rather
    /// than raised, for the same reason `/ingest` counts its skipped readings.
    #[serde(default)]
    pub skipped: usize,
    pub stream_id: Uuid,
    pub paired: bool,
}

/// Stream-based status event ingestion (non-numeric device states like "low_battery").
/// Hypertable inserts keyed by stream_id. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/ingest/status_events",
    request_body = IngestStatusEventsRequest,
    responses(
        (status = 200, description = "Inserted count and pairing state. Events outside the admissible timestamp window are skipped and counted in `skipped`", body = IngestStatusEventsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_status_events(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(mut payload): Json<IngestStatusEventsRequest>,
) -> AppResult<Json<IngestStatusEventsResponse>> {
    if payload.events.is_empty() {
        return Ok(Json(IngestStatusEventsResponse {
            inserted: 0,
            skipped: 0,
            stream_id: payload.stream_id,
            paired: false,
        }));
    }

    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(payload.stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let (site_id, parameter_id) = resolve_stream_slot(db, stream.site_parameter_id).await?;
    let paired = site_id.is_some();

    enforce_ingest_scope(&state.db, &scope, site_id).await?;

    // A status event carries no numeric value, so the timestamp bound is the whole of admission
    // for it. Applied per event and counted, as on `/ingest`: an unbounded timestamp would open a
    // hypertable chunk far outside the data range, and refusing the request would stall the
    // stream that sent it.
    let submitted = payload.events.len();
    let now = Utc::now();
    payload
        .events
        .retain(|e| admission::time_rejection_at(now, e.time).is_none());
    let skipped = submitted - payload.events.len();
    if skipped > 0 {
        tracing::warn!(
            stream_id = %payload.stream_id,
            skipped,
            submitted,
            "Skipped status events outside the admissible timestamp window"
        );
    }
    if payload.events.is_empty() {
        return Ok(Json(IngestStatusEventsResponse {
            inserted: 0,
            skipped,
            stream_id: payload.stream_id,
            paired,
        }));
    }

    let models: Vec<status_events::ActiveModel> = payload
        .events
        .iter()
        .map(|e| status_events::ActiveModel {
            stream_id: Set(payload.stream_id),
            time: Set(e.time.into()),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            value: Set(e.value.clone()),
            sensor_id: Set(e.sensor_id),
        })
        .collect();

    let total = models.len();
    let mut inserted = 0usize;

    for chunk in models.chunks(BATCH_SIZE) {
        match status_events::Entity::insert_many(chunk.to_vec())
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    status_events::Column::StreamId,
                    status_events::Column::Time,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(db)
            .await
        {
            Ok(rows) => inserted += rows as usize,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("None of the records") {
                    // All duplicates
                } else {
                    tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert status event batch");
                    return Err(AppError::Database(e));
                }
            }
        }
    }

    tracing::debug!(total, inserted, skipped, stream_id = %payload.stream_id, paired, "Status events ingest complete");
    Ok(Json(IngestStatusEventsResponse {
        inserted,
        skipped,
        stream_id: payload.stream_id,
        paired,
    }))
}

/// The (site_id, parameter_id) a stream's pairing resolves to. Both are `None` when the stream is
/// unpaired, ie. its readings land unattributed and stay out of the rollups until it is paired.
async fn resolve_stream_slot(
    db: &sea_orm::DatabaseConnection,
    site_parameter_id: Option<Uuid>,
) -> AppResult<(Option<Uuid>, Option<Uuid>)> {
    let Some(sp_id) = site_parameter_id else {
        return Ok((None, None));
    };
    let Some(row) = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT site_id, parameter_id FROM site_parameters WHERE id = $1",
            [sp_id.into()],
        ))
        .await?
    else {
        return Ok((None, None));
    };
    let site_id: Uuid = row
        .try_get("", "site_id")
        .map_err(|e| AppError::Internal(format!("Failed to read site_id: {e}")))?;
    let parameter_id: Uuid = row
        .try_get("", "parameter_id")
        .map_err(|e| AppError::Internal(format!("Failed to read parameter_id: {e}")))?;
    Ok((Some(site_id), Some(parameter_id)))
}

/// Project-scope check for stream-based ingest. A scoped token may only write to a stream paired
/// to a site within its project; an unpaired stream (no resolved site) is rejected outright so a
/// scoped key cannot create unattributed, project-less data.
async fn enforce_ingest_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    site_id: Option<Uuid>,
) -> AppResult<()> {
    if !scope.is_restricted() {
        return Ok(());
    }
    match site_id {
        Some(sid) => enforce_project_scope_for_sites(db, scope, &[sid]).await?,
        None => {
            return Err(AppError::Forbidden(
                "Project-scoped token cannot ingest into an unpaired stream".to_string(),
            ));
        }
    }
    Ok(())
}
