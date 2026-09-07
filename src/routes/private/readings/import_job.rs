//! The `csv_import` job: the worker half of `readings/import.rs`. The request stages rows and
//! enqueues; this inserts them, recomputes derived parameters, refreshes aggregates over the
//! imported window and enqueues the alarm backfill.

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, Set, Statement};
use uuid::Uuid;

use super::batch::{ConflictMode, readings_on_conflict};
use super::import::BATCH_SIZE as CSV_BATCH_SIZE;
use super::{sample_groups, tail};
use crate::routes::private::readings;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::jobs::{
    as_db_err, optional_datetime, required_uuid, uuid_pair_array,
};
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use crate::routes::private::sensors::calibrations::service::{
    Curve, apply_curves, recalculate_derived_at_timestamp,
};

/// Take a spot group's replicates from `count` onwards out of what the group serves, ahead of an
/// overwrite that carries only `count` columns.
///
/// The displacement is a `withdrawn_at` stamp, not a delete: a flag, a hand-picked standard curve
/// and the value itself all survive it, and an import that later carries the column again clears
/// the stamp. A displaced row somebody had curated raises a `source_modified` hold, because
/// dropping it out of the served group is a ruling the file's column count should not make alone.
async fn displace_spot_tail(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    count: i16,
) -> crate::error::AppResult<()> {
    let time_value = sea_orm::prelude::DateTimeWithTimeZone::from(time);
    let curated = txn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT replicate_index, \
                    CASE WHEN is_flagged IS TRUE THEN 'flagged' ELSE 'standard_curve' END AS reason \
             FROM readings \
             WHERE stream_id = $1 AND time = $2 AND replicate_index >= $3 \
               AND measurement_type = 'spot' AND withdrawn_at IS NULL \
               AND (is_flagged IS TRUE OR standard_curve_id IS NOT NULL) \
             ORDER BY replicate_index",
            [
                stream_id.into(),
                time_value.into(),
                count.into(),
            ],
        ))
        .await?;
    if !curated.is_empty() {
        let entries = curated
            .iter()
            .map(|row| {
                Ok(serde_json::json!({
                    "replicate_index": row.try_get::<i16>("", "replicate_index")?,
                    "reason": row.try_get::<String>("", "reason")?,
                }))
            })
            .collect::<Result<Vec<_>, DbErr>>()?;
        crate::routes::private::readings::reconcile::upsert_source_modified_hold(
            txn,
            stream_id,
            time,
            serde_json::json!({ "claim": "displaced", "withdrawn": entries }),
            serde_json::json!({ "replicates": count }),
            "pending",
        )
        .await?;
    }

    // The displacement and its reversal are decisions of CSV origin (ADR 0008).
    use crate::routes::private::readings::decisions::{Kind, NewValue, Origin, record_many};
    record_many(
        txn,
        Kind::Withdraw,
        "r.stream_id = $1 AND r.time = $2 AND r.replicate_index >= $3 \
         AND r.measurement_type = 'spot' AND r.withdrawn_at IS NULL",
        vec![stream_id.into(), time_value.clone().into(), count.into()],
        NewValue::Literal(serde_json::json!({ "reason": "displaced_by_overwrite" })),
        "csv_import",
        Some("displaced_by_overwrite"),
        Origin::Csv,
        None,
    )
    .await
    .map_err(as_db_err)?;

    // A file that carries the column again re-asserts it, so its own displacement is reversed.
    record_many(
        txn,
        Kind::Reassert,
        "r.stream_id = $1 AND r.time = $2 AND r.replicate_index < $3 \
         AND r.withdrawn_reason = 'displaced_by_overwrite'",
        vec![stream_id.into(), time_value.into(), count.into()],
        NewValue::Literal(serde_json::json!({})),
        "csv_import",
        Some("re-asserted by the file"),
        Origin::Csv,
        None,
    )
    .await
    .map_err(as_db_err)?;
    Ok(())
}

/// Insert a CSV import's staged readings, recompute derived parameters and refresh aggregates over
/// the imported window, then enqueue an `alarm_backfill` for the touched slots. Reads its inputs
/// (and the staged rows, by `import_token`) from params, so any replica can run it. Non-rerunnable:
/// the staging rows are deleted on completion. Readings constants and the request-level
/// measurement_type are re-applied here, and replicate groups are numbered and given a sample.
pub struct CsvImport;

