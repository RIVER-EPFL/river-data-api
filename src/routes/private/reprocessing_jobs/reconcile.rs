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
use sea_orm::Order;
use sea_orm::sea_query::{
    Alias, CommonTableExpression, Condition, Expr, ExprTrait as _, JoinType, PostgresQueryBuilder,
    Query as SeaQuery, WithClause,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
    Statement,
};
use uuid::Uuid;

use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::model as samples;
use crate::routes::private::readings::status_events::model as status_events;

use super::job::Job;
use super::lifecycle::{JobContext, JobReport};
use crate::common::bulk_write;
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams;
use crate::routes::private::sites::parameters as site_parameters;
use crate::routes::private::sync::service::{DEFAULT_ABS_TOL, DEFAULT_REL_TOL};

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
        .query_all_raw(Statement::from_sql_and_values(
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
        .map(|r| PairRow::from_query_result(r, ""))
        .map(|r| {
            r.map(|p| FamilyPair {
                new_id: p.new_id,
                new_key: p.new_key,
                new_paired: p.new_paired,
                old_id: p.old_id,
                old_key: p.old_key,
                old_site_parameter_id: p.old_sp,
            })
        })
        .collect()
}

/// One retired family and the replicate family that replaces it, as the pairing query returns it.
#[derive(FromQueryResult)]
struct PairRow {
    new_id: Uuid,
    new_key: String,
    new_paired: bool,
    old_id: Uuid,
    old_key: String,
    old_sp: Option<Uuid>,
}

/// The tolerance bound between two value expressions, shared with the sync-time audit's
/// `stats_agree` (same relative form, absolute floor, and portal quantum floor).
fn bound_sql(a: &str, b: &str, rel_bind: &str) -> String {
    crate::routes::private::sync::service::bound_sql(a, b, rel_bind, DEFAULT_ABS_TOL)
}

/// The two sides one family verification compares: `o`, the old avg stream's served value at each
/// instant, and `served`, what the family stream will serve there (the trigger-computed sample
/// mean, or the group average before materialisation).
fn comparison_ctes(old_id: Uuid, new_id: Uuid) -> WithClause {
    let old = SeaQuery::select()
        .column(readings::Column::Time)
        .expr_as(
            crate::common::served::continuous_value_of(Alias::new("readings")),
            Alias::new("v"),
        )
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::StreamId).eq(old_id))
        .and_where(Expr::col(readings::Column::ReplicateIndex).eq(0))
        .and_where(Expr::cust("is_flagged IS NOT TRUE"))
        .take();

    let r = Alias::new("r");
    let smp = Alias::new("s");
    let served = SeaQuery::select()
        .column((r.clone(), readings::Column::Time))
        .expr_as(
            Expr::cust("COALESCE(MAX(s.mean), AVG(COALESCE(r.calibrated_value, r.raw_value)))"),
            Alias::new("v"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            smp.clone(),
            Expr::col((smp, samples::Column::Id)).equals((r.clone(), readings::Column::SampleId)),
        )
        .and_where(Expr::col((r.clone(), readings::Column::StreamId)).eq(new_id))
        .and_where(Expr::cust("r.is_flagged IS NOT TRUE"))
        .add_group_by([Expr::col((r, readings::Column::Time)).into()])
        .take();

    let cte = |name: &str, query: sea_orm::sea_query::SelectStatement| {
        let mut cte = CommonTableExpression::new();
        cte.table_name(Alias::new(name)).query(query);
        cte
    };
    WithClause::new()
        .cte(cte("o", old))
        .cte(cte("served", served))
        .to_owned()
}

#[derive(Debug, Clone, Copy, Default, FromQueryResult)]
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
    let bound = bound_sql("served.v", "o.v", "$1");
    let (sql, values) = SeaQuery::select()
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("compared"))
        .expr_as(
            Expr::cust_with_values(
                format!(
                    "COUNT(*) FILTER (WHERE served.v IS NULL OR abs(served.v - o.v) > {bound})::bigint"
                ),
                [rel_tol],
            ),
            Alias::new("mismatched"),
        )
        .from(Alias::new("o"))
        .join_as(
            JoinType::LeftJoin,
            Alias::new("served"),
            Alias::new("served"),
            Expr::cust("served.time = o.time"),
        )
        .take()
        .with(comparison_ctes(old_id, new_id))
        .build(PostgresQueryBuilder);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| DbErr::Custom("verification returned no row".to_string()))?;
    VerifyOutcome::from_query_result(&row, "")
}

/// The first mismatching instants, for the job detail an operator reviews.
async fn mismatch_examples<C: ConnectionTrait>(
    conn: &C,
    old_id: Uuid,
    new_id: Uuid,
    rel_tol: f64,
    limit: usize,
) -> Result<Vec<serde_json::Value>, DbErr> {
    let bound = bound_sql("served.v", "o.v", "$1");
    let (sql, values) = SeaQuery::select()
        .expr(Expr::cust("o.time"))
        .expr_as(Expr::cust("o.v"), Alias::new("old_value"))
        .expr_as(Expr::cust("served.v"), Alias::new("new_value"))
        .from(Alias::new("o"))
        .join_as(
            JoinType::LeftJoin,
            Alias::new("served"),
            Alias::new("served"),
            Expr::cust("served.time = o.time"),
        )
        .cond_where(Condition::any().add(Expr::cust("served.v IS NULL")).add(
            Expr::cust_with_values(format!("abs(served.v - o.v) > {bound}"), [rel_tol]),
        ))
        .order_by_expr(Expr::cust("o.time"), Order::Asc)
        .limit(limit as u64)
        .take()
        .with(comparison_ctes(old_id, new_id))
        .build(PostgresQueryBuilder);
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    rows.iter()
        .map(|r| MismatchRow::from_query_result(r, ""))
        .map(|r| {
            r.map(|m| {
                serde_json::json!({
                    "time": m.time.with_timezone(&Utc),
                    "old_value": m.old_value,
                    "new_value": m.new_value,
                    "delta": m.old_value.zip(m.new_value).map(|(a, b)| a - b),
                })
            })
        })
        .collect()
}

