use async_trait::async_trait;
use sea_orm::ColumnTrait;
use sea_orm::Condition;
use sea_orm::DbErr;
use sea_orm::EntityTrait;
use sea_orm::ExprTrait;
use sea_orm::FromQueryResult;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::Set;
use sea_orm::entity::prelude::*;
use sea_orm::sea_query;
use sea_orm::sea_query::Expr;
use uuid::Uuid;

use crate::routes::private::data_streams;
use crate::routes::private::readings;
use crate::routes::private::readings::models::ConflictMode;
use crate::routes::private::readings::models::import_staging;
use crate::routes::private::readings::service::BATCH_SIZE as CSV_BATCH_SIZE;
use crate::routes::private::readings::service::readings_on_conflict;
use crate::routes::private::reprocessing_jobs::flows::{
    as_db_err, optional_datetime, required_uuid, uuid_array, uuid_pair_array,
};
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::service::Curve;
use crate::routes::private::sensor_calibrations::service::apply_curves;
use crate::routes::private::sensor_calibrations::service::recalculate_derived_at_timestamp;
use crate::routes::private::sync::models::HoldStatus;

/// Whether a run's exit takes the import's staged rows with it: a success has used them, and a
/// failure no retry follows leaves nobody to read them. A failure the worker retries keeps them.
pub(super) fn should_drop_staged(succeeded: bool, final_attempt: bool) -> bool {
    succeeded || final_attempt
}

/// Take an import's staged rows out of the way, once it finished or failed for the last time. A run
/// that never returns leaves them to the janitor's [`prune_orphaned_staging`].
pub(super) async fn drop_staged<C: sea_orm::ConnectionTrait>(
    db: &C,
    import_token: Uuid,
) -> Result<(), DbErr> {
    import_staging::Entity::delete_many()
        .filter(import_staging::Column::ImportToken.eq(import_token))
        .exec(db)
        .await?;
    Ok(())
}

/// The staged tokens no import will read: every one but those a job still in flight names.
pub(super) fn orphaned_tokens(staged: &[Uuid], live: &[Uuid]) -> Vec<Uuid> {
    staged
        .iter()
        .filter(|token| !live.contains(token))
        .copied()
        .collect()
}

/// Remove the staged rows of imports whose job finished without taking them: one whose worker died
/// on every attempt, or whose row was pruned. Returns the rows removed.
pub async fn prune_orphaned_staging<C: sea_orm::ConnectionTrait>(db: &C) -> Result<u64, DbErr> {
    use crate::routes::private::reprocessing_jobs::models::job as jobs;
    use crate::routes::private::reprocessing_jobs::service::IN_FLIGHT_STATES;
    let staged: Vec<Uuid> = import_staging::Entity::find()
        .select_only()
        .column(import_staging::Column::ImportToken)
        .distinct()
        .into_tuple()
        .all(db)
        .await?;
    if staged.is_empty() {
        return Ok(0);
    }
    let live: Vec<Uuid> = jobs::Entity::find()
        .filter(jobs::Column::TriggerType.eq("csv_import"))
        .filter(jobs::Column::Status.is_in(IN_FLIGHT_STATES))
        .all(db)
        .await?
        .iter()
        .filter_map(|job| {
            job.params
                .get("import_token")?
                .as_str()?
                .parse::<Uuid>()
                .ok()
        })
        .collect();
    let orphaned = orphaned_tokens(&staged, &live);
    if orphaned.is_empty() {
        return Ok(0);
    }
    Ok(import_staging::Entity::delete_many()
        .filter(import_staging::Column::ImportToken.is_in(orphaned))
        .exec(db)
        .await?
        .rows_affected)
}

/// One staged row, as `csv_import_staging` holds it. The job reads the set four times, so the
/// columns are named once here rather than in each pass.
#[derive(Debug, Clone, Copy)]
pub(super) struct StagedRow {
    pub(super) stream_id: Uuid,
    pub(super) site_id: Option<Uuid>,
    pub(super) parameter_id: Option<Uuid>,
    pub(super) time: chrono::DateTime<chrono::FixedOffset>,
    pub(super) raw_value: f64,
    pub(super) sensor_id: Option<Uuid>,
    pub(super) calibration_id: Option<Uuid>,
    pub(super) deployment_id: Option<Uuid>,
}

