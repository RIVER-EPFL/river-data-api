//! Migration of wrong-shape portal streams onto their replicate-family streams.
//!
//! History: each portal `_avg` column was synced as its own stream and paired to a slot; the
//! replicate columns never left the portal. The family streams (source_key `<old_key>:reps`)
//! arrive unpaired and are backfilled by the sync service while invisible. The
//! `replicate_reconciliation` job then, per family: verifies the family's would-be-served values
//! against the old avg readings, pairs the family stream to the old stream's slot, materialises
//! samples, and re-verifies the trigger-computed statistics, all in one transaction, so a
//! failing verification rolls the family back to exactly the prior state. Nothing is deleted.
//!
//! Deletion is its own job (`replicate_reconciliation_delete`), which re-verifies each family and
//! only then removes the obsolete avg stream and its readings. The destructive step is therefore
//! always behind two verifications and an explicit second operator action.

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::{ConnectionTrait, DbErr, Statement};
use uuid::Uuid;

use super::job::Job;
use super::lifecycle::JobContext;
use crate::common::bulk_write;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::sample_groups;
use crate::routes::private::sync::replicate_audit::{DEFAULT_ABS_TOL, DEFAULT_REL_TOL};

/// A family stream and the legacy avg stream it supersedes. The pairing is exact, not guessed:
/// the family's source_key is the old key plus the `:reps` suffix the sync service appends.
#[derive(Debug, Clone)]
pub struct FamilyPair {
    pub new_id: Uuid,
    pub new_key: String,
    pub new_paired: bool,
    pub old_id: Uuid,
    pub old_key: String,
    pub old_site_parameter_id: Option<Uuid>,
}

pub async fn family_pairs<C: ConnectionTrait>(
    conn: &C,
    source_system: &str,
) -> Result<Vec<FamilyPair>, DbErr> {
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT n.id AS new_id, n.source_key AS new_key,
                    n.site_parameter_id IS NOT NULL AS new_paired,
                    o.id AS old_id, o.source_key AS old_key, o.site_parameter_id AS old_sp
             FROM data_streams n
             JOIN data_streams o
               ON o.source_system = n.source_system
              AND o.source_key = left(n.source_key, length(n.source_key) - 5)
             WHERE n.source_system = $1
               AND n.source_key LIKE '%:reps'
               AND n.metadata ? 'replicates'
             ORDER BY n.source_key",
            [source_system.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(FamilyPair {
                new_id: r.try_get("", "new_id")?,
                new_key: r.try_get("", "new_key")?,
                new_paired: r.try_get("", "new_paired")?,
                old_id: r.try_get("", "old_id")?,
                old_key: r.try_get("", "old_key")?,
                old_site_parameter_id: r.try_get("", "old_sp")?,
            })
        })
        .collect()
}

/// The tolerance bound between two value expressions, shared with the sync-time audit's
/// `stats_agree` (same relative form, absolute floor, and portal quantum floor).
fn bound_sql(a: &str, b: &str, rel_bind: &str) -> String {
    crate::routes::private::sync::replicate_audit::bound_sql(a, b, rel_bind, DEFAULT_ABS_TOL)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyOutcome {
    pub compared: i64,
    pub mismatched: i64,
}

/// Compare what the family stream will serve at each of the old stream's instants against the old
/// avg reading: `COALESCE(samples.mean, AVG over the family group)` vs the old served value.
/// Before cutover the samples side is empty and the group AVG stands in for it; after
/// materialisation the samples.mean is the trigger-computed number. `$3` = relative tolerance.
async fn verify_family<C: ConnectionTrait>(
    conn: &C,
    old_id: Uuid,
    new_id: Uuid,
    rel_tol: f64,
) -> Result<VerifyOutcome, DbErr> {
    let bound = bound_sql("served.v", "o.v", "$3");
    let row = conn
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "WITH o AS (
                     SELECT time, COALESCE(calibrated_value, raw_value) AS v
                     FROM readings
                     WHERE stream_id = $1 AND replicate_index = 0 AND is_flagged IS NOT TRUE
                 ),
                 served AS (
                     SELECT r.time,
                            COALESCE(MAX(s.mean), AVG(COALESCE(r.calibrated_value, r.raw_value)))
                                AS v
                     FROM readings r
                     LEFT JOIN samples s ON s.id = r.sample_id
                     WHERE r.stream_id = $2 AND r.is_flagged IS NOT TRUE
                     GROUP BY r.time
                 )
                 SELECT COUNT(*)::bigint AS compared,
                        COUNT(*) FILTER (
                            WHERE served.v IS NULL OR abs(served.v - o.v) > {bound}
                        )::bigint AS mismatched
                 FROM o LEFT JOIN served ON served.time = o.time"
            ),
            [old_id.into(), new_id.into(), rel_tol.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("verification returned no row".to_string()))?;
    Ok(VerifyOutcome {
        compared: row.try_get("", "compared")?,
        mismatched: row.try_get("", "mismatched")?,
    })
}

