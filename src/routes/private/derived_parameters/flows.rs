use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::sea_query;
use sea_orm::sea_query::{
    Alias, Expr, Func, JoinType, Order, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, ExprTrait,
    FromQueryResult, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Statement,
};
use uuid::Uuid;

use crate::common::sync_state;
use crate::routes::private::derived_parameters::service::DerivedPass;
use crate::routes::private::readings;
use crate::routes::private::reprocessing_jobs::flows::{
    as_db_err, build, optional_uuid, required_uuid,
};
use crate::routes::private::reprocessing_jobs::models::job as jobs;
use crate::routes::private::reprocessing_jobs::service::Job;
use crate::routes::private::reprocessing_jobs::service::{JobContext, JobReport};
use crate::routes::private::sensor_calibrations::service::recalculate_derived_at_timestamp;
use crate::routes::private::site_parameters::models as site_parameters;

use super::models::{definition, source};

const MAX_GAPS_PER_RUN: usize = 50_000;

/// How many instants a report lists per calculation and site; the count covers them all.
const FILL_INSTANTS_LISTED: usize = 100;

/// The statuses a prune leaves alone: a job still queued or running is not history yet.
const IN_FLIGHT: [&str; 4] = ["queued", "pending", "running", "retrying"];

/// Whether a site has any active derived `site_parameter` on the stream arm. The spawn-guard for
/// the ingest/batch derived-compute jobs: when false, a derived recompute at that site would do
/// nothing, so the job is skipped entirely (the dominant source of empty `ingest_derived` jobs).
/// Anything genuinely needed is still caught by the periodic janitor gap scan.
pub async fn site_has_active_derived<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
) -> Result<bool, sea_orm::DbErr> {
    let row = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .filter(site_parameters::Column::EntryMode.eq("tool"))
        .filter(site_parameters::Column::Cadence.eq("high"))
        .filter(
            sea_orm::Condition::any()
                .add(site_parameters::Column::IsActive.eq(true))
                .add(site_parameters::Column::IsActive.is_null()),
        )
        .select_only()
        .column(site_parameters::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await?;
    Ok(row.is_some())
}

/// The gap scan, as the statement it emits.
///
/// `since` bounds the readings side by time. Unbounded, the anti-join hashes the whole hypertable:
/// on the production shape that is a parallel hash whose 16MB doubling step does not fit the DB
/// pod's 64MB `/dev/shm` when another parallel query holds shared memory, so the run aborts and no
/// gap is filled that hour. Bounded, it is an index-range probe over the last few hours; the
/// periodic unbounded run is what still covers drift older than the window.
fn gap_scan(since: Option<chrono::DateTime<chrono::Utc>>) -> SelectStatement {
    let (r, sp, d, f, dps, r2) = (
        Alias::new("r"),
        Alias::new("sp"),
        Alias::new("d"),
        Alias::new("f"),
        Alias::new("dps"),
        Alias::new("r2"),
    );

    let derived_written = SeaQuery::select()
        .expr(Expr::val(1))
        .from_as(readings::Entity, r2.clone())
        .and_where(
            Expr::col((r2.clone(), readings::Column::SiteId))
                .equals((r.clone(), readings::Column::SiteId)),
        )
        .and_where(
            Expr::col((r2.clone(), readings::Column::ParameterId))
                .equals((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .and_where(
            Expr::col((r2, readings::Column::Time)).equals((r.clone(), readings::Column::Time)),
        )
        .take();

    let mut query = SeaQuery::select();
    query
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::Time))
        .column((d.clone(), definition::Column::ToolScriptId))
        // When the newest source value arrived, and when the output slot was declared: a gap
        // whose inputs all predate the slot is its backfill, not a write that missed its recompute.
        .expr_as(
            Func::max(Expr::col((r.clone(), readings::Column::IngestedAt))),
            Alias::new("input_written"),
        )
        .expr_as(
            Func::max(Expr::col((sp.clone(), site_parameters::Column::CreatedAt))),
            Alias::new("slot_declared"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::Join,
            site_parameters::Entity,
            sp.clone(),
            Expr::col((sp.clone(), site_parameters::Column::SiteId))
                .equals((r.clone(), readings::Column::SiteId))
                .and(Expr::col((sp.clone(), site_parameters::Column::EntryMode)).eq("tool"))
                .and(Expr::col((sp.clone(), site_parameters::Column::Cadence)).eq("high"))
                .and(
                    Expr::expr(Func::coalesce([
                        Expr::col((sp.clone(), site_parameters::Column::IsActive)),
                        Expr::val(true),
                    ]))
                    .eq(true),
                ),
        )
        .join_as(
            JoinType::Join,
            definition::Entity,
            d.clone(),
            Expr::col((d.clone(), definition::Column::OutputParameterId))
                .equals((sp.clone(), site_parameters::Column::ParameterId))
                .and(Expr::col((d.clone(), definition::Column::ToolScriptId)).is_not_null()),
        )
        // Any formula of the same calculation: a source only a step reads still makes the instant
        // one the set computes at, because the step feeds the output the slot holds.
        .join_as(
            JoinType::Join,
            definition::Entity,
            f.clone(),
            Expr::col((f.clone(), definition::Column::ToolScriptId))
                .equals((d.clone(), definition::Column::ToolScriptId)),
        )
        .join_as(
            JoinType::Join,
            source::Entity,
            dps.clone(),
            Expr::col((dps.clone(), source::Column::DerivedDefinitionId))
                .equals((f, definition::Column::Id))
                .and(
                    Expr::col((dps.clone(), source::Column::ParameterId))
                        .equals((r.clone(), readings::Column::ParameterId)),
                ),
        )
        .and_where(Expr::exists(derived_written).not())
        .group_by_col((r.clone(), readings::Column::SiteId))
        .group_by_col((r.clone(), readings::Column::Time))
        .group_by_col((d.clone(), definition::Column::ToolScriptId))
        .order_by((r.clone(), readings::Column::SiteId), Order::Asc)
        .order_by((r.clone(), readings::Column::Time), Order::Asc)
        .limit(MAX_GAPS_PER_RUN as u64);
    if let Some(from) = since {
        query.and_where(
            Expr::col((r, readings::Column::Time))
                .gte(sea_orm::prelude::DateTimeWithTimeZone::from(from)),
        );
    }
    query
}

/// Find (site_id, time) pairs where a source reading exists but no corresponding
/// derived reading was ever written. Recompute each.
///
/// Catches gaps from crashes during ingest, sync cycles missed while the API
/// was down, derived parameters assigned after historical source data already
/// existed, and any other inconsistency between source and derived readings.
///
/// After filling any gaps, refreshes continuous aggregates back to the earliest filled timestamp so
/// the hourly/daily/weekly/monthly rollups reflect the newly written derived values.
///
/// Reports progress into the caller's job rather than opening one of its own: the janitor always
/// runs as a step of the worker-pool `janitor_service` job, and a second row opened from inside that
/// job would carry no lease, so nothing could ever reclaim it. What the pass counted is returned for
/// the caller's one report, which replaces the job's detail whole.
/// One instant of a derived slot the sweep found stale.
#[derive(FromQueryResult)]
struct StaleSlot {
    site_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
    tool_script_id: Uuid,
    input_written: Option<chrono::DateTime<chrono::FixedOffset>>,
    slot_declared: Option<chrono::DateTime<chrono::FixedOffset>>,
}

/// One calculation's gap at one site and instant, and whether it is the slot's backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gap {
    pub tool_script_id: Uuid,
    pub site_id: Uuid,
    pub time: chrono::DateTime<chrono::Utc>,
    pub backfill: bool,
}

/// Whether a gap is the backfill of a slot declared over values already stored: every input
/// arrived before the slot was. A value that predates arrival tracking counts as stored; a slot
/// that predates it cannot vouch, so its fill is shown as missed.
fn is_backfill(
    input_written: Option<chrono::DateTime<chrono::Utc>>,
    slot_declared: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    match (input_written, slot_declared) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(input), Some(slot)) => input < slot,
    }
}