impl From<import_staging::Model> for StagedRow {
    fn from(row: import_staging::Model) -> Self {
        Self {
            stream_id: row.stream_id,
            site_id: row.site_id,
            parameter_id: row.parameter_id,
            time: row.time,
            raw_value: row.raw_value,
            sensor_id: row.sensor_id,
            calibration_id: row.calibration_id,
            deployment_id: row.deployment_id,
        }
    }
}

/// A staged row as its stream's pairing stands in the transaction storing it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Attributed {
    pub(super) row: StagedRow,
    /// The slot the stream was paired to after the row was staged, whose reprocess resolves the
    /// row's deployment and curve.
    pub(super) slot_since_staging: Option<(Uuid, Uuid)>,
}

/// Attribute a staged row from the pairing its stream holds as the write transaction locked it,
/// not the one it was staged under. An unpaired stream stores the row with no site, parameter,
/// instrument, deployment or curve. A stream paired since staging stamps its slot and instrument,
/// the way the pairing backfill would have, and leaves the deployment and curve to its reprocess.
pub(super) fn attribute_staged(
    row: StagedRow,
    stream: readings::service::TargetStream,
) -> Attributed {
    let Some((site_id, parameter_id)) = stream.slot else {
        return Attributed {
            row: StagedRow {
                site_id: None,
                parameter_id: None,
                sensor_id: None,
                calibration_id: None,
                deployment_id: None,
                ..row
            },
            slot_since_staging: None,
        };
    };
    if row.site_id == Some(site_id) && row.parameter_id == Some(parameter_id) {
        return Attributed {
            row,
            slot_since_staging: None,
        };
    }
    Attributed {
        row: StagedRow {
            site_id: Some(site_id),
            parameter_id: Some(parameter_id),
            sensor_id: stream.instrument.or(row.sensor_id),
            calibration_id: None,
            deployment_id: None,
            ..row
        },
        slot_since_staging: Some((site_id, parameter_id)),
    }
}

/// The readings an import stores from its attributed rows, with the request-level
/// measurement_type re-applied (or each row's resolved from its stream and instrument), and rows
/// sharing (stream_id, time) numbered replicate_index 0..n-1 in staging seq order.
async fn import_models<C: sea_orm::ConnectionTrait>(
    conn: &C,
    rows: &[StagedRow],
    request_measurement_type: Option<&str>,
) -> Result<Vec<readings::ActiveModel>, DbErr> {
    let curves = import_curves(conn, rows).await?;
    let (stream_defaults, sensor_types) = if request_measurement_type.is_none() {
        let mut stream_ids: Vec<Uuid> = rows.iter().map(|row| row.stream_id).collect();
        stream_ids.sort_unstable();
        stream_ids.dedup();
        let mut sensor_ids: Vec<Uuid> = rows.iter().filter_map(|row| row.sensor_id).collect();
        sensor_ids.sort_unstable();
        sensor_ids.dedup();
        let defaults: std::collections::HashMap<Uuid, Option<String>> =
            data_streams::Entity::find()
                .filter(data_streams::Column::Id.is_in(stream_ids))
                .all(conn)
                .await?
                .into_iter()
                .map(|stream| (stream.id, stream.measurement_type))
                .collect();
        let types = readings::service::measurement_types_for_sensors(conn, &sensor_ids).await?;
        (defaults, types)
    } else {
        Default::default()
    };

    let mut group_counts: std::collections::HashMap<(Uuid, chrono::DateTime<chrono::Utc>), i16> =
        std::collections::HashMap::with_capacity(rows.len());
    Ok(rows
        .iter()
        .map(|row| {
            let measurement_type = readings::service::resolve_measurement_type(
                request_measurement_type,
                stream_defaults
                    .get(&row.stream_id)
                    .and_then(|d| d.as_deref()),
                row.sensor_id,
                &sensor_types,
            );
            // A value is only ever what a curve produced: with none resolved the column stays
            // NULL, the same as an entry through `/grab_samples` or `/ingest`, so an uncorrected
            // measurement is never mistaken for a corrected one. Consumers read
            // COALESCE(calibrated_value, raw_value), so the raw value is still what is served.
            let base = row.calibration_id.and_then(|id| curves.get(&id)).copied();
            let counter = group_counts
                .entry((row.stream_id, row.time.with_timezone(&chrono::Utc)))
                .or_insert(0);
            let replicate_index = *counter;
            *counter += 1;
            readings::ActiveModel {
                provenance_kind: Set(Some("csv_import".to_string())),
                site_id: Set(row.site_id),
                parameter_id: Set(row.parameter_id),
                calibrated_value: Set(base.map(|c| apply_curves(row.raw_value, Some(c), None))),
                sensor_id: Set(row.sensor_id),
                calibration_id: Set(row.calibration_id),
                deployment_id: Set(row.deployment_id),
                logged: Set(Some(false)),
                measurement_type: Set(Some(measurement_type)),
                ..readings::new(row.stream_id, row.time, replicate_index, row.raw_value)
            }
        })
        .collect())
}