#[async_trait]
impl Job for CsvImport {
    fn name(&self) -> &'static str {
        "csv_import"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let import_token = required_uuid(ctx.params(), "import_token")?;
        let outcome = Self::run_import(&ctx, import_token).await;
        if outcome.is_err() {
            // Success deletes the staging rows below; a mid-run error would otherwise orphan them
            // (there is no janitor for csv_import_staging), so drop them on the failure path too.
            let _ = ctx
                .db()
                .execute_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM csv_import_staging WHERE import_token = $1",
                    [import_token.into()],
                ))
                .await;
        }
        outcome
    }
}

impl CsvImport {
    async fn run_import(ctx: &JobContext, import_token: Uuid) -> Result<i64, DbErr> {
        let params = ctx.params();
        let site_id = required_uuid(params, "site_id")?;
        let site_name = params
            .get("site_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let conflict = match params.get("conflict").and_then(serde_json::Value::as_str) {
            Some("overwrite") => ConflictMode::Overwrite,
            _ => ConflictMode::Skip,
        };
        let since = optional_datetime(params, "since");
        let latest = optional_datetime(params, "latest");
        let overlapping = params
            .get("overlapping")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let param_streams = uuid_pair_array(params, "param_streams");
        // Explicit request-level classification, or None to resolve per row from the
        // stream declaration and the owning sensor's data_frequency.
        let request_measurement_type = params
            .get("measurement_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        ctx.set_site(site_id).await;

        // Read the staged rows back and rebuild the readings, re-applying the constant fields.
        let mut staged = ctx
            .db()
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT stream_id, site_id, parameter_id, time, raw_value, \
                        sensor_id, calibration_id, deployment_id \
                 FROM csv_import_staging WHERE import_token = $1 ORDER BY seq",
                [import_token.into()],
            ))
            .await?;