/// What one calculation had filled at one site: the values a missed recompute left, with the
/// first instants of them, and apart from them the slot's backfill.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SiteFills {
    pub values: usize,
    pub backfilled: usize,
    pub instants: Vec<chrono::DateTime<chrono::Utc>>,
}

/// Each calculation's filled gaps by site, from the scan's gaps and the `(site, instant)` pairs
/// the recompute filled.
fn fills_by_calculation(
    gaps: &[Gap],
    filled: &HashSet<(Uuid, chrono::DateTime<chrono::Utc>)>,
) -> BTreeMap<Uuid, BTreeMap<Uuid, SiteFills>> {
    let mut fills: BTreeMap<Uuid, BTreeMap<Uuid, SiteFills>> = BTreeMap::new();
    for gap in gaps {
        if !filled.contains(&(gap.site_id, gap.time)) {
            continue;
        }
        let at = fills
            .entry(gap.tool_script_id)
            .or_default()
            .entry(gap.site_id)
            .or_default();
        if gap.backfill {
            at.backfilled += 1;
            continue;
        }
        at.values += 1;
        if at.instants.len() < FILL_INSTANTS_LISTED {
            at.instants.push(gap.time);
        }
    }
    fills
}

/// What one gap-fill pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GapFill {
    pub found: usize,
    pub filled: usize,
    pub refused_slots: usize,
    pub earliest_filled: Option<chrono::DateTime<chrono::Utc>>,
    /// The scan returned `MAX_GAPS_PER_RUN` rows, so gaps may remain past what this pass saw.
    pub capped: bool,
    /// What each calculation, by `tool_script_id`, had filled at each site.
    pub by_calculation: BTreeMap<Uuid, BTreeMap<Uuid, SiteFills>>,
}