/// The coefficients of each calibration the rows name, read back so the stored value is the one
/// that calibration produces: a row that names a curve and carries the uncorrected number claims a
/// correction it never had.
async fn import_curves<C: sea_orm::ConnectionTrait>(
    conn: &C,
    rows: &[StagedRow],
) -> Result<std::collections::HashMap<Uuid, Curve>, DbErr> {
    let mut ids: Vec<Uuid> = rows.iter().filter_map(|row| row.calibration_id).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(sensor_calibrations::models::Entity::find()
        .filter(sensor_calibrations::models::Column::Id.is_in(ids))
        .all(conn)
        .await?
        .into_iter()
        .map(|curve| {
            let c = Curve {
                id: curve.id,
                slope: curve.slope,
                intercept: curve.intercept,
            };
            (curve.id, c)
        })
        .collect())
}

/// How many spot rows each (stream, instant) group carries.
fn spot_group_sizes(
    models: &[readings::ActiveModel],
) -> std::collections::HashMap<(Uuid, chrono::DateTime<chrono::Utc>), i16> {
    let mut sizes = std::collections::HashMap::new();
    for m in models {
        if m.measurement_type.as_ref().as_deref() == Some(readings::service::SPOT) {
            *sizes
                .entry((
                    *m.stream_id.as_ref(),
                    m.time.as_ref().with_timezone(&chrono::Utc),
                ))
                .or_default() += 1;
        }
    }
    sizes
}

/// A curated replicate an overwrite is about to displace, and what makes it curated.
#[derive(FromQueryResult)]
pub(super) struct CuratedRow {
    pub(super) replicate_index: i16,
    pub(super) reason: String,
}

/// Take a spot group's replicates from `count` onwards out of what the group serves, ahead of an
/// overwrite that carries only `count` columns.
///
/// The displacement is a `withdrawn_at` stamp, not a delete: a flag, a hand-picked standard curve
/// and the value itself all survive it, and an import that later carries the column again clears
/// the stamp. A displaced row somebody had curated raises a `source_modified` hold, because
/// dropping it out of the served group is a ruling the file's column count should not make alone.
pub(super) async fn displace_spot_tail(
    txn: &sea_orm::DatabaseTransaction,
    stream_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    count: i16,
) -> crate::error::AppResult<()> {
    let time_value = sea_orm::prelude::DateTimeWithTimeZone::from(time);
    // Withdrawn rows are already excluded, so a survivor is kept for a flag or a hand curve.
    let reason: Expr = sea_orm::sea_query::CaseStatement::new()
        .case(Expr::cust("is_flagged IS TRUE"), "flagged")
        .finally("standard_curve")
        .into();
    let curated = readings::Entity::find()
        .select_only()
        .column(readings::Column::ReplicateIndex)
        .column_as(reason, "reason")
        .filter(readings::Column::StreamId.eq(stream_id))
        .filter(readings::Column::Time.eq(time_value))
        .filter(readings::Column::ReplicateIndex.gte(count))
        .filter(readings::Column::MeasurementType.eq("spot"))
        .filter(readings::Column::WithdrawnAt.is_null())
        .filter(
            Condition::any()
                .add(Expr::cust("is_flagged IS TRUE"))
                .add(readings::Column::StandardCurveId.is_not_null()),
        )
        .order_by_asc(readings::Column::ReplicateIndex)
        .into_model::<CuratedRow>()
        .all(txn)
        .await?;
    if !curated.is_empty() {
        let entries = curated
            .iter()
            .map(|row| {
                serde_json::json!({
                    "replicate_index": row.replicate_index,
                    "reason": row.reason,
                })
            })
            .collect::<Vec<_>>();
        crate::routes::private::readings::service::upsert_source_modified_hold(
            txn,
            stream_id,
            time,
            serde_json::json!({ "claim": "displaced", "withdrawn": entries }),
            serde_json::json!({ "replicates": count }),
            HoldStatus::Pending,
        )
        .await?;
    }

    // The displacement and its reversal are decisions of CSV origin (ADR 0008).
    use super::models::Kind;
    use super::models::Origin;
    use super::service::NewValue;
    use super::service::r;
    use super::service::record_many;
    record_many(
        txn,
        Kind::Withdraw,
        Condition::all()
            .add(r(readings::Column::StreamId).eq(stream_id))
            .add(r(readings::Column::Time).eq(time_value))
            .add(r(readings::Column::ReplicateIndex).gte(count))
            .add(r(readings::Column::MeasurementType).eq("spot"))
            .add(r(readings::Column::WithdrawnAt).is_null()),
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
        Condition::all()
            .add(r(readings::Column::StreamId).eq(stream_id))
            .add(r(readings::Column::Time).eq(time_value))
            .add(r(readings::Column::ReplicateIndex).lt(count))
            .add(r(readings::Column::WithdrawnReason).eq("displaced_by_overwrite")),
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
        // Success drops the staged rows itself; a failure keeps them for the retry that reads them.
        if outcome.is_err() && should_drop_staged(false, ctx.is_final_attempt()) {
            let _ = drop_staged(ctx.db(), import_token).await;
        }
        outcome
    }
}

