//! Sync orchestration: the reconciliation enqueue and the jobs that drive the ledger sweeps.

use async_trait::async_trait;
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use sea_orm::Order;
use sea_orm::sea_query::extension::postgres::PgBinOper;
use sea_orm::sea_query::{
    Alias, CommonTableExpression, Condition, Expr, ExprTrait as _, JoinType, PostgresQueryBuilder,
    Query as SeaQuery, WithClause,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
    Statement,
};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::bulk_write;
use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::models::receipts;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::readings::status_events::models as status_events;
use crate::routes::private::reprocessing_jobs;
use crate::routes::private::reprocessing_jobs::service as jobs;
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport, Schedule};
use crate::routes::private::site_parameters;
use crate::routes::private::sync::service::{DEFAULT_ABS_TOL, DEFAULT_REL_TOL};

use super::models::*;

/// Backdate every `(site, parameter)` slot that has a deployment, re-deriving its readings from the
/// current deployment + calibration timelines. The slot set is recomputed inside the job from
/// `sensor_deployments`, so a rerun always reflects the current deployment topology. Backs the
/// The instant `days` days before now, as a retention prune's cutoff.
pub(super) fn days_ago(days: u32) -> sea_orm::prelude::DateTimeWithTimeZone {
    (chrono::Utc::now() - chrono::Duration::days(i64::from(days))).into()
}

/// What the sweeper writes into a closed row's `errors`, so the sweep and anything reading the
/// reason cannot drift apart.
pub(super) const SWEPT_REASON: &str = "Closed by sweeper: service stopped reporting";

/// A cycle still reporting `running` whose start is older than the threshold. The cut-off is
/// computed here rather than left to the statement, so the window is a typed instant.
pub(super) fn stale_running(cutoff: DateTime<Utc>) -> Condition {
    Condition::all()
        .add(events::Column::Status.eq("running"))
        .add(events::Column::StartedAt.lt(cutoff))
}

/// Close 'running' sync_events older than the staleness threshold; returns the row count.
pub async fn sweep_stale_sync_events(
    db: &sea_orm::DatabaseConnection,
    stale_after_seconds: u64,
) -> Result<u64, DbErr> {
    let cutoff =
        Utc::now() - Duration::seconds(i64::try_from(stale_after_seconds).unwrap_or(i64::MAX));
    let appended = Expr::col(events::Column::Errors)
        .if_null(Expr::val(serde_json::json!([])))
        .binary(
            PgBinOper::Concatenate,
            Expr::val(serde_json::json!([SWEPT_REASON])),
        );
    let res = events::Entity::update_many()
        .col_expr(events::Column::Status, Expr::value("failed"))
        .col_expr(events::Column::CompletedAt, Expr::current_timestamp())
        .col_expr(events::Column::Errors, appended)
        .filter(stale_running(cutoff))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

pub(super) async fn enqueue_reconciliation(
    state: &AppState,
    trigger_type: &str,
    payload: &StartReconciliationRequest,
) -> AppResult<Json<StartReconciliationResponse>> {
    if payload.source_system.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_system is required".to_string(),
        ));
    }
    // One live run per (job kind, source): a second concurrent migration over the same streams
    // would race the per-family claims for no benefit.
    // The live set for one job kind is a handful of rows, so the `params` match is made here
    // rather than as a jsonb predicate the typed column API cannot express.
    let active = reprocessing_jobs::models::job::Entity::find()
        .filter(reprocessing_jobs::models::job::Column::TriggerType.eq(trigger_type))
        .filter(
            reprocessing_jobs::models::job::Column::Status.is_in(["queued", "running", "retrying"]),
        )
        .all(&state.db)
        .await?
        .into_iter()
        .find(|job| {
            job.params
                .get("source_system")
                .and_then(serde_json::Value::as_str)
                == Some(payload.source_system.as_str())
        })
        .map(|job| job.id);
    if let Some(id) = active {
        return Err(AppError::Conflict(format!(
            "{trigger_type} already running for {} (job {id})",
            payload.source_system
        )));
    }

    let job_id = jobs::enqueue(
        &state.db,
        trigger_type,
        None,
        None,
        &serde_json::json!({
            "source_system": payload.source_system,
            "dry_run": payload.dry_run,
            "tolerance": payload.tolerance,
        }),
        None,
    )
    .await?
    .ok_or_else(|| AppError::Internal("job enqueue inserted nothing".to_string()))?;
    Ok(Json(StartReconciliationResponse { job_id }))
}