/// One instant where the retired family and its replacement disagree.
#[derive(FromQueryResult)]
struct MismatchRow {
    time: chrono::DateTime<chrono::FixedOffset>,
    old_value: Option<f64>,
    new_value: Option<f64>,
}

/// Old-stream instants the family stream has no readings for yet. Non-zero means the backfill has
/// not covered the old history and the family is not ready for cutover.
async fn missing_instants<C: ConnectionTrait>(
    conn: &C,
    old_id: Uuid,
    new_id: Uuid,
) -> Result<i64, DbErr> {
    let o = Alias::new("o");
    let n = Alias::new("n");
    let (sql, values) = SeaQuery::select()
        .expr_as(Expr::cust("COUNT(*)::bigint"), Alias::new("missing"))
        .from_as(readings::Entity, o.clone())
        .and_where(Expr::col((o.clone(), readings::Column::StreamId)).eq(old_id))
        .and_where(Expr::col((o.clone(), readings::Column::ReplicateIndex)).eq(0))
        .and_where(
            Expr::exists(
                SeaQuery::select()
                    .expr(Expr::value(1))
                    .from_as(readings::Entity, n.clone())
                    .and_where(Expr::col((n.clone(), readings::Column::StreamId)).eq(new_id))
                    .and_where(
                        Expr::col((n, readings::Column::Time)).equals((o, readings::Column::Time)),
                    )
                    .take(),
            )
            .not(),
        )
        .take()
        .build(PostgresQueryBuilder);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
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
        txn.execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SET LOCAL lock_timeout = '5s'".to_owned(),
        ))
        .await?;

        let (site_id, parameter_id) =
            site_parameters::models::Entity::find_by_id(site_parameter_id)
                .select_only()
                .column(site_parameters::models::Column::SiteId)
                .column(site_parameters::models::Column::ParameterId)
                .into_tuple::<(Uuid, Uuid)>()
                .one(txn)
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!("site_parameter {site_parameter_id} not found"))
                })?;

        let claimed = data_streams::models::Entity::update_many()
            .col_expr(
                data_streams::models::Column::SiteParameterId,
                Expr::value(site_parameter_id),
            )
            .col_expr(
                data_streams::models::Column::PairedAt,
                Expr::current_timestamp(),
            )
            .col_expr(
                data_streams::models::Column::UpdatedAt,
                Expr::current_timestamp(),
            )
            .filter(data_streams::models::Column::Id.eq(pair.new_id))
            .filter(data_streams::models::Column::SiteParameterId.is_null())
            .exec(txn)
            .await?
            .rows_affected;
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
            SeaQuery::update()
                .table(readings::Entity)
                .value(readings::Column::SiteId, site_id)
                .value(readings::Column::ParameterId, parameter_id)
                .value(
                    readings::Column::SensorId,
                    Expr::cust("COALESCE(readings.sensor_id, data_streams.sensor_id)"),
                )
                .value(
                    readings::Column::MeasurementType,
                    Expr::cust("COALESCE(readings.measurement_type, 'spot')"),
                )
                .from(data_streams::models::Entity)
                .and_where(Expr::cust("data_streams.id = readings.stream_id"))
                .and_where(Expr::col(readings::Column::StreamId).eq(pair.new_id))
                .and_where(Expr::col(readings::Column::SiteId).is_null())
                .take(),
        )
        .await?;

        crate::routes::private::readings::service::materialise_samples(
            txn,
            "r.stream_id = $1",
            vec![pair.new_id.into()],
        )
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
        .query_all_raw(Statement::from_sql_and_values(
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

        ctx.report(
            JobReport::new()
                .scope("source_system", source_system.clone())
                .scope("dry_run", dry_run)
                .scope("tolerance", rel_tol)
                .scope("families", families.clone())
                .scope("mismatches", mismatches.clone())
                .count("families", pairs.len())
                .count("migrated", cut_over)
                .count("already_migrated", already)
                .count("awaiting_backfill", not_ready)
                .count("old_stream_unpaired", unpaired_old)
                .count("preverify_failed", preverify_failed)
                .count("cutover_failed", cutover_failed)
                .count("stray_member_streams", strays.len()),
        )
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
                    SeaQuery::delete()
                        .from_table(readings::Entity)
                        .and_where(Expr::col(readings::Column::StreamId).eq(pair.old_id))
                        .take(),
                )
                .await?
                .rows;
                status_events::Entity::delete_many()
                    .filter(status_events::Column::StreamId.eq(pair.old_id))
                    .exec(txn)
                    .await?;
                data_streams::models::Entity::delete_by_id(pair.old_id)
                    .exec(txn)
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

        ctx.report(
            JobReport::new()
                .scope("source_system", source_system.clone())
                .scope("dry_run", dry_run)
                .scope("tolerance", rel_tol)
                .scope("families", families.clone())
                .count("families", pairs.len())
                .count("streams_deleted", deleted_streams)
                .count("readings_deleted", deleted_readings)
                .count("skipped_unmigrated", skipped_unmigrated)
                .count("verify_failed", verify_failed),
        )
        .await;

        if let Some(state) = crate::common::global_app_state() {
            state.response_cache.invalidate_all();
        }
        Ok(deleted_readings)
    }
}