impl GapFill {
    /// The pass's counts and scope added to the report of the job it ran in.
    #[must_use]
    pub fn report_into(&self, report: JobReport) -> JobReport {
        report
            .scope_opt(
                "earliest_filled",
                self.earliest_filled.map(|t| t.to_rfc3339()),
            )
            .scope("capped_at_limit", self.capped)
            .scope_opt(
                "filled_by_calculation",
                (!self.by_calculation.is_empty())
                    .then(|| serde_json::to_value(&self.by_calculation).unwrap_or_default()),
            )
            .count("gaps_found", self.found)
            .count("filled", self.filled)
            .count("refused_slots", self.refused_slots)
    }
}

pub async fn run_once(
    db: &DatabaseConnection,
    ctx: Option<&JobContext>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<GapFill, sea_orm::DbErr> {
    let started = std::time::Instant::now();

    let (sql, values) = gap_scan(since).build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    // A row per calculation with a gap at the instant; the recompute covers every calculation at
    // the site, so it runs once per instant.
    let mut gaps = Vec::with_capacity(rows.len());
    for row in &rows {
        let slot = StaleSlot::from_query_result(row, "")?;
        let utc = |t: chrono::DateTime<chrono::FixedOffset>| t.with_timezone(&chrono::Utc);
        gaps.push(Gap {
            tool_script_id: slot.tool_script_id,
            site_id: slot.site_id,
            time: utc(slot.time),
            backfill: is_backfill(slot.input_written.map(utc), slot.slot_declared.map(utc)),
        });
    }
    let mut instants: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> =
        gaps.iter().map(|g| (g.site_id, g.time)).collect();
    instants.dedup();

    let total = i32::try_from(instants.len()).unwrap_or(i32::MAX);
    if let Some(ctx) = ctx {
        ctx.set_progress(0, Some(total)).await;
    }
    if instants.is_empty() {
        tracing::info!("Derived janitor: no gaps found");
        return Ok(GapFill::default());
    }
    tracing::info!(gaps = total, "Derived janitor: filling gaps");

    let mut filled_slots = HashSet::new();
    let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut refused = DerivedPass::default();
    for (i, &(site_id, utc_time)) in instants.iter().enumerate() {
        if ctx.is_some_and(JobContext::is_cancelled) {
            break;
        }
        match crate::routes::private::sensor_calibrations::service::recalculate_derived_at_timestamp(
            db, site_id, utc_time,
        )
        .await
        {
            Ok(slots) => {
                refused.record(&slots, utc_time);
                filled_slots.insert((site_id, utc_time));
                min_filled = Some(min_filled.map_or(utc_time, |m| Ord::min(m, utc_time)));
            }
            Err(e) => tracing::warn!(error = %e, site_id = %site_id, time = %utc_time, "Janitor failed to fill derived gap"),
        }
        if (i + 1) % 1000 == 0
            && let Some(ctx) = ctx
        {
            ctx.set_progress(i as i32 + 1, Some(total)).await;
        }
    }

    if let Some(since) = min_filled {
        tracing::info!(%since, filled = filled_slots.len(), "Derived janitor: refreshing continuous aggregates after backfill");
        crate::common::sync_state::refresh_continuous_aggregates(db, since)
            .await
            .map_err(|e| sea_orm::DbErr::Custom(e.to_string()))?;
    }
    let refused_slots = refused.report(db).await?;
    let filled = filled_slots.len();
    if let Some(ctx) = ctx {
        ctx.set_progress(total, Some(total)).await;
        ctx.info(&format!("Filled {filled} of {total} derived gaps"))
            .await;
    }
    tracing::info!(
        filled,
        total,
        capped_at_limit = total as usize >= MAX_GAPS_PER_RUN,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Derived janitor: gap fill complete"
    );
    Ok(GapFill {
        found: instants.len(),
        filled,
        refused_slots,
        earliest_filled: min_filled,
        capped: rows.len() >= MAX_GAPS_PER_RUN,
        by_calculation: fills_by_calculation(&gaps, &filled_slots),
    })
}

/// Tiered retention for `reprocessing_jobs` (logs cascade-delete with their job). Three layers:
///   1. `maintenance` rows (high-volume janitor/ingest/refresh/alarm-backfill) age out fast.
///   2. `operator`/`metadata` rows (audit value) age out slowly.
///   3. A hard count cap on `maintenance` rows so an ingestion burst can't blow storage between the
///      daily prunes. Each window/cap of 0 disables that layer.
///
/// Returns the rows deleted across all layers and, when any layer failed, which and why. Every
/// layer is attempted whatever the others did.
pub async fn prune_tracked_jobs(
    db: &DatabaseConnection,
    maintenance_days: u32,
    operator_days: u32,
    maintenance_max_rows: u64,
) -> Pruned {
    let mut pruned = Pruned::default();

    if maintenance_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(maintenance_days));
        pruned.add(
            run_delete(
                jobs::Entity::delete_many()
                    .filter(jobs::Column::Category.eq("maintenance"))
                    .filter(jobs::Column::CreatedAt.lt(cutoff))
                    .filter(jobs::Column::Status.is_not_in(IN_FLIGHT)),
                db,
                "maintenance age",
            )
            .await,
        );
    }
    if operator_days > 0 {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(operator_days));
        pruned.add(
            run_delete(
                jobs::Entity::delete_many()
                    .filter(jobs::Column::Category.is_in(["operator", "metadata"]))
                    .filter(jobs::Column::CreatedAt.lt(cutoff))
                    .filter(jobs::Column::Status.is_not_in(IN_FLIGHT)),
                db,
                "operator/metadata age",
            )
            .await,
        );
    }
    if maintenance_max_rows > 0 {
        // Keep the most-recent N maintenance rows; delete the older overflow.
        let overflow = jobs::Entity::find()
            .select_only()
            .column(jobs::Column::Id)
            .filter(jobs::Column::Category.eq("maintenance"))
            .filter(jobs::Column::Status.is_not_in(IN_FLIGHT))
            .order_by_desc(jobs::Column::CreatedAt)
            .offset(maintenance_max_rows)
            .into_query();
        pruned.add(
            run_delete(
                jobs::Entity::delete_many().filter(jobs::Column::Id.in_subquery(overflow)),
                db,
                "maintenance count cap",
            )
            .await,
        );
    }

    if pruned.deleted > 0 {
        tracing::info!(
            deleted = pruned.deleted,
            "Tracked-job retention: pruned old job rows"
        );
    }
    pruned
}