/// The first mismatching instants, for the job detail an operator reviews.
async fn mismatch_examples<C: ConnectionTrait>(
    conn: &C,
    old_id: Uuid,
    new_id: Uuid,
    rel_tol: f64,
    limit: usize,
) -> Result<Vec<serde_json::Value>, DbErr> {
    let bound = bound_sql("served.v", "o.v", "$3");
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "WITH o AS (
                     SELECT time, COALESCE(calibrated_value, raw_value) AS v
                     FROM readings
                     WHERE stream_id = $1 AND replicate_index = 0 AND is_flagged IS NOT TRUE
                 ),
                 served AS (
                     SELECT r.time,
                            COALESCE(MAX(s.mean), AVG(COALESCE(r.calibrated_value, r.raw_value)))
                                AS v
                     FROM readings r
                     LEFT JOIN samples s ON s.id = r.sample_id
                     WHERE r.stream_id = $2 AND r.is_flagged IS NOT TRUE
                     GROUP BY r.time
                 )
                 SELECT o.time, o.v AS old_value, served.v AS new_value
                 FROM o LEFT JOIN served ON served.time = o.time
                 WHERE served.v IS NULL OR abs(served.v - o.v) > {bound}
                 ORDER BY o.time
                 LIMIT {limit}"
            ),
            [old_id.into(), new_id.into(), rel_tol.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let time: chrono::DateTime<chrono::FixedOffset> = r.try_get("", "time")?;
            let old_value: Option<f64> = r.try_get("", "old_value")?;
            let new_value: Option<f64> = r.try_get("", "new_value")?;
            Ok(serde_json::json!({
                "time": time.with_timezone(&Utc),
                "old_value": old_value,
                "new_value": new_value,
                "delta": old_value.zip(new_value).map(|(a, b)| a - b),
            }))
        })
        .collect()
}

/// Old-stream instants the family stream has no readings for yet. Non-zero means the backfill has
/// not covered the old history and the family is not ready for cutover.
async fn missing_instants<C: ConnectionTrait>(
    conn: &C,
    old_id: Uuid,
    new_id: Uuid,
) -> Result<i64, DbErr> {
    let row = conn
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*)::bigint AS missing
             FROM readings o
             WHERE o.stream_id = $1 AND o.replicate_index = 0
               AND NOT EXISTS (
                   SELECT 1 FROM readings n WHERE n.stream_id = $2 AND n.time = o.time
               )",
            [old_id.into(), new_id.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("missing-instants probe returned no row".to_string()))?;
    row.try_get("", "missing")
}