        // The import handler refuses rows targeting a replicate-family stream before staging, and
        // the same rule holds here so no staging row, however it got there, mints replicate
        // indexes onto a family (a reading's index is the source's column position).
        {
            let mut staged_streams: Vec<Uuid> = staged
                .iter()
                .filter_map(|row| row.try_get::<Uuid>("", "stream_id").ok())
                .collect();
            staged_streams.sort_unstable();
            staged_streams.dedup();
            let family_ids: std::collections::HashSet<Uuid> = ctx
                .db()
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT id FROM data_streams \
                     WHERE id = ANY($1) AND metadata -> 'replicates' IS NOT NULL",
                    [staged_streams.into()],
                ))
                .await?
                .iter()
                .filter_map(|row| row.try_get::<Uuid>("", "id").ok())
                .collect();
            if !family_ids.is_empty() {
                let before = staged.len();
                staged.retain(|row| {
                    row.try_get::<Uuid>("", "stream_id")
                        .is_ok_and(|id| !family_ids.contains(&id))
                });
                ctx.log(
                    "warn",
                    "Staged rows targeting replicate-family streams were dropped; family \
                     replicates sync from the source and cannot be written by CSV import",
                    serde_json::json!({ "rows_dropped": before - staged.len() }),
                )
                .await;
            }
        }

        // Staging carries the calibration each row resolved at upload time. The coefficients are
        // read back here so the stored value is the one that calibration produces: a row that names
        // a curve and carries the uncorrected number claims a correction it never had.
        let staged_curves = {
            let mut ids: Vec<Uuid> = staged
                .iter()
                .filter_map(|row| {
                    row.try_get::<Option<Uuid>>("", "calibration_id")
                        .ok()
                        .flatten()
                })
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let mut curves: std::collections::HashMap<Uuid, Curve> =
                std::collections::HashMap::new();
            if !ids.is_empty() {
                for row in ctx
                    .db()
                    .query_all_raw(Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "SELECT id, slope, intercept FROM sensor_calibrations WHERE id = ANY($1)",
                        [ids.into()],
                    ))
                    .await?
                {
                    let id: Uuid = row.try_get("", "id")?;
                    curves.insert(
                        id,
                        Curve {
                            id,
                            slope: row.try_get("", "slope")?,
                            intercept: row.try_get("", "intercept")?,
                        },
                    );
                }
            }
            curves
        };

        let (stream_defaults, sensor_types) = if request_measurement_type.is_none() {
            let mut stream_ids: Vec<Uuid> = Vec::new();
            let mut sensor_ids: Vec<Uuid> = Vec::new();
            for row in &staged {
                stream_ids.push(row.try_get("", "stream_id")?);
                if let Some(sid) = row.try_get::<Option<Uuid>>("", "sensor_id")? {
                    sensor_ids.push(sid);
                }
            }
            stream_ids.sort_unstable();
            stream_ids.dedup();
            sensor_ids.sort_unstable();
            sensor_ids.dedup();

            let mut defaults: std::collections::HashMap<Uuid, Option<String>> =
                std::collections::HashMap::new();
            for row in ctx
                .db()
                .query_all_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT id, measurement_type FROM data_streams WHERE id = ANY($1)",
                    [stream_ids.into()],
                ))
                .await?
            {
                let id: Uuid = row.try_get("", "id")?;
                defaults.insert(id, row.try_get("", "measurement_type")?);
            }
            let types =
                crate::routes::private::readings::measurement::measurement_types_for_sensors(
                    ctx.db(),
                    &sensor_ids,
                )
                .await?;
            (defaults, types)
        } else {
            (
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
            )
        };

        let mut models: Vec<readings::ActiveModel> = Vec::with_capacity(staged.len());
        let mut distinct_ts: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
        for row in &staged {
            let stream_id: Uuid = row.try_get("", "stream_id")?;
            let row_site_id: Option<Uuid> = row.try_get("", "site_id")?;
            let parameter_id: Option<Uuid> = row.try_get("", "parameter_id")?;
            let time: chrono::DateTime<chrono::FixedOffset> = row.try_get("", "time")?;
            let raw_value: f64 = row.try_get("", "raw_value")?;
            let sensor_id: Option<Uuid> = row.try_get("", "sensor_id")?;
            let calibration_id: Option<Uuid> = row.try_get("", "calibration_id")?;
            let deployment_id: Option<Uuid> = row.try_get("", "deployment_id")?;
            distinct_ts.push(time.with_timezone(&chrono::Utc));
            let measurement_type =
                crate::routes::private::readings::measurement::resolve_measurement_type(
                    request_measurement_type.as_deref(),
                    stream_defaults.get(&stream_id).and_then(|d| d.as_deref()),
                    sensor_id,
                    &sensor_types,
                );
            // A value is only ever what a curve produced: with none resolved the column stays
            // NULL, the same as an entry through `/grab_samples` or `/ingest`, so an uncorrected
            // measurement is never mistaken for a corrected one. Consumers read
            // COALESCE(calibrated_value, raw_value), so the raw value is still what is served.
            let base = calibration_id
                .and_then(|id| staged_curves.get(&id))
                .copied();
            let calibrated_value = base.map(|c| apply_curves(raw_value, Some(c), None));
            models.push(readings::ActiveModel {
                provenance_kind: Set(Some("csv_import".to_string())),
                site_id: Set(row_site_id),
                parameter_id: Set(parameter_id),
                calibrated_value: Set(calibrated_value),
                sensor_id: Set(sensor_id),
                calibration_id: Set(calibration_id),
                deployment_id: Set(deployment_id),
                logged: Set(Some(false)),
                measurement_type: Set(Some(measurement_type)),
                ..readings::new(stream_id, time, 0, raw_value)
            });
        }
        distinct_ts.sort_unstable();
        distinct_ts.dedup();

        // Rows sharing (stream_id, time) are numbered replicate_index 0..n-1 in staging seq order.
        let mut group_counts: std::collections::HashMap<
            (Uuid, chrono::DateTime<chrono::Utc>),
            i16,
        > = std::collections::HashMap::with_capacity(models.len());
        for m in &mut models {
            let key = (
                *m.stream_id.as_ref(),
                m.time.as_ref().with_timezone(&chrono::Utc),
            );
            let counter = group_counts.entry(key).or_insert(0);
            m.replicate_index = Set(*counter);
            *counter += 1;
        }

        // Whether a group is a sample is not decided here: `sample_groups::forms_sample` is the
        // one answer, two or more rows classified spot sharing a slot instant.
        let mut spot_groups: std::collections::HashMap<
            (Uuid, Uuid, chrono::DateTime<chrono::Utc>),
            usize,
        > = std::collections::HashMap::new();
        for m in &models {
            if m.measurement_type.as_ref().as_deref() != Some(sample_groups::SPOT) {
                continue;
            }
            if let (Some(sid), Some(pid)) = (*m.site_id.as_ref(), *m.parameter_id.as_ref()) {
                *spot_groups
                    .entry((sid, pid, m.time.as_ref().with_timezone(&chrono::Utc)))
                    .or_default() += 1;
            }
        }
        let replicate_groups = spot_groups
            .values()
            .filter(|count| sample_groups::forms_sample(**count))
            .count();

        let total = i32::try_from(models.len()).unwrap_or(i32::MAX);
        ctx.set_progress(0, Some(total)).await;

        // An overwrite replaces the whole replicate set, not the Nth row by the Nth: stored spot
        // replicates beyond the incoming count would survive a positional upsert and keep
        // double-counting the group, so the tail leaves the group before the insert.
        let mut spot_group_sizes: std::collections::HashMap<
            (Uuid, chrono::DateTime<chrono::Utc>),
            i16,
        > = std::collections::HashMap::new();
        if conflict == ConflictMode::Overwrite {
            for m in &models {
                if m.measurement_type.as_ref().as_deref() == Some(sample_groups::SPOT) {
                    *spot_group_sizes
                        .entry((
                            *m.stream_id.as_ref(),
                            m.time.as_ref().with_timezone(&chrono::Utc),
                        ))
                        .or_default() += 1;
                }
            }
        }

        // Phase 1: displace the tail and insert, in one transaction. A crash between the two
        // would otherwise leave the group short of both its old rows and its new ones.
        let mut inserted_so_far = 0usize;
        let (affected_total, corrected) = crate::common::bulk_write::guarded(ctx.db(), async |txn| {
            for ((stream_id, time), count) in &spot_group_sizes {
                displace_spot_tail(txn, *stream_id, *time, *count).await?;
            }

            let mut affected_total = 0usize;
            let mut corrected = 0usize;
            for chunk in models.chunks(CSV_BATCH_SIZE) {
                // A correction corrects the measurement, so it is a decision: recorded before the
                // write, against the values still stored, and only for the rows the write moves
                // (ADR 0008). The count it returns is what the run reports as overwritten.
                if conflict == ConflictMode::Overwrite {
                    let corrections =
                        crate::routes::private::readings::decisions::record_value_corrections(
                            txn,
                            chunk,
                            "csv_import",
                            crate::routes::private::readings::decisions::Origin::Csv,
                        )
                        .await?;
                    corrected += usize::try_from(corrections.rows).unwrap_or(usize::MAX);
                }
                match readings::Entity::insert_many(chunk.to_vec())
                    .on_conflict(readings_on_conflict(conflict))
                    .exec_without_returning(txn)
                    .await
                {
                    Ok(affected) => affected_total += affected as usize,
                    Err(e) => {
                        let msg = e.to_string();
                        if !msg.contains("None of the records") {
                            tracing::warn!(error = %e, "Failed to insert imported readings chunk");
                            return Err(e.into());
                        }
                    }
                }
                inserted_so_far += chunk.len();
                if inserted_so_far % 5000 < CSV_BATCH_SIZE {
                    ctx.set_progress(
                        i32::try_from(inserted_so_far).unwrap_or(i32::MAX),
                        Some(total),
                    )
                    .await;
                }
            }
            Ok((affected_total, corrected))
        })
        .await
        .map_err(as_db_err)?;

        // An overwrite replaces the measurement, not the correction: an import never decides which
        // curve applies, so the corrected raw value goes back through the curves already on the row.
        if conflict == ConflictMode::Overwrite
            && overlapping > 0
            && let (Some(first), Some(last)) = (distinct_ts.first(), distinct_ts.last())
        {
            let mut stream_ids: Vec<Uuid> = models.iter().map(|m| *m.stream_id.as_ref()).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let recomposed =
                crate::routes::private::sensors::calibrations::service::recompose_from_own_curves_guarded(
                    ctx.db(),
                    "TRUE",
                    "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3",
                    vec![
                        stream_ids.into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(*first).into(),
                        sea_orm::prelude::DateTimeWithTimeZone::from(*last).into(),
                    ],
                )
                .await
                .map_err(as_db_err)?;
            tracing::info!(
                site = %site_name,
                recomposed,
                "CSV overwrite recomposed corrected values from the curves already on the rows"
            );
        }

        let mut touched_visits: Vec<
            crate::routes::private::collection_events::recompute::TouchedEvent,
        > = Vec::new();
        // Samples are found-or-created and stamped after the insert, by the one materialiser, over
        // the streams and the time span this import touched. Scoping it that way rather than to the
        // rows this run inserted is deliberate: a reading already present at a group's slot is part
        // of the same collection event, whose identity is (site, parameter, instant).
        if !spot_groups.is_empty()
            && let (Some(first), Some(last)) = (distinct_ts.first(), distinct_ts.last())
        {
            let mut stream_ids: Vec<Uuid> = models.iter().map(|m| *m.stream_id.as_ref()).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let row_predicate = format!(
                "r.stream_id = ANY($1) AND r.time >= '{}'::timestamptz AND r.time <= '{}'::timestamptz",
                first.to_rfc3339(),
                last.to_rfc3339()
            );
            let stream_ids_for_events = stream_ids.clone();
            sample_groups::materialise_samples(
                ctx.db(),
                &row_predicate,
                vec![stream_ids.clone().into()],
            )
            .await
            .map_err(as_db_err)?;

            // A CSV import is a person entering visits after the fact: manual collection events.
            crate::routes::private::collection_events::attach::attach_collection_events(
                ctx.db(),
                &row_predicate,
                vec![stream_ids.into()],
                crate::routes::private::collection_events::attach::EventSource::Manual,
            )
            .await
            .map_err(as_db_err)?;

            // The values have landed at their visits; the calculations that read them run without
            // anyone asking (ADR 0007). Read after the attach, which is what gives the rows the
            // events this looks them up by.
            touched_visits = crate::routes::private::collection_events::recompute::touched_events(
                ctx.db(),
                &row_predicate,
                vec![stream_ids_for_events.into()],
            )
            .await
            .map_err(as_db_err)?;
        }

        let (inserted_total, overwritten) = match conflict {
            ConflictMode::Skip => (affected_total, 0),
            // The rows a correction was recorded for are the rows the write moved, so the run
            // reports the same number the decision record holds rather than the staged estimate.
            ConflictMode::Overwrite => (affected_total.saturating_sub(overlapping), corrected),
        };
        tracing::info!(site = %site_name, inserted_total, overwritten, "CSV import inserted readings");

        if inserted_total > 0 || overwritten > 0 {
            // Phase 2: derived recompute over the imported timestamps.
            let derived_total = i32::try_from(models.len() + distinct_ts.len()).unwrap_or(i32::MAX);
            ctx.set_progress(
                i32::try_from(models.len()).unwrap_or(i32::MAX),
                Some(derived_total),
            )
            .await;
            for (i, time) in distinct_ts.iter().enumerate() {
                if ctx.is_cancelled() {
                    break;
                }
                let _ = recalculate_derived_at_timestamp(ctx.db(), site_id, *time).await;
                if (i + 1) % 500 == 0 {
                    let prog = i32::try_from(models.len() + i + 1).unwrap_or(i32::MAX);
                    ctx.set_progress(prog, Some(derived_total)).await;
                }
            }

            // An import is a person entering visits after the fact, so the rollups are refreshed
            // from the earliest instant it landed, and a failure there fails the job: a swallowed
            // refresh reports an import as complete while the rollups still serve the old numbers.
            // The window can be long, so episodes are rebuilt by the `alarm_backfill` job.
            let app = crate::common::global_app_state();
            let written =
                tail::Written::new(u64::try_from(inserted_total + overwritten).unwrap_or(u64::MAX))
                    .over(since.zip(latest))
                    .at(param_streams
                        .iter()
                        .map(|(parameter_id, stream_id)| {
                            tail::Slot::paired(site_id, *parameter_id).through(*stream_id)
                        })
                        .collect())
                    .touching(touched_visits);
            tail::run(
                tail::Sink {
                    db: ctx.db(),
                    events: ctx.events(),
                    cache: app.as_ref().map(|a| &a.response_cache),
                },
                &written,
                &tail::Axes {
                    cache: tail::Cache::Sites,
                    refresh: tail::Refresh::Since { fatal: true },
                    announce: true,
                    reconcile_alarms: false,
                    episodes: tail::Episodes::Job,
                    recompute_derived: false,
                    writer: crate::routes::private::collection_events::recompute::Writer::Person,
                },
                "csv_import",
            )
            .await
            .map_err(as_db_err)?;
        }

        // The staging source has served its purpose, drop it (makes this job non-rerunnable).
        ctx.db()
            .execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "DELETE FROM csv_import_staging WHERE import_token = $1",
                [import_token.into()],
            ))
            .await?;

        ctx.report(
            JobReport::new()
                .scope("site_id", site_id.to_string())
                .count("inserted", inserted_total)
                .count("overwritten", overwritten)
                .count("replicate_groups", replicate_groups),
        )
        .await;
        Ok(i64::from(
            i32::try_from(inserted_total + overwritten).unwrap_or(i32::MAX),
        ))
    }
}