/// What a retention pass deleted, and each layer that failed with its error.
#[derive(Debug, Default)]
pub struct Pruned {
    pub deleted: u64,
    pub failed: Vec<String>,
}

impl Pruned {
    fn add(&mut self, layer: Result<u64, String>) {
        match layer {
            Ok(n) => self.deleted += n,
            Err(e) => self.failed.push(e),
        }
    }

    /// `Ok` when every layer ran, otherwise every failed layer and its error.
    pub fn result(&self) -> Result<(), String> {
        if self.failed.is_empty() {
            Ok(())
        } else {
            Err(self.failed.join("; "))
        }
    }
}

async fn run_delete(
    delete: sea_orm::DeleteMany<jobs::Entity>,
    db: &DatabaseConnection,
    label: &str,
) -> Result<u64, String> {
    match delete.exec(db).await {
        Ok(res) => Ok(res.rows_affected),
        Err(e) => {
            tracing::warn!(error = %e, label, "Tracked-job retention: prune layer failed");
            Err(format!("{label}: {e}"))
        }
    }
}

/// One instant of one site's derived work.
#[derive(FromQueryResult)]
struct DerivedInstant {
    site_id: Uuid,
    time: chrono::DateTime<chrono::FixedOffset>,
}
/// One instant, where the site is already known.
#[derive(FromQueryResult)]
struct InstantRow {
    time: chrono::DateTime<chrono::FixedOffset>,
}
/// Parse a `params` array of RFC 3339 strings into UTC timestamps (skipping unparseable entries).
fn parse_timestamps(value: Option<&serde_json::Value>) -> Vec<chrono::DateTime<chrono::Utc>> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|t| t.with_timezone(&chrono::Utc))
                })
                .collect()
        })
        .unwrap_or_default()
}
/// Recompute derived values from their source readings, then refresh continuous aggregates. Backs
/// the `derived_recompute` trigger, in either of two scopes: one derived parameter definition over
/// its whole history (`calculation_id`), or every calculation reading a given slot over a
/// window (`site_ids`, `parameter_ids`, `start`, `end`), which is what a curation decision leaves
/// behind.
pub struct DerivedRecompute;