/// Pair the family stream to the old stream's slot, backfill attribution, materialise samples and
/// verify the trigger-computed statistics: one transaction, rolled back whole on any failure, so
/// a family either cuts over verified or stays exactly as it was.
async fn cutover_family(
    db: &sea_orm::DatabaseConnection,
    pair: &FamilyPair,
    site_parameter_id: Uuid,
    rel_tol: f64,
) -> AppResult<VerifyOutcome> {
    bulk_write::guarded(db, async |txn| {
        txn.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SET LOCAL lock_timeout = '5s'".to_owned(),
        ))
        .await?;

        let slot = txn
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT site_id, parameter_id FROM site_parameters WHERE id = $1",
                [site_parameter_id.into()],
            ))
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("site_parameter {site_parameter_id} not found"))
            })?;
        let site_id: Uuid = slot.try_get("", "site_id")?;
        let parameter_id: Uuid = slot.try_get("", "parameter_id")?;

        let claimed = txn
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE data_streams
                 SET site_parameter_id = $1, paired_at = NOW(), updated_at = NOW()
                 WHERE id = $2 AND site_parameter_id IS NULL",
                [site_parameter_id.into(), pair.new_id.into()],
            ))
            .await?
            .rows_affected();
        if claimed == 0 {
            return Err(AppError::Conflict(format!(
                "family stream {} is already paired",
                pair.new_key
            )));
        }

        // Attribution comes from the pairing. The family's sensor (the lab instrument, when the
        // family carries curves) is already frozen on the stream; readings keep whatever sensor
        // they resolved at ingest, and rows from before pairing gain the stream's.
        bulk_write::mutation(
            txn,
            Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "UPDATE readings r
                 SET site_id = $1, parameter_id = $2,
                     sensor_id = COALESCE(r.sensor_id, ds.sensor_id),
                     measurement_type = COALESCE(r.measurement_type, 'spot')
                 FROM data_streams ds
                 WHERE ds.id = r.stream_id AND r.stream_id = $3 AND r.site_id IS NULL",
                [site_id.into(), parameter_id.into(), pair.new_id.into()],
            ),
        )
        .await?;

        // The replicate spec is the writer's declaration that these groups are collection events,
        // so a single-replicate instant still forms its samples row, as on declared ingest.
        sample_groups::materialise_samples(txn, "r.stream_id = $1", vec![pair.new_id.into()], true)
            .await?;

        // Trigger-computed verification: the row triggers have populated samples.mean inside this
        // transaction, so a disagreement here rolls everything back.
        let verified = verify_family(txn, pair.old_id, pair.new_id, rel_tol).await?;
        if verified.mismatched > 0 {
            return Err(AppError::Conflict(format!(
                "family {}: {} of {} instants disagree with the old served values after \
                 materialisation",
                pair.new_key, verified.mismatched, verified.compared
            )));
        }
        Ok(verified)
    })
    .await
}

fn job_inputs(params: &serde_json::Value) -> Result<(String, f64, bool), DbErr> {
    let source_system = params
        .get("source_system")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DbErr::Custom("replicate reconciliation needs source_system".to_string()))?
        .to_string();
    let rel_tol = params
        .get("tolerance")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(DEFAULT_REL_TOL);
    let dry_run = params
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok((source_system, rel_tol, dry_run))
}

/// Member-column streams that should never exist (a replicate synced as its own stream by some
/// past error): reported into the job detail, never touched.
async fn stray_member_streams<C: ConnectionTrait>(
    conn: &C,
    source_system: &str,
) -> Result<Vec<String>, DbErr> {
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT o.source_key
             FROM data_streams n
             JOIN LATERAL jsonb_array_elements_text(n.metadata->'replicates'->'source_columns')
                      AS member(col) ON TRUE
             JOIN data_streams o
               ON o.source_system = n.source_system
              AND o.source_key = split_part(n.source_key, ':', 1) || ':' || member.col
             WHERE n.source_system = $1 AND n.metadata ? 'replicates'",
            [source_system.into()],
        ))
        .await?;
    rows.iter().map(|r| r.try_get("", "source_key")).collect()
}

/// Migrate + verify. Per family: readiness probe, pre-verification over the group averages,
/// transactional cutover with trigger-computed re-verification. Never deletes anything.
pub struct ReplicateReconciliation;