/// An import is a person entering visits after the fact, so the rollups are refreshed from the
/// earliest instant it landed, and a failure there fails the job: a swallowed refresh reports an
/// import as complete while the rollups still serve the old numbers. The window can be long, so
/// episodes are rebuilt by the `alarm_backfill` job.
const IMPORT_TAIL: crate::routes::private::readings::service::Axes =
    crate::routes::private::readings::service::Axes {
        cache: crate::routes::private::readings::service::Cache::Sites,
        refresh: crate::routes::private::readings::service::Refresh::Since { fatal: true },
        announce: true,
        reconcile_alarms: false,
        episodes: crate::routes::private::readings::service::Episodes::Job,
        recompute_derived: false,
        writer: crate::routes::private::collection_events::flows::Writer::Person,
    };

/// The rows an import inserted and the rows it corrected. The rows a correction was recorded for
/// are the rows the write moved, so the run reports the same number the decision record holds
/// rather than the staged estimate.
fn import_counts(
    conflict: ConflictMode,
    affected: usize,
    overlapping: usize,
    corrected: usize,
) -> (usize, usize) {
    match conflict {
        ConflictMode::Skip => (affected, 0),
        ConflictMode::Overwrite => (affected.saturating_sub(overlapping), corrected),
    }
}

/// What an import landed at one site, in the shape the shared tail reads.
fn import_written(
    site_id: Uuid,
    span: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    param_streams: &[(Uuid, Uuid)],
    moved: usize,
) -> crate::routes::private::readings::service::Written {
    crate::routes::private::readings::service::Written::new(
        u64::try_from(moved).unwrap_or(u64::MAX),
    )
    .over(span)
    .at(param_streams
        .iter()
        .map(|(parameter_id, stream_id)| {
            crate::routes::private::readings::service::Slot::paired(site_id, *parameter_id)
                .through(*stream_id)
        })
        .collect())
}