/// The `(site, time)` instants a derived recompute covers: every reading of a parameter some
/// calculation reads, at a site whose slot for one of that calculation's outputs is tool-entered
/// on the stream arm. A low-cadence slot is the chain's, computed at the visit that recorded it.
///
/// A source only a step reads counts: the step feeds the output the slot holds, so the instant is
/// one the set computes at.
fn derived_instants(
    scope: sea_query::Condition,
    join_calculation_on: Option<Uuid>,
) -> sea_query::SelectStatement {
    let r = sea_query::Alias::new("r");
    let f = sea_query::Alias::new("f");
    let o = sea_query::Alias::new("o");
    let dps = sea_query::Alias::new("dps");
    let sp = sea_query::Alias::new("sp");

    let mut query = sea_query::Query::select();
    query
        .distinct()
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone());

    match join_calculation_on {
        // One calculation: its formulas are the given rows, and the sources join to them.
        Some(calculation_id) => {
            query
                .join_as(
                    sea_query::JoinType::Join,
                    definition::Entity,
                    f.clone(),
                    sea_query::Expr::col((f.clone(), definition::Column::ToolScriptId))
                        .eq(calculation_id),
                )
                .join_as(
                    sea_query::JoinType::Join,
                    source::Entity,
                    dps.clone(),
                    sea_query::Condition::all()
                        .add(
                            sea_query::Expr::col((
                                dps.clone(),
                                source::Column::DerivedDefinitionId,
                            ))
                            .equals((f.clone(), definition::Column::Id)),
                        )
                        .add(
                            sea_query::Expr::col((dps.clone(), source::Column::ParameterId))
                                .equals((r.clone(), readings::Column::ParameterId)),
                        ),
                );
        }
        // Every formula that reads the parameter this reading carries.
        None => {
            query
                .join_as(
                    sea_query::JoinType::Join,
                    source::Entity,
                    dps.clone(),
                    sea_query::Condition::all().add(
                        sea_query::Expr::col((dps.clone(), source::Column::ParameterId))
                            .equals((r.clone(), readings::Column::ParameterId)),
                    ),
                )
                .join_as(
                    sea_query::JoinType::Join,
                    definition::Entity,
                    f.clone(),
                    sea_query::Condition::all()
                        .add(
                            sea_query::Expr::col((f.clone(), definition::Column::Id))
                                .equals((dps.clone(), source::Column::DerivedDefinitionId)),
                        )
                        .add(
                            sea_query::Expr::col((f.clone(), definition::Column::ToolScriptId))
                                .is_not_null(),
                        ),
                );
        }
    }

    query
        .join_as(
            sea_query::JoinType::Join,
            definition::Entity,
            o.clone(),
            sea_query::Condition::all()
                .add(
                    sea_query::Expr::col((o.clone(), definition::Column::ToolScriptId))
                        .equals((f, definition::Column::ToolScriptId)),
                )
                .add(
                    sea_query::Expr::col((o.clone(), definition::Column::OutputParameterId))
                        .is_not_null(),
                ),
        )
        .join_as(
            sea_query::JoinType::Join,
            site_parameters::Entity,
            sp.clone(),
            sea_query::Condition::all()
                .add(
                    sea_query::Expr::col((sp.clone(), site_parameters::Column::SiteId))
                        .equals((r.clone(), readings::Column::SiteId)),
                )
                .add(
                    sea_query::Expr::col((sp.clone(), site_parameters::Column::EntryMode))
                        .eq("tool"),
                )
                .add(
                    sea_query::Expr::col((sp.clone(), site_parameters::Column::Cadence)).eq("high"),
                )
                .add(
                    sea_query::Expr::col((sp, site_parameters::Column::ParameterId))
                        .equals((o, definition::Column::OutputParameterId)),
                ),
        )
        .cond_where(scope)
        .order_by((r.clone(), readings::Column::SiteId), sea_query::Order::Asc)
        .order_by((r, readings::Column::Time), sea_query::Order::Asc)
        .to_owned()
}