#[async_trait]
impl Job for ReplicateReconciliation {
    fn name(&self) -> &'static str {
        "replicate_reconciliation"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let (source_system, rel_tol, dry_run) = job_inputs(ctx.params())?;
        let db = ctx.db();

        let pairs = family_pairs(db, &source_system).await?;
        let strays = stray_member_streams(db, &source_system).await?;
        if !strays.is_empty() {
            ctx.log(
                "warn",
                &format!(
                    "{} streams carry a replicate member column as their own stream (past sync \
                     error); they are not migrated by this job",
                    strays.len()
                ),
                serde_json::json!({ "streams": strays }),
            )
            .await;
        }

        let total = i32::try_from(pairs.len()).unwrap_or(i32::MAX);
        let mut cut_over = 0i64;
        let mut already = 0i64;
        let mut not_ready = 0i64;
        let mut unpaired_old = 0i64;
        let mut preverify_failed = 0i64;
        let mut cutover_failed = 0i64;
        let mut families = Vec::new();
        let mut mismatches = Vec::new();

        for (done, pair) in pairs.iter().enumerate() {
            if ctx.is_cancelled() {
                ctx.info("Cancelled between families; completed cutovers stand")
                    .await;
                break;
            }
            ctx.set_progress(i32::try_from(done).unwrap_or(i32::MAX), Some(total))
                .await;

            let mut family = serde_json::json!({
                "family": pair.new_key,
                "old_stream_id": pair.old_id,
                "new_stream_id": pair.new_id,
            });
            let record = |family: &mut serde_json::Value, status: &str| {
                family["status"] = serde_json::json!(status);
            };

            if pair.new_paired {
                already += 1;
                record(&mut family, "already_migrated");
                families.push(family);
                continue;
            }
            let Some(site_parameter_id) = pair.old_site_parameter_id else {
                unpaired_old += 1;
                record(&mut family, "old_stream_unpaired");
                families.push(family);
                continue;
            };

            let missing = missing_instants(db, pair.old_id, pair.new_id).await?;
            if missing > 0 {
                not_ready += 1;
                family["missing_instants"] = serde_json::json!(missing);
                record(&mut family, "awaiting_backfill");
                families.push(family);
                continue;
            }

            let pre = verify_family(db, pair.old_id, pair.new_id, rel_tol).await?;
            family["compared"] = serde_json::json!(pre.compared);
            if pre.mismatched > 0 {
                preverify_failed += 1;
                family["mismatched"] = serde_json::json!(pre.mismatched);
                record(&mut family, "preverify_failed");
                if mismatches.len() < 100 {
                    let mut examples =
                        mismatch_examples(db, pair.old_id, pair.new_id, rel_tol, 10).await?;
                    for e in &mut examples {
                        e["family"] = serde_json::json!(pair.new_key);
                    }
                    mismatches.extend(examples);
                    mismatches.truncate(100);
                }
                families.push(family);
                continue;
            }

            if dry_run {
                record(&mut family, "ready");
                families.push(family);
                continue;
            }

            match cutover_family(db, pair, site_parameter_id, rel_tol).await {
                Ok(verified) => {
                    cut_over += 1;
                    family["compared"] = serde_json::json!(verified.compared);
                    record(&mut family, "migrated");
                }
                Err(e) => {
                    cutover_failed += 1;
                    family["error"] = serde_json::json!(e.to_string());
                    record(&mut family, "cutover_failed");
                    ctx.log(
                        "warn",
                        &format!("family {} rolled back: {e}", pair.new_key),
                        serde_json::json!({}),
                    )
                    .await;
                }
            }
            families.push(family);
        }