/// Close sync_events rows left 'running' past a staleness threshold. A sync service killed
/// mid-cycle (SIGKILL, node loss) can never terminate its own event; without this sweep the
/// row reads as "sync in progress" forever.
pub struct SyncEventSweep {
    interval_seconds: u64,
    stale_after_seconds: u64,
}

impl SyncEventSweep {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.sync_event_sweep_interval_seconds,
            stale_after_seconds: config.sync_event_stale_after_seconds,
        }
    }
}

#[async_trait]
impl Job for SyncEventSweep {
    fn name(&self) -> &'static str {
        "sync_event_sweep"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(
            Ord::max(self.interval_seconds, 1) as i64
        ))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let closed = sweep_stale_sync_events(ctx.db(), self.stale_after_seconds).await?;
        ctx.report(
            JobReport::new()
                .scope("stale_after_seconds", self.stale_after_seconds)
                .count("sync_events_closed", closed),
        )
        .await;
        Ok(closed as i64)
    }
}

/// Age-based retention for the sync ledgers. sync_events accretes one row per cycle and
/// ingest_receipts one per windowed pass; without pruning both grow forever. Running
/// sync_events rows are never touched (the staleness sweep owns those). A receipt is the record
/// of how a stored value arrived, so age alone does not release one: a receipt whose window still
/// covers a stored reading is kept whatever its age, and only receipts nothing resolves to are
/// pruned.
pub struct SyncLedgerRetention {
    sync_event_retention_days: u32,
    ingest_receipt_retention_days: u32,
}

impl SyncLedgerRetention {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let retention = crate::common::retention::Retention::from_config(config);
        Self {
            sync_event_retention_days: retention.sync_events.horizon_days().unwrap_or(0),
            ingest_receipt_retention_days: retention.ingest_receipts.horizon_days().unwrap_or(0),
        }
    }
}

#[async_trait]
impl Job for SyncLedgerRetention {
    fn name(&self) -> &'static str {
        "sync_ledger_retention"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(86_400))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let db = ctx.db();
        let mut events_pruned = 0u64;
        if self.sync_event_retention_days > 0 {
            events_pruned = events::Entity::delete_many()
                .filter(events::Column::Status.ne("running"))
                .filter(events::Column::StartedAt.lt(days_ago(self.sync_event_retention_days)))
                .exec(db)
                .await?
                .rows_affected;
        }
        let mut receipts_pruned = 0u64;
        if self.ingest_receipt_retention_days > 0 {
            // The readings the pass wrote are what a receipt explains, so one whose window still
            // holds rows is kept however old it is.
            use sea_orm::sea_query::ExprTrait as _;
            let explains_a_stored_reading = SeaQuery::select()
                .expr(Expr::value(1))
                .from_as(readings::Entity, Alias::new("r"))
                .and_where(
                    Expr::col((Alias::new("r"), readings::Column::StreamId))
                        .equals((receipts::Entity, receipts::Column::StreamId)),
                )
                .and_where(
                    Expr::col((Alias::new("r"), readings::Column::Time))
                        .gte(Expr::col((receipts::Entity, receipts::Column::WindowFrom))),
                )
                .and_where(
                    Expr::col((Alias::new("r"), readings::Column::Time))
                        .lt(Expr::col((receipts::Entity, receipts::Column::WindowTo))),
                )
                .take();
            receipts_pruned = receipts::Entity::delete_many()
                .filter(receipts::Column::At.lt(days_ago(self.ingest_receipt_retention_days)))
                .filter(Expr::exists(explains_a_stored_reading).not())
                .exec(db)
                .await?
                .rows_affected;
        }
        ctx.report(
            JobReport::new()
                .scope("sync_event_retention_days", self.sync_event_retention_days)
                .scope(
                    "ingest_receipt_retention_days",
                    self.ingest_receipt_retention_days,
                )
                .count("sync_events_pruned", events_pruned)
                .count("ingest_receipts_pruned", receipts_pruned),
        )
        .await;
        Ok((events_pruned + receipts_pruned) as i64)
    }
}