/// The instants at one site where a calculation's inputs were recorded. Every formula of the set
/// counts, a step included: a step's source is an input the set reads.
fn instants_a_calculation_reads(calculation_id: Uuid, site_id: Uuid) -> sea_query::SelectStatement {
    let r = sea_query::Alias::new("r");
    let f = sea_query::Alias::new("f");
    let dps = sea_query::Alias::new("dps");
    sea_query::Query::select()
        .distinct()
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone())
        .join_as(
            sea_query::JoinType::Join,
            source::Entity,
            dps.clone(),
            sea_query::Condition::all().add(
                sea_query::Expr::col((dps.clone(), source::Column::ParameterId))
                    .equals((r.clone(), readings::Column::ParameterId)),
            ),
        )
        .join_as(
            sea_query::JoinType::Join,
            definition::Entity,
            f.clone(),
            sea_query::Expr::col((f.clone(), definition::Column::Id))
                .equals((dps, source::Column::DerivedDefinitionId)),
        )
        .and_where(sea_query::Expr::col((f, definition::Column::ToolScriptId)).eq(calculation_id))
        .and_where(sea_query::Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
        .order_by((r, readings::Column::Time), sea_query::Order::Asc)
        .to_owned()
}

/// The `(site, time)` instants a `derived_recompute` run must recompute, in either scope. A window
/// is two statements: the instants its parameters are read at, and the pulses holding them.
fn derived_recompute_instants(params: &serde_json::Value) -> Result<Vec<Statement>, DbErr> {
    if params.get("calculation_id").is_some() {
        let calculation_id = required_uuid(params, "calculation_id")?;
        return Ok(vec![build(&derived_instants(
            sea_query::Condition::all(),
            Some(calculation_id),
        ))]);
    }

    let uuids = |key: &str| -> Result<Vec<Uuid>, DbErr> {
        params
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                    .collect()
            })
            .ok_or_else(|| DbErr::Custom(format!("derived_recompute: missing {key}")))
    };
    let time = |key: &str| -> Result<chrono::DateTime<chrono::Utc>, DbErr> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .ok_or_else(|| DbErr::Custom(format!("derived_recompute: missing {key}")))
    };

    let (site_ids, parameter_ids) = (uuids("site_ids")?, uuids("parameter_ids")?);
    let (start, end) = (time("start")?, time("end")?);
    let r = sea_query::Alias::new("r");
    let window = sea_query::Condition::all()
        .add(sea_query::Expr::col((r.clone(), readings::Column::SiteId)).is_in(site_ids.clone()))
        .add(
            sea_query::Expr::col((r.clone(), readings::Column::ParameterId))
                .is_in(parameter_ids.clone()),
        )
        .add(sea_query::Expr::col((r.clone(), readings::Column::Time)).gte(start))
        .add(sea_query::Expr::col((r, readings::Column::Time)).lte(end));
    Ok(vec![build(&derived_instants(window, None))])
}

#[async_trait]
impl Job for DerivedRecompute {
    fn name(&self) -> &'static str {
        "derived_recompute"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let statements = derived_recompute_instants(ctx.params())?;
        let work = async {
            tracing::info!(job_id = %ctx.job_id(), "Recomputing derived parameters");
            let mut instants = std::collections::BTreeSet::new();
            for statement in statements {
                for row in ctx.db().query_all_raw(statement).await? {
                    let instant = DerivedInstant::from_query_result(&row, "")?;
                    instants.insert((instant.time.with_timezone(&chrono::Utc), instant.site_id));
                }
            }
            let rows: Vec<(chrono::DateTime<chrono::Utc>, Uuid)> = instants.into_iter().collect();

            let total = i32::try_from(rows.len()).unwrap_or(i32::MAX);
            ctx.set_progress(0, Some(total)).await;

            let mut filled: i32 = 0;
            let mut min_filled: Option<chrono::DateTime<chrono::Utc>> = None;
            let mut filled_sites: std::collections::BTreeSet<Uuid> =
                std::collections::BTreeSet::new();
            let mut refused = DerivedPass::default();
            for (i, row) in rows.iter().enumerate() {
                if ctx.is_cancelled() {
                    break;
                }
                let (utc_time, site_id) = *row;
                match recalculate_derived_at_timestamp(ctx.db(), site_id, utc_time).await {
                    Ok(slots) => {
                        refused.record(&slots, utc_time);
                        filled += 1;
                        min_filled = Some(min_filled.map_or(utc_time, |m| Ord::min(m, utc_time)));
                        filled_sites.insert(site_id);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, time = %utc_time, "Failed to recompute derived value")
                    }
                }
                if (i + 1) % 500 == 0 {
                    ctx.set_progress(i as i32 + 1, Some(total)).await;
                }
            }