impl CsvImport {
    pub(super) async fn run_import(ctx: &JobContext, import_token: Uuid) -> Result<i64, DbErr> {
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
        // Decoded once here: the rows are read four times below, and a column named in one place
        // and not another is exactly the drift the derive removes.
        let mut staged: Vec<StagedRow> = import_staging::Entity::find()
            .filter(import_staging::Column::ImportToken.eq(import_token))
            .order_by_asc(import_staging::Column::Seq)
            .all(ctx.db())
            .await?
            .into_iter()
            .map(StagedRow::from)
            .collect();
        // The handler stages at least one row with the enqueue, so a token naming none is a run
        // whose rows were taken already, and reporting it completed would say nothing was lost.
        if staged.is_empty() {
            return Err(DbErr::Custom(format!(
                "csv_import {import_token} has no staged rows to import"
            )));
        }

        // The import handler refuses rows targeting a replicate-family stream before staging, and
        // the same rule holds here so no staging row, however it got there, mints replicate
        // indexes onto a family (a reading's index is the source's column position).
        {
            let mut staged_streams: Vec<Uuid> = staged.iter().map(|row| row.stream_id).collect();
            staged_streams.sort_unstable();
            staged_streams.dedup();
            let family_ids: std::collections::HashSet<Uuid> =
                super::service::replicate_family_keys(ctx.db(), &staged_streams)
                    .await
                    .map_err(|e| DbErr::Custom(e.to_string()))?
                    .into_keys()
                    .collect();
            if !family_ids.is_empty() {
                let before = staged.len();
                staged.retain(|row| !family_ids.contains(&row.stream_id));
                ctx.log(
                    "warn",
                    "Staged rows targeting replicate-family streams were dropped; family \
                     replicates sync from the source and cannot be written by CSV import",
                    serde_json::json!({ "rows_dropped": before - staged.len() }),
                )
                .await;
            }
        }

        let total = i32::try_from(staged.len()).unwrap_or(i32::MAX);
        ctx.set_progress(0, Some(total)).await;
        let mut distinct_ts: Vec<chrono::DateTime<chrono::Utc>> = staged
            .iter()
            .map(|row| row.time.with_timezone(&chrono::Utc))
            .collect();
        distinct_ts.sort_unstable();
        distinct_ts.dedup();

        // Phase 1: attribute, displace the tail and insert, in one transaction. The rows take the
        // pairing their streams hold with those streams locked, so a pairing or unpair committed
        // since staging is what they land under, and one arriving now waits for them. A crash
        // between the displacement and the insert would otherwise leave the group short of both
        // its old rows and its new ones.
        let mut inserted_so_far = 0usize;
        let (affected_total, corrected, models) = crate::common::bulk_write::guarded(ctx.db(), async |txn| {
            let streams = readings::service::lock_target_streams(
                txn,
                staged.iter().map(|row| row.stream_id),
            )
            .await?;
            let mut since_staging: std::collections::HashMap<Uuid, ((Uuid, Uuid), u64)> =
                std::collections::HashMap::new();
            let attributed: Vec<StagedRow> = staged
                .iter()
                .map(|row| {
                    let stream = streams.get(&row.stream_id).copied().unwrap_or_default();
                    let attributed = attribute_staged(*row, stream);
                    if let Some(slot) = attributed.slot_since_staging {
                        since_staging.entry(row.stream_id).or_insert((slot, 0)).1 += 1;
                    }
                    attributed.row
                })
                .collect();
            let models =
                import_models(txn, &attributed, request_measurement_type.as_deref()).await?;

            // An overwrite replaces the whole replicate set, not the Nth row by the Nth: stored
            // spot replicates beyond the incoming count would survive a positional upsert and keep
            // double-counting the group, so the tail leaves the group before the insert.
            if conflict == ConflictMode::Overwrite {
                for ((stream_id, time), count) in spot_group_sizes(&models) {
                    displace_spot_tail(txn, stream_id, time, count).await?;
                }
            }

            let mut affected_total = 0usize;
            let mut corrected = 0usize;
            for chunk in models.chunks(CSV_BATCH_SIZE) {
                // A correction corrects the measurement, so it is a decision: recorded before the
                // write, against the values still stored, and only for the rows the write moves
                // (ADR 0008). The count it returns is what the run reports as overwritten.
                if conflict == ConflictMode::Overwrite {
                    let corrections =
                        crate::routes::private::readings::service::record_value_corrections(
                            txn,
                            chunk,
                            "csv_import",
                            crate::routes::private::readings::models::Origin::Csv,
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

            // A stream paired since staging landed its rows under the slot alone: the reprocess
            // the pairing owes resolves their deployment and curve, as it would have had they
            // been stored before the pairing.
            for (stream_id, (slot, rows)) in since_staging {
                data_streams::flows::enqueue_slot_reprocess(txn, stream_id, slot, rows).await?;
            }
            let (inserted, overwritten) =
                import_counts(conflict, affected_total, overlapping, corrected);
            let landed =
                import_written(site_id, since.zip(latest), &param_streams, inserted + overwritten);
            crate::routes::private::readings::service::queue(txn, &landed, &IMPORT_TAIL).await?;
            Ok((affected_total, corrected, models))
        })
        .await
        .map_err(as_db_err)?;

        // Whether a group is a sample is not decided here: `crate::routes::private::readings::service::forms_sample` is the
        // one answer, two or more rows classified spot sharing a slot instant.
        let mut spot_groups: std::collections::HashMap<
            (Uuid, Uuid, chrono::DateTime<chrono::Utc>),
            usize,
        > = std::collections::HashMap::new();
        for m in &models {
            if m.measurement_type.as_ref().as_deref()
                != Some(crate::routes::private::readings::service::SPOT)
            {
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
            .filter(|count| crate::routes::private::readings::service::forms_sample(**count))
            .count();
        ctx.info(&format!(
            "Imported {} readings at {site_name} over {} instants, {replicate_groups} of them replicate groups",
            models.len(),
            distinct_ts.len()
        ))
        .await;

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
                crate::routes::private::sensor_calibrations::service::recompose_from_own_curves_guarded(
                    ctx.db(),
                    sea_orm::sea_query::Expr::cust("TRUE"),
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
            crate::routes::private::collection_events::flows::TouchedEvent,
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
            let window = || {
                use crate::routes::private::collection_events::flows::row;
                use crate::routes::private::readings::models::Column;
                sea_orm::Condition::all()
                    .add(row(Column::StreamId).is_in(stream_ids.clone()))
                    .add(row(Column::Time).gte(*first))
                    .add(row(Column::Time).lte(*last))
            };
            crate::routes::private::readings::service::materialise_samples(ctx.db(), window())
                .await
                .map_err(as_db_err)?;

            // A CSV import is a person entering visits after the fact: manual collection events.
            crate::routes::private::collection_events::service::attach_collection_events(
                ctx.db(),
                window(),
                crate::routes::private::collection_events::service::EventSource::Manual,
            )
            .await
            .map_err(as_db_err)?;

            // The values have landed at their visits; the calculations that read them run without
            // anyone asking (ADR 0007). Read after the attach, which is what gives the rows the
            // events this looks them up by.
            touched_visits = crate::routes::private::collection_events::flows::touched_events(
                ctx.db(),
                window(),
            )
            .await
            .map_err(as_db_err)?;
        }

        let (inserted_total, overwritten) =
            import_counts(conflict, affected_total, overlapping, corrected);
        tracing::info!(site = %site_name, inserted_total, overwritten, "CSV import inserted readings");
        ctx.info(&format!(
            "Wrote {inserted_total} readings and corrected {overwritten}"
        ))
        .await;

        if inserted_total > 0 || overwritten > 0 {
            // Phase 2: derived recompute over the imported timestamps.
            let derived_total = i32::try_from(models.len() + distinct_ts.len()).unwrap_or(i32::MAX);
            ctx.set_progress(
                i32::try_from(models.len()).unwrap_or(i32::MAX),
                Some(derived_total),
            )
            .await;
            let mut refused =
                crate::routes::private::derived_parameters::service::DerivedPass::default();
            for (i, time) in distinct_ts.iter().enumerate() {
                if ctx.is_cancelled() {
                    break;
                }
                if let Ok(slots) = recalculate_derived_at_timestamp(ctx.db(), site_id, *time).await
                {
                    refused.record(&slots, *time);
                }
                if (i + 1) % 500 == 0 {
                    let prog = i32::try_from(models.len() + i + 1).unwrap_or(i32::MAX);
                    ctx.set_progress(prog, Some(derived_total)).await;
                }
            }
            refused.report(ctx.db()).await?;
            ctx.info(&format!(
                "Recomputed the calculations at {} instants",
                distinct_ts.len()
            ))
            .await;

            let app = crate::common::global_app_state();
            crate::routes::private::readings::service::run(
                crate::routes::private::readings::service::Sink {
                    db: ctx.db(),
                    events: ctx.events(),
                    cache: app.as_ref().map(|a| &a.response_cache),
                },
                &import_written(
                    site_id,
                    since.zip(latest),
                    &param_streams,
                    inserted_total + overwritten,
                )
                .touching(touched_visits),
                &IMPORT_TAIL,
                "csv_import",
            )
            .await
            .map_err(as_db_err)?;
        }

        // The staging source has served its purpose, drop it (makes this job non-rerunnable).
        drop_staged(ctx.db(), import_token).await?;

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

/// Retag readings.measurement_type for a sensor/stream scope, then refresh continuous aggregates
/// over the affected window. Backs the bulk reclassification actions (mark sensors low/high
/// frequency, classify sensorless streams): the classification columns (`sensors.data_frequency`,
/// `data_streams.measurement_type`) are updated synchronously by the endpoint; this job makes the
/// existing rows agree. Rerunnable (idempotent, the UPDATE skips rows already at the target).
/// Decompression-safe: portal/lab history lives in compressed (>30-day) chunks.
pub struct MeasurementRetag;

/// The rewrite a `measurement_retag` run makes. `target` is the classification every reading in
/// scope takes; `None` is the 'declared' arm, which joins each reading to its stream and takes the
/// stream's own. The scope also matches by stream ownership: a reading ingested before attribution
/// backfill carries `sensor_id` NULL and belongs to the sensor's streams all the same.
fn retag_readings(
    target: Option<&str>,
    sensor_ids: &[Uuid],
    stream_ids: &[Uuid],
    source_system: Option<&str>,
) -> crate::common::bulk_write::Spanned {
    use crate::routes::private::data_streams::models as data_streams;
    use crate::routes::private::readings::models as readings;
    use sea_orm::sea_query::ExprTrait;

    let r = sea_query::Alias::new("readings");
    let ds = sea_query::Alias::new("data_streams");
    let col = |alias: &sea_query::Alias, column: readings::Column| {
        sea_query::Expr::col((alias.clone(), column))
    };

    let streams_of = |predicate: sea_query::Expr| {
        sea_query::Query::select()
            .column(data_streams::Column::Id)
            .from(data_streams::Entity)
            .and_where(predicate)
            .to_owned()
    };
    let mut scope = sea_query::Condition::any()
        .add(col(&r, readings::Column::SensorId).is_in(sensor_ids.to_vec()))
        .add(col(&r, readings::Column::StreamId).is_in(stream_ids.to_vec()))
        .add(col(&r, readings::Column::StreamId).in_subquery(streams_of(
            sea_query::Expr::col(data_streams::Column::SensorId).is_in(sensor_ids.to_vec()),
        )));
    if let Some(system) = source_system {
        scope = scope.add(col(&r, readings::Column::StreamId).in_subquery(streams_of(
            sea_query::Expr::col(data_streams::Column::SourceSystem).eq(system),
        )));
    }

    // What the write changes and what the span reads are the same rows, so the predicate is built
    // once: the `declared` arm joins `data_streams` to compare against each stream's own value.
    let changing = || {
        match target {
        // sea-query has no IS DISTINCT FROM, and a NULL measurement_type reads as continuous, so
        // the comparison cannot be a plain inequality.
        Some(value) => sea_query::Condition::all().add(sea_query::Expr::cust(format!(
            r#""readings"."measurement_type" IS DISTINCT FROM '{value}'"#
        ))),
        None => sea_query::Condition::all()
            .add(col(&r, readings::Column::StreamId).equals((ds.clone(), data_streams::Column::Id)))
            .add(
                sea_query::Expr::col((ds.clone(), data_streams::Column::MeasurementType))
                    .is_not_null(),
            )
            .add(sea_query::Expr::cust(
                r#""readings"."measurement_type" IS DISTINCT FROM "data_streams"."measurement_type""#,
            )),
    }
    };

    let mut update = sea_query::Query::update();
    update.table(readings::Entity);
    match target {
        Some(value) => {
            update.value(readings::Column::MeasurementType, value);
        }
        None => {
            update
                .value(
                    readings::Column::MeasurementType,
                    sea_query::Expr::col((ds.clone(), data_streams::Column::MeasurementType)),
                )
                .from(data_streams::Entity);
        }
    }
    let update = update
        .cond_where(changing())
        .cond_where(scope.clone())
        .to_owned();

    let mut rows = sea_query::Query::select();
    rows.column(readings::Column::Time).from(readings::Entity);
    if target.is_none() {
        rows.from(data_streams::Entity);
    }
    let rows = rows.cond_where(changing()).cond_where(scope).to_owned();

    crate::common::bulk_write::Spanned::new(rows, update)
}

#[async_trait]
impl Job for MeasurementRetag {
    fn name(&self) -> &'static str {
        "measurement_retag"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let params = ctx.params();
        let target = params
            .get("target")
            .and_then(serde_json::Value::as_str)
            .filter(|t| {
                crate::routes::private::readings::service::retag_target_rejection(t).is_none()
            })
            .ok_or_else(|| DbErr::Custom("measurement_retag needs target".to_string()))?
            .to_string();
        // 'declared' aligns each reading with its own stream's classification, for source systems
        // that mix grab and logger columns.
        let declared = target == crate::routes::private::readings::service::RETAG_DECLARED;
        let sensor_ids = uuid_array(params, "sensor_ids");
        let stream_ids = uuid_array(params, "stream_ids");
        let source_system = params
            .get("source_system")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if sensor_ids.is_empty() && stream_ids.is_empty() && source_system.is_none() {
            return Err(DbErr::Custom(
                "measurement_retag needs sensor_ids, stream_ids, or source_system".to_string(),
            ));
        }

        // The family guard holds here, not only on the HTTP routes: a stored job row is replayed
        // by rerun with its params verbatim, so a route-only guard is bypassed by replaying a row
        // that predates it. 'spot' is what a family already is, and 'declared' realigns readings
        // with the stream's own declaration, which every write path holds at 'spot'.
        if matches!(target.as_str(), "continuous" | "derived") {
            let families =
                crate::routes::private::data_streams::service::family_keys_in_retag_scope(
                    ctx.db(),
                    &sensor_ids,
                    &stream_ids,
                    source_system.as_deref(),
                )
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
            crate::routes::private::data_streams::service::refuse_family_retag(&families, &target)
                .map_err(|e| DbErr::Custom(e.to_string()))?;
        }

        // A stream declaring a different classification will keep writing its own value on
        // ingest, so the retag would drift back; surface the conflict in the job timeline.
        if !declared {
            let conflicting = super::service::streams_declaring_other_type(
                ctx.db(),
                &target,
                &sensor_ids,
                &stream_ids,
                source_system.as_deref(),
            )
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;
            for (system, key) in &conflicting {
                ctx.log(
                    "warn",
                    &format!(
                        "Stream {system}/{key} declares a different measurement_type; future ingest will keep writing its declared value. Retag the stream too or use target 'declared'."
                    ),
                    serde_json::json!({}),
                )
                .await;
            }
        }

        ctx.info(&format!("Retagging readings in scope to '{target}'"))
            .await;
        let touched = crate::common::bulk_write::guarded_mutation(
            ctx.db(),
            retag_readings(
                declared
                    .then_some(())
                    .map_or(Some(target.as_str()), |()| None),
                &sensor_ids,
                &stream_ids,
                source_system.as_deref(),
            ),
        )
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
        let retagged = touched.rows;

        // Membership in the rollups changed (spot and derived are excluded), so every aggregate is
        // refreshed over what the rewrite touched. A failure here leaves the rollups holding the
        // old membership, so it fails the job rather than being logged.
        let Some((lo, hi)) = touched.span() else {
            ctx.info("Nothing to retag, every reading in scope already matches")
                .await;
            return Ok(0);
        };
        let refreshed = crate::common::aggregates::refresh(
            ctx.db(),
            crate::common::aggregates::Window::Range(lo, hi),
        )
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.info(&refreshed.line()).await;

        // Reclassified rows change what bounded cached responses would serve.
        if retagged > 0
            && let Some(state) = crate::common::global_app_state()
        {
            state.response_cache.invalidate_all();
        }

        ctx.report(
            JobReport::new()
                .scope("target", target)
                .scope("from", lo.to_rfc3339())
                .scope("until", hi.to_rfc3339())
                .count("readings_retagged", retagged),
        )
        .await;
        Ok(retagged.try_into().unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
#[path = "tests/retag_rewrite.rs"]
mod retag_rewrite_tests;

#[cfg(test)]
#[path = "tests/csv_import_job.rs"]
mod csv_import_job_tests;