/// Queue a trigger_full_sync for every live, unpaused service with `full_reassert_enabled`. The
/// digest handshake stops a service re-sending unchanged content, which also means routine passes
/// can no longer repair server-side drift (rows changed outside the sync path); the full pass
/// ignores digests and re-asserts everything the source holds.
///
/// What that repairs depends on the source. A reconciled backend declares the window it
/// re-asserts, so its diff applies the corrections. An append-only one (Vaisala, NOMIS) sends no
/// window and the driver ingests with `overwrite` false, so the pass inserts rows missing here and
/// leaves every stored value as it is; correcting those is `resync_streams`, not this.
///
/// Delivery is the normal heartbeat pickup; a service already holding a pending command is not
/// queued twice.
pub struct SyncFullReassert {
    command_expiry_secs: u64,
}

impl SyncFullReassert {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            command_expiry_secs: config.sync_command_expiry_secs,
        }
    }
}

#[async_trait]
impl Job for SyncFullReassert {
    fn name(&self) -> &'static str {
        "sync_full_reassert"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(604_800))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let rows = ctx
            .db()
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO sync_commands
                     (id, service_id, command, status, created_at, expires_at)
                 SELECT gen_random_uuid(), s.id, 'trigger_full_sync', 'pending', NOW(),
                        NOW() + ($1 || ' seconds')::interval
                 FROM sync_services s
                 WHERE s.paused IS NOT TRUE
                   AND s.full_reassert_enabled
                   AND s.last_heartbeat > NOW() - INTERVAL '1 hour'
                   AND NOT EXISTS (
                       SELECT 1 FROM sync_commands c
                       WHERE c.service_id = s.id
                         AND c.command = 'trigger_full_sync'
                         AND c.status = 'pending'
                         AND c.expires_at > NOW()
                   )
                 RETURNING service_id",
                [self.command_expiry_secs.to_string().into()],
            ))
            .await?;
        let services: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<Uuid>("", "service_id"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        let queued = services.len();
        ctx.report(
            JobReport::new()
                .scope("service_ids", services)
                .count("commands_queued", queued),
        )
        .await;
        Ok(i64::try_from(queued).unwrap_or(i64::MAX))
    }
}

// --- Replicate reconciliation ---
//
// Migration of wrong-shape portal streams onto their replicate-family streams.
//
// History: each portal `_avg` column was synced as its own stream and paired to a slot; the
// replicate columns never left the portal. The family streams (source_key `<old_key>:reps`)
// arrive unpaired and are backfilled by the sync service while invisible. The
// `replicate_reconciliation` job then, per family: verifies the family's would-be-served values
// against the old avg readings, pairs the family stream to the old stream's slot, materialises
// samples, and re-verifies the trigger-computed statistics, all in one transaction, so a
// failing verification rolls the family back to exactly the prior state. Nothing is deleted.
//
// Deletion is its own job (`replicate_reconciliation_delete`), which re-verifies each family and
// only then removes the obsolete avg stream and its readings. The destructive step is therefore
// always behind two verifications and an explicit second operator action.

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
        .add_group_by([Expr::col((r, readings::Column::Time))])
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
            sea_orm::Condition::all().add(
                crate::routes::private::collection_events::flows::row(
                    crate::routes::private::readings::models::Column::StreamId,
                )
                .eq(pair.new_id),
            ),
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

#[cfg(test)]
#[path = "tests/flows.rs"]
mod tests;