            if let Some(since) = min_filled {
                tracing::info!(%since, "Refreshing continuous aggregates after derived recompute");
                sync_state::refresh_continuous_aggregates(ctx.db(), since)
                    .await
                    .map_err(as_db_err)?;
                for site_id in filled_sites {
                    announce_derived_write(&ctx, site_id, filled);
                }
            }
            ctx.set_progress(total, Some(total)).await;
            let refused_slots = refused.report(ctx.db()).await?;
            ctx.report(
                JobReport::new()
                    .scope_opt(
                        "calculation_id",
                        ctx.params()
                            .get("calculation_id")
                            .and_then(|v| v.as_str().map(str::to_string)),
                    )
                    .scope_opt("earliest_filled", min_filled.map(|t| t.to_rfc3339()))
                    .count("timestamps", total)
                    .count("filled", filled)
                    .count("refused_slots", refused_slots),
            )
            .await;
            tracing::info!(total, filled, "Derived parameter recomputation complete");
            Ok::<i64, DbErr>(i64::from(filled))
        };

        match tokio::time::timeout(Duration::from_secs(600), work).await {
            Ok(res) => res,
            Err(_) => Err(DbErr::Custom("Timed out after 10 minutes".to_string())),
        }
    }
}
/// Backfill derived values for the readings already present at a site when a derived
/// `site_parameter` is assigned, then refresh continuous aggregates. Backs the `derived_assignment`
/// trigger. Reads `calculation_id` and `site_id` from params.
pub struct DerivedAssignment;

#[async_trait]
impl Job for DerivedAssignment {
    fn name(&self) -> &'static str {
        "derived_assignment"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let calculation_id = required_uuid(ctx.params(), "calculation_id")?;
        let site_id = required_uuid(ctx.params(), "site_id")?;
        tracing::info!(%calculation_id, %site_id, "Computing derived values after site assignment");
        ctx.set_site(site_id).await;

        let rows = ctx
            .db()
            .query_all_raw(build(&instants_a_calculation_reads(
                calculation_id,
                site_id,
            )))
            .await?;

        let mut filled = 0i64;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut refused = DerivedPass::default();
        for row in &rows {
            if ctx.is_cancelled() {
                break;
            }
            let utc = InstantRow::from_query_result(row, "")?
                .time
                .with_timezone(&chrono::Utc);
            if let Ok(slots) = recalculate_derived_at_timestamp(ctx.db(), site_id, utc).await {
                refused.record(&slots, utc);
                filled += 1;
                earliest = Some(earliest.map_or(utc, |e| Ord::min(e, utc)));
            }
        }

        if let Some(since) = earliest {
            sync_state::refresh_continuous_aggregates(ctx.db(), since)
                .await
                .map_err(as_db_err)?;
            announce_derived_write(&ctx, site_id, i32::try_from(filled).unwrap_or(i32::MAX));
        }

        let refused_slots = refused.report(ctx.db()).await?;
        ctx.report(
            JobReport::new()
                .scope("calculation_id", calculation_id.to_string())
                .scope("site_id", site_id.to_string())
                .scope_opt("earliest_filled", earliest.map(|t| t.to_rfc3339()))
                .count("timestamps", rows.len())
                .count("filled", filled)
                .count("refused_slots", refused_slots),
        )
        .await;
        tracing::info!(%calculation_id, %site_id, filled, "Derived assignment backfill completed");
        Ok(filled)
    }
}
/// Compute and upsert derived parameter values for an explicit list of `(site, timestamps)` pairs,
/// then refresh continuous aggregates from the earliest timestamp. Backs `compute_derived` (the
/// operator action) and `batch_derived` (auto-compute after a batch insert). Reads `site_timestamps`
/// (array of `{ site_id, timestamps[] }`) from params.
pub struct SiteTimestampsDerived {
    name: &'static str,
}

impl SiteTimestampsDerived {
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}
#[async_trait]
impl Job for SiteTimestampsDerived {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let groups = ctx
            .params()
            .get("site_timestamps")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut work: Vec<(Uuid, Vec<chrono::DateTime<chrono::Utc>>)> = Vec::new();
        for group in &groups {
            let Some(site_id) = optional_uuid(group, "site_id") else {
                continue;
            };
            work.push((site_id, parse_timestamps(group.get("timestamps"))));
        }