        ctx.set_detail(serde_json::json!({
            "scope": {
                "source_system": source_system,
                "dry_run": dry_run,
                "tolerance": rel_tol,
            },
            "counts": {
                "families": pairs.len(),
                "migrated": cut_over,
                "already_migrated": already,
                "awaiting_backfill": not_ready,
                "old_stream_unpaired": unpaired_old,
                "preverify_failed": preverify_failed,
                "cutover_failed": cutover_failed,
                "stray_member_streams": strays.len(),
            },
            "families": families,
            "mismatches": mismatches,
        }))
        .await;

        if let Some(state) = crate::common::global_app_state() {
            state.response_cache.invalidate_all();
        }
        Ok(cut_over)
    }
}

/// The destructive half: for every family already migrated, re-verify the served values one more
/// time and only then delete the obsolete avg stream's readings, status events and stream row.
pub struct ReplicateReconciliationDelete;

#[async_trait]
impl Job for ReplicateReconciliationDelete {
    fn name(&self) -> &'static str {
        "replicate_reconciliation_delete"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let (source_system, rel_tol, dry_run) = job_inputs(ctx.params())?;
        let db = ctx.db();

        let pairs = family_pairs(db, &source_system).await?;
        let total = i32::try_from(pairs.len()).unwrap_or(i32::MAX);
        let mut deleted_streams = 0i64;
        let mut deleted_readings = 0i64;
        let mut skipped_unmigrated = 0i64;
        let mut verify_failed = 0i64;
        let mut families = Vec::new();

        for (done, pair) in pairs.iter().enumerate() {
            if ctx.is_cancelled() {
                ctx.info("Cancelled between families; completed deletions stand")
                    .await;
                break;
            }
            ctx.set_progress(i32::try_from(done).unwrap_or(i32::MAX), Some(total))
                .await;

            let mut family =
                serde_json::json!({ "family": pair.new_key, "old_stream_id": pair.old_id });

            if !pair.new_paired {
                skipped_unmigrated += 1;
                family["status"] = serde_json::json!("not_migrated");
                families.push(family);
                continue;
            }

            let verified = verify_family(db, pair.old_id, pair.new_id, rel_tol).await?;
            family["compared"] = serde_json::json!(verified.compared);
            if verified.mismatched > 0 {
                verify_failed += 1;
                family["mismatched"] = serde_json::json!(verified.mismatched);
                family["status"] = serde_json::json!("verify_failed");
                families.push(family);
                continue;
            }

            if dry_run {
                family["status"] = serde_json::json!("would_delete");
                families.push(family);
                continue;
            }

            let removed = bulk_write::guarded(db, async |txn| {
                let removed = bulk_write::mutation(
                    txn,
                    Statement::from_sql_and_values(
                        sea_orm::DatabaseBackend::Postgres,
                        "DELETE FROM readings WHERE stream_id = $1",
                        [pair.old_id.into()],
                    ),
                )
                .await?
                .rows;
                txn.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM status_events WHERE stream_id = $1",
                    [pair.old_id.into()],
                ))
                .await?;
                txn.execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM data_streams WHERE id = $1",
                    [pair.old_id.into()],
                ))
                .await?;
                Ok(removed)
            })
            .await
            .map_err(|e| DbErr::Custom(e.to_string()))?;

            deleted_streams += 1;
            deleted_readings += i64::try_from(removed).unwrap_or(0);
            family["status"] = serde_json::json!("deleted");
            family["readings_deleted"] = serde_json::json!(removed);
            families.push(family);
        }

        ctx.set_detail(serde_json::json!({
            "scope": {
                "source_system": source_system,
                "dry_run": dry_run,
                "tolerance": rel_tol,
            },
            "counts": {
                "families": pairs.len(),
                "streams_deleted": deleted_streams,
                "readings_deleted": deleted_readings,
                "skipped_unmigrated": skipped_unmigrated,
                "verify_failed": verify_failed,
            },
            "families": families,
        }))
        .await;

        if let Some(state) = crate::common::global_app_state() {
            state.response_cache.invalidate_all();
        }
        Ok(deleted_readings)
    }
}