        let total =
            i32::try_from(work.iter().map(|(_, ts)| ts.len()).sum::<usize>()).unwrap_or(i32::MAX);
        ctx.set_progress(0, Some(total)).await;

        let mut progress = 0i32;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut refused = DerivedPass::default();
        'outer: for (site_id, timestamps) in &work {
            for time in timestamps {
                if ctx.is_cancelled() {
                    break 'outer;
                }
                match recalculate_derived_at_timestamp(ctx.db(), *site_id, *time).await {
                    Ok(slots) => {
                        refused.record(&slots, *time);
                        earliest = Some(earliest.map_or(*time, |e| Ord::min(e, *time)));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, site_id = %site_id, time = %time, "Failed to compute derived values");
                    }
                }
                progress += 1;
                if progress % 500 == 0 {
                    ctx.set_progress(progress, Some(total)).await;
                }
            }
        }

        if let Some(since) = earliest {
            tracing::info!(%since, "Refreshing continuous aggregates after derived computation");
            sync_state::refresh_continuous_aggregates(ctx.db(), since)
                .await
                .map_err(as_db_err)?;
            for (site_id, timestamps) in &work {
                announce_derived_write(
                    &ctx,
                    *site_id,
                    i32::try_from(timestamps.len()).unwrap_or(i32::MAX),
                );
            }
        }
        ctx.set_progress(progress, Some(total)).await;
        let refused_slots = refused.report(ctx.db()).await?;
        ctx.report(
            JobReport::new()
                .scope("sites", work.len())
                .scope_opt("earliest_computed", earliest.map(|t| t.to_rfc3339()))
                .count("timestamps", total)
                .count("computed", progress)
                .count("refused_slots", refused_slots),
        )
        .await;
        tracing::info!(computed = progress, "Derived computation complete");
        Ok(i64::from(progress))
    }
}
/// A derived value is a served value, so a job that writes one announces it: `DataIngested` naming
/// the site is what drops that site's cached responses (`common/cache.rs:17-18`).
fn announce_derived_write(ctx: &JobContext, site_id: Uuid, count: i32) {
    let _ = ctx.events().send(crate::common::AppEvent::DataIngested {
        site_id: Some(site_id),
        parameter_id: None,
        stream_id: None,
        count: usize::try_from(count).unwrap_or(0),
    });
}
/// Auto-compute derived values for one site's newly ingested timestamps. Backs the `ingest_derived`
/// trigger fired after a single-stream ingest. Reads `site_id`, `stream_id`, and `timestamps[]` from
/// params.
pub struct IngestDerived;

#[async_trait]
impl Job for IngestDerived {
    fn name(&self) -> &'static str {
        "ingest_derived"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let site_id = required_uuid(ctx.params(), "site_id")?;
        let stream_id = optional_uuid(ctx.params(), "stream_id");
        let timestamps = parse_timestamps(ctx.params().get("timestamps"));
        let total = i32::try_from(timestamps.len()).unwrap_or(i32::MAX);

        ctx.set_site(site_id).await;
        ctx.report(
            JobReport::new()
                .scope("site_id", site_id.to_string())
                .scope_opt("stream_id", stream_id.map(|id| id.to_string()))
                .count("timestamps", total),
        )
        .await;
        ctx.set_progress(0, Some(total)).await;

        let mut progress = 0i32;
        let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut refused = DerivedPass::default();
        for time in timestamps {
            if ctx.is_cancelled() {
                break;
            }
            match recalculate_derived_at_timestamp(ctx.db(), site_id, time).await {
                Ok(slots) => {
                    refused.record(&slots, time);
                    earliest = Some(
                        earliest.map_or(time, |e: chrono::DateTime<chrono::Utc>| Ord::min(e, time)),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, site_id = %site_id, time = %time, "Failed to auto-compute derived values after ingest");
                }
            }
            progress += 1;
            if progress % 500 == 0 {
                ctx.set_progress(progress, Some(total)).await;
            }
        }

        if let Some(since) = earliest {
            sync_state::refresh_continuous_aggregates(ctx.db(), since)
                .await
                .map_err(as_db_err)?;
            announce_derived_write(&ctx, site_id, progress);
        }
        ctx.set_progress(progress, Some(total)).await;
        refused.report(ctx.db()).await?;
        Ok(i64::from(progress))
    }
}

#[cfg(test)]
#[path = "tests/flows_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/derived_instants.rs"]
mod derived_instants_tests;
