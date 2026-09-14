//! Alarm queries and the threshold resolution they share.
//!
//! The breach definition (which value counts as warning, which as alarm) and the two-tier
//! threshold fallback live here so the live evaluation and the historical episode rebuild cannot
//! drift apart.

use chrono::{DateTime, FixedOffset, Utc};
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::sea_query::{
    Alias, CommonTableExpression, Condition, Expr, Frame, FrameType, Func, JoinType, Order,
    PostgresQueryBuilder, Query as SeaQuery, SelectStatement, UnionType, WindowStatement,
    WithClause, WithQuery,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, ExprTrait, FromQueryResult, Statement,
    TransactionTrait,
};
use std::collections::HashMap;
use uuid::Uuid;

use super::flows::reconcile_all_from_hook;
use super::models::{AlarmThreshold, ResolvedThreshold, alarm_event};
use crate::common::AppState;
use crate::common::scope::{Unowned, project_filter, project_of_alarm_event, require_row_in_scope};
use crate::common::served::{self};
use crate::error::AppResult;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::sensors::models as sensors;
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::sites::models as sites;
/// site if needed.
pub fn severity_case(val: &str, wmin: &str, wmax: &str, amin: &str, amax: &str) -> String {
    format!(
        "CASE \
            WHEN ({amin} IS NOT NULL AND {val} < {amin}) OR ({amax} IS NOT NULL AND {val} > {amax}) THEN 2 \
            WHEN ({wmin} IS NOT NULL AND {val} < {wmin}) OR ({wmax} IS NOT NULL AND {val} > {wmax}) THEN 1 \
            ELSE 0 \
        END"
    )
}

/// Rust mirror of [`severity_case`] for the one path that classifies in Rust (aggregate buckets):
/// a single value → severity (2=alarm, 1=warning, 0=ok). Kept in lock-step with `severity_case` by
/// the alarm-consistency test.
pub fn severity_of(value: f64, t: &ResolvedThreshold) -> i16 {
    if t.alarm_min.is_some_and(|m| value < m) || t.alarm_max.is_some_and(|m| value > m) {
        2
    } else if t.warning_min.is_some_and(|m| value < m) || t.warning_max.is_some_and(|m| value > m) {
        1
    } else {
        0
    }
}

/// Severity of an aggregate bucket: its `min` can breach a lower bound and its `max` an upper bound,
/// so the bucket severity is the worse of the two, expressed via [`severity_of`] so the ladder is
/// defined once.
pub fn severity_of_range(min: Option<f64>, max: Option<f64>, t: &ResolvedThreshold) -> i16 {
    let lo = min.map(|v| severity_of(v, t)).unwrap_or(0);
    let hi = max.map(|v| severity_of(v, t)).unwrap_or(0);
    std::cmp::max(lo, hi)
}

/// A range breach is an alarm, never a warning: the instrument cannot read the value at all, so
/// there is no degree to it.
pub const INSTRUMENT_RANGE_SEVERITY: i16 = 2;

/// The bounds an instrument's manufacturer specifies. An undeclared bound is no bound, and an
/// instrument declaring neither raises nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstrumentRange {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Whether a value falls outside what the instrument that measured it can read.
#[must_use]
pub fn out_of_instrument_range(value: f64, range: &InstrumentRange) -> bool {
    range.min.is_some_and(|m| value < m) || range.max.is_some_and(|m| value > m)
}

/// SQL mirror of [`out_of_instrument_range`], kept in lock-step with it by the alarm-consistency
/// test. Same bound-expression convention as [`severity_case`].
#[must_use]
pub fn instrument_range_condition(val: &str, rmin: &str, rmax: &str) -> String {
    format!("(({rmin} IS NOT NULL AND {val} < {rmin}) OR ({rmax} IS NOT NULL AND {val} > {rmax}))")
}

/// Boolean SQL predicate true when a value breaches at or above `min_severity` (1 includes warnings,
/// 2 restricts to alarms). Same bound-expression convention as [`severity_case`].
pub fn violation_condition(
    val: &str,
    wmin: &str,
    wmax: &str,
    amin: &str,
    amax: &str,
    min_severity: i16,
) -> String {
    let alarm = format!(
        "({amin} IS NOT NULL AND {val} < {amin}) OR ({amax} IS NOT NULL AND {val} > {amax})"
    );
    if min_severity >= 2 {
        format!("({alarm})")
    } else {
        format!(
            "({alarm} OR ({wmin} IS NOT NULL AND {val} < {wmin}) OR ({wmax} IS NOT NULL AND {val} > {wmax}))"
        )
    }
}

fn col(table: &str, c: &str) -> Expr {
    Expr::col((Alias::new(table), Alias::new(c)))
}

/// THE single definition of the two-tier resolution: the site's own `alarm_thresholds` row, else
/// the parameter's global one. A parameter's own bounds are that global row and live nowhere else.
///
/// Built with sea-query so it is composable and dialect-portable. Produces one row per active
/// `(site_id, parameter_id)` slot with the winning bounds + `source`, picking the highest-priority
/// tier per slot via a portable `ROW_NUMBER() OVER (… ORDER BY priority)`. Whole-row semantics are
/// preserved (a site row wins entirely, so an all-NULL site row reads as disabled and blocks the
/// fallback). `site_id`/`param_ids` scope it (all slots when both `None`).
pub fn resolve_thresholds_query(
    site_id: Option<Uuid>,
    param_ids: Option<Vec<Uuid>>,
) -> SelectStatement {
    // Tier 1+2: explicit alarm_thresholds rows (site-specific or global) joined to active slots.
    let mut rows = SeaQuery::select();
    rows.expr_as(col("sp", "site_id"), Alias::new("site_id"))
        .expr_as(col("sp", "parameter_id"), Alias::new("parameter_id"))
        .expr_as(col("t", "warning_min"), Alias::new("warning_min"))
        .expr_as(col("t", "warning_max"), Alias::new("warning_max"))
        .expr_as(col("t", "alarm_min"), Alias::new("alarm_min"))
        .expr_as(col("t", "alarm_max"), Alias::new("alarm_max"))
        .expr_as(
            Expr::case(
                col("t", "site_id").equals((Alias::new("sp"), Alias::new("site_id"))),
                1,
            )
            .finally(2),
            Alias::new("priority"),
        )
        .expr_as(
            Expr::case(
                col("t", "site_id").equals((Alias::new("sp"), Alias::new("site_id"))),
                "site",
            )
            .finally("global"),
            Alias::new("source"),
        )
        .from_as(Alias::new("site_parameters"), Alias::new("sp"))
        .join_as(
            JoinType::Join,
            Alias::new("alarm_thresholds"),
            Alias::new("t"),
            col("t", "parameter_id")
                .equals((Alias::new("sp"), Alias::new("parameter_id")))
                .and(
                    col("t", "site_id")
                        .equals((Alias::new("sp"), Alias::new("site_id")))
                        .or(col("t", "site_id").is_null()),
                ),
        )
        .and_where(col("sp", "is_active").eq(true));

    if let Some(s) = site_id {
        rows.and_where(col("sp", "site_id").eq(s));
    }
    if let Some(pids) = param_ids {
        rows.and_where(col("sp", "parameter_id").is_in(pids));
    }

    // Rank tiers per slot and keep the winner.
    let mut ranked = SeaQuery::select();
    ranked
        .columns([
            Alias::new("site_id"),
            Alias::new("parameter_id"),
            Alias::new("warning_min"),
            Alias::new("warning_max"),
            Alias::new("alarm_min"),
            Alias::new("alarm_max"),
            Alias::new("source"),
        ])
        .expr_as(
            Expr::cust("ROW_NUMBER() OVER (PARTITION BY site_id, parameter_id ORDER BY priority)"),
            Alias::new("rn"),
        )
        .from_subquery(rows, Alias::new("sources"));

    let mut winner = SeaQuery::select();
    winner
        .columns([
            Alias::new("site_id"),
            Alias::new("parameter_id"),
            Alias::new("warning_min"),
            Alias::new("warning_max"),
            Alias::new("alarm_min"),
            Alias::new("alarm_max"),
            Alias::new("source"),
        ])
        .from_subquery(ranked, Alias::new("ranked"))
        .and_where(Expr::col(Alias::new("rn")).eq(1));
    winner
}

/// The latest value at each `(site, parameter)` slot in the last 30 days, one row per slot.
///
/// Bounded to recent chunks so TimescaleDB excludes the rest. Continuous readings win over spot,
/// so an occasional grab does not stand in for a sensor's current value; a spot-only slot still
/// reports its latest grab.
pub fn latest_slot_values_query() -> SelectStatement {
    SeaQuery::select()
        .distinct_on([
            (Alias::new("r"), readings::Column::SiteId),
            (Alias::new("r"), readings::Column::ParameterId),
        ])
        .column((Alias::new("r"), readings::Column::SiteId))
        .column((Alias::new("r"), readings::Column::ParameterId))
        .expr_as(
            Expr::cust("COALESCE(smp.mean, r.calibrated_value, r.raw_value)"),
            Alias::new("current_value"),
        )
        .from_as(readings::Entity, Alias::new("r"))
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            Alias::new("smp"),
            Expr::col((Alias::new("smp"), samples::Column::Id))
                .equals((Alias::new("r"), readings::Column::SampleId)),
        )
        .and_where(Expr::col((Alias::new("r"), readings::Column::SiteId)).is_not_null())
        .and_where(Expr::cust("r.is_flagged IS NOT TRUE"))
        .and_where(Expr::cust("r.unverified IS NOT TRUE"))
        .and_where(Expr::cust("r.time > now() - interval '30 days'"))
        .order_by((Alias::new("r"), readings::Column::SiteId), Order::Asc)
        .order_by((Alias::new("r"), readings::Column::ParameterId), Order::Asc)
        .order_by_expr(
            Expr::cust("(r.measurement_type IS NOT DISTINCT FROM 'spot')"),
            Order::Asc,
        )
        .order_by((Alias::new("r"), readings::Column::Time), Order::Desc)
        .order_by(
            (Alias::new("r"), readings::Column::ReplicateIndex),
            Order::Asc,
        )
        .to_owned()
}

/// Render [`latest_slot_values_query`] to a standalone SQL string, for splicing as a CTE body
/// beside [`resolve_thresholds_query`].
pub fn latest_slot_values_sql() -> String {
    latest_slot_values_query().to_string(PostgresQueryBuilder)
}

/// Resolve the resolved threshold for one `(site, parameter)` slot, the per-slot wrapper over the
/// single [`resolve_thresholds_query`] definition. Returns `None` when the slot has no threshold
/// at any tier. An all-NULL row (the "Disabled" state) resolves to `Some(..)` with every bound
/// `None` and suppresses alarms, see [`ResolvedThreshold::is_disabled`].
pub async fn resolve_threshold(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Option<ResolvedThreshold>, sea_orm::DbErr> {
    let (sql, values) = resolve_thresholds_query(Some(site_id), Some(vec![parameter_id]))
        .build(PostgresQueryBuilder);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            values.0,
        ))
        .await?;

    row.map(|r| ResolvedThreshold::from_query_result(&r, ""))
        .transpose()
}

/// Threshold changes re-check breach state immediately instead of waiting for the periodic
/// backstop sweep. The reconcile is global (all active slots): threshold edits are rare, the
/// post-LATERAL-rewrite sweep is O(active slots), and a global threshold (`site_id IS NULL`)
/// fans out across every site carrying the parameter anyway.
pub struct AlarmThresholdOperations;

impl CRUDOperations for AlarmThresholdOperations {
    type Resource = AlarmThreshold;

    async fn after_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _entity: &mut AlarmThreshold,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _entity: &mut AlarmThreshold,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }

    async fn after_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        _id: Uuid,
    ) -> Result<(), ApiError> {
        reconcile_all_from_hook(db).await;
        Ok(())
    }
}
/// Row from the violations query
#[derive(Debug, FromQueryResult)]
pub(super) struct ViolationRow {
    pub(super) parameter_id: Uuid,
    pub(super) time: chrono::DateTime<chrono::FixedOffset>,
    pub(super) value: f64,
    pub(super) severity: i16,
}
/// Parameter with threshold info for building response
pub(super) struct ParameterWithThreshold {
    pub(super) id: Uuid,
    pub(super) name: String,
    pub(super) sensor_type: String,
    pub(super) display_units: Option<String>,
}
/// The site's breaching readings over a range, one row per violating instant
/// (`parameter_id, time, value, severity`), unordered. `param_ids` `None` covers every slot the
/// site has a threshold for. Binds `$1` = site id, `$2` = start,
/// `$3` = end; the threshold scope is inlined, so the CTE carries no bind params of its own.
///
/// Each arm is what the site serves for that cadence, filtered to violations inside the arm.
/// Continuous and derived rows live at replicate_index 0, kept when flagged: an out-of-range
/// value should keep alerting after someone flags it. A spot instant is the replicate group
/// `(stream_id, time)`, evaluated at the sample mean over its unflagged replicates (fallback:
/// the lowest unflagged replicate's own value when no sample row exists), so flagging a bad
/// replicate moves the evaluated value instead of hiding the instant or handing it to a
/// different replicate. A fully flagged group is not evaluated.
fn violations_parts(
    site_id: Uuid,
    param_ids: Option<Vec<Uuid>>,
    min_severity: i16,
) -> (SelectStatement, CommonTableExpression) {
    // The violation filter and severity ladder are built over each cadence arm's own value
    // expression, so the filter stays inside the index scan rather than above the union.
    let arm_predicates = |val_expr: &str| {
        (
            Expr::cust(violation_condition(
                val_expr,
                "t.warning_min",
                "t.warning_max",
                "t.alarm_min",
                "t.alarm_max",
                min_severity,
            )),
            Expr::cust(format!(
                "({})::smallint",
                severity_case(
                    val_expr,
                    "t.warning_min",
                    "t.warning_max",
                    "t.alarm_min",
                    "t.alarm_max",
                )
            )),
        )
    };
    let (violation_cont, sev_cont) = arm_predicates("COALESCE(r.calibrated_value, r.raw_value)");
    let (violation_spot, sev_spot) = arm_predicates("sp.value");

    let r = served::r();
    let t = Alias::new("t");
    let sp = Alias::new("sp");
    let resolved = Alias::new("resolved_thresholds");
    let in_range = || {
        Condition::all()
            .add(Expr::cust("r.site_id = $1"))
            .add(Expr::cust("r.time >= $2"))
            .add(Expr::cust("r.time <= $3"))
    };

    let mut continuous_arm = SeaQuery::select()
        .column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::Time))
        .expr_as(served::continuous_value(), Alias::new("value"))
        .expr_as(sev_cont, Alias::new("severity"))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            resolved.clone(),
            t.clone(),
            Expr::col((r.clone(), readings::Column::ParameterId))
                .equals((t.clone(), Alias::new("parameter_id"))),
        )
        .cond_where(
            in_range()
                .add(served::continuous_rows())
                .add(violation_cont),
        )
        .take();

    let smp = Alias::new("smp");
    let mut group = SeaQuery::select();
    group
        .distinct_on(served::spot_instant_key())
        .column((r.clone(), readings::Column::ParameterId))
        .column((r.clone(), readings::Column::Time))
        .expr_as(served::spot_value(), Alias::new("value"))
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            smp.clone(),
            Expr::col((smp, samples::Column::Id)).equals((r.clone(), readings::Column::SampleId)),
        )
        .cond_where(
            in_range()
                .add(
                    Expr::col((r.clone(), readings::Column::ParameterId)).in_subquery(
                        SeaQuery::select()
                            .column(Alias::new("parameter_id"))
                            .from(resolved.clone())
                            .take(),
                    ),
                )
                .add(served::served_spot()),
        );
    for (expr, order) in served::spot_instant_order() {
        group.order_by_expr(expr, order);
    }
    let spot_arm = SeaQuery::select()
        .columns([
            (sp.clone(), Alias::new("parameter_id")),
            (sp.clone(), Alias::new("time")),
            (sp.clone(), Alias::new("value")),
        ])
        .expr_as(sev_spot, Alias::new("severity"))
        .from_subquery(group.take(), sp.clone())
        .join_as(
            JoinType::InnerJoin,
            resolved.clone(),
            t.clone(),
            Expr::col((sp.clone(), Alias::new("parameter_id")))
                .equals((t.clone(), Alias::new("parameter_id"))),
        )
        .and_where(violation_spot)
        .take();

    let sv = Alias::new("sv");
    let violations = SeaQuery::select()
        .columns([
            (sv.clone(), Alias::new("parameter_id")),
            (sv.clone(), Alias::new("time")),
            (sv.clone(), Alias::new("value")),
            (sv.clone(), Alias::new("severity")),
        ])
        .from_subquery(
            continuous_arm.union(UnionType::All, spot_arm).take(),
            sv.clone(),
        )
        .take();

    let mut cte = CommonTableExpression::new();
    cte.table_name(resolved)
        .query(resolve_thresholds_query(Some(site_id), param_ids));
    (violations, cte)
}

/// The site's breaching readings as one statement, the CTE its threshold scope needs attached.
pub(crate) fn violations_query(
    site_id: Uuid,
    param_ids: Option<Vec<Uuid>>,
    min_severity: i16,
) -> WithQuery {
    let (violations, cte) = violations_parts(site_id, param_ids, min_severity);
    violations.with(WithClause::new().cte(cte).to_owned())
}

/// How many breaching readings each parameter contributes, over the same select
/// [`violations_query`] serves, so a count and the export it gates cannot disagree.
fn violation_counts_query(site_id: Uuid, min_severity: i16) -> WithQuery {
    let (violations, cte) = violations_parts(site_id, None, min_severity);
    let v = Alias::new("v");
    SeaQuery::select()
        .expr_as(
            Expr::col((v.clone(), Alias::new("parameter_id"))),
            Alias::new("pid"),
        )
        .expr_as(Func::count(Expr::cust("*")), Alias::new("n"))
        .from_subquery(violations, v.clone())
        .add_group_by([Expr::col((v, Alias::new("parameter_id")))])
        .take()
        .with(WithClause::new().cte(cte).to_owned())
}

/// One parameter's row count over a range.
#[derive(FromQueryResult)]
pub(super) struct ParameterCount {
    pub(super) pid: Uuid,
    pub(super) n: i64,
}
pub async fn count_violations_by_parameter(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> AppResult<std::collections::HashMap<Uuid, i64>> {
    let sql = violation_counts_query(site_id, 1).to_string(PostgresQueryBuilder);
    let mut counts = std::collections::HashMap::new();
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            [
                site_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(start).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(end).into(),
            ],
        ))
        .await?
    {
        let counted = ParameterCount::from_query_result(&row, "")?;
        counts.insert(counted.pid, counted.n);
    }
    Ok(counts)
}
pub(crate) fn cadence_label(spot: bool) -> &'static str {
    if spot { "spot" } else { "continuous" }
}
/// Correlated subquery body for the latest served value at one slot, columns `value` and `time`.
/// Continuous and derived rows live at replicate_index 0 and stay when flagged: an out-of-range
/// value keeps alerting after someone flags it. A spot instant is evaluated at the sample mean
/// over its unflagged replicates (fallback: the lowest unflagged replicate's own value when no
/// sample row exists), so flagging a bad replicate moves the evaluated value instead of hiding
/// the instant or handing it to a different replicate; a fully flagged group is skipped.
pub(crate) fn latest_served_query(spot: bool, site: Expr, parameter: Expr) -> SelectStatement {
    let r = served::r();
    let mut latest = SeaQuery::select();
    latest
        .column((r.clone(), readings::Column::Time))
        // The instrument the value was measured on, which is what an instrument-range episode is
        // about. Unread by the threshold arm, and one more column on a single-row lateral.
        .column((r.clone(), readings::Column::SensorId))
        .from_as(readings::Entity, r.clone())
        .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(site))
        .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).eq(parameter))
        .order_by((r.clone(), readings::Column::Time), Order::Desc)
        .limit(1);
    if spot {
        let smp = Alias::new("smp");
        latest
            .expr_as(served::spot_value(), Alias::new("value"))
            .join_as(
                JoinType::LeftJoin,
                samples::Entity,
                smp.clone(),
                Expr::col((smp, samples::Column::Id))
                    .equals((r.clone(), readings::Column::SampleId)),
            )
            .cond_where(served::served_spot())
            .order_by((r.clone(), readings::Column::ReplicateIndex), Order::Asc);
    } else {
        latest
            .expr_as(served::continuous_value(), Alias::new("value"))
            .cond_where(served::continuous_rows());
    }
    latest.take()
}
/// Row from the active alarms query
#[derive(Debug, FromQueryResult)]
pub(crate) struct ActiveAlarmRow {
    pub(crate) site_id: Uuid,
    pub(crate) site_name: String,
    pub(crate) parameter_id: Uuid,
    pub(crate) parameter_name: String,
    pub(crate) current_value: f64,
    pub(crate) time: chrono::DateTime<chrono::FixedOffset>,
    pub(crate) warning_min: Option<f64>,
    pub(crate) warning_max: Option<f64>,
    pub(crate) alarm_min: Option<f64>,
    pub(crate) alarm_max: Option<f64>,
    pub(crate) severity: i16,
}
impl ActiveAlarmRow {
    /// The four bounds this row carries, as the one type that names them.
    pub(super) fn bounds(&self) -> ResolvedThreshold {
        ResolvedThreshold {
            warning_min: self.warning_min,
            warning_max: self.warning_max,
            alarm_min: self.alarm_min,
            alarm_max: self.alarm_max,
        }
    }
}
/// Fetch active alarm violations across all sites. The sweeper reuses this as the "current breach
/// set" so the persisted events never diverge from what `/alarms/active` would compute.
/// `spot` selects the cadence lens: grab series and sensor series are evaluated independently,
/// so a monthly grab can neither mask nor phantom-resolve a sensor breach.
pub(crate) async fn fetch_active_alarm_rows<C: ConnectionTrait>(
    db: &C,
    scope: &crate::common::authz::AccessScope,
    slots: Option<&[(Uuid, Uuid)]>,
    spot: bool,
) -> AppResult<Vec<ActiveAlarmRow>> {
    // Empty slot list means "evaluate nothing", short-circuit before building an invalid `IN ()`.
    if matches!(slots, Some(s) if s.is_empty()) {
        return Ok(Vec::new());
    }

    let sev_case = severity_case(
        "lr.value",
        "rt.warning_min",
        "rt.warning_max",
        "rt.alarm_min",
        "rt.alarm_max",
    );
    let violation = violation_condition(
        "lr.value",
        "rt.warning_min",
        "rt.warning_max",
        "rt.alarm_min",
        "rt.alarm_max",
        1,
    );

    let rt = Alias::new("rt");
    let s_ = Alias::new("s");
    let sp = Alias::new("sp");
    let lr = Alias::new("lr");
    let resolved = Alias::new("resolved_thresholds");

    let mut active = Condition::all().add(Expr::cust(violation));
    if let Some(predicate) = project_filter(scope, (s_.clone(), sites::Column::ProjectId)) {
        active = active.add(predicate);
    }
    if let Some(slots) = slots {
        let pairs: Vec<Expr> = slots
            .iter()
            .map(|(site_id, parameter_id)| {
                Expr::tuple([Expr::value(*site_id), Expr::value(*parameter_id)])
            })
            .collect();
        active = active.add(
            Expr::tuple([
                Expr::col((rt.clone(), Alias::new("site_id"))),
                Expr::col((rt.clone(), Alias::new("parameter_id"))),
            ])
            .is_in(pairs),
        );
    }

    // Loose index scan: one `ORDER BY time DESC LIMIT 1` per active slot via
    // `idx_readings_site_param_time`, instead of a `DISTINCT ON` over the whole hypertable. Cost is
    // O(active slots), independent of history depth. The lateral body carries the per-cadence
    // serving rule (see `latest_served_query`).
    let latest = latest_served_query(
        spot,
        Expr::col((rt.clone(), Alias::new("site_id"))),
        Expr::col((rt.clone(), Alias::new("parameter_id"))),
    );

    let mut rows_query = SeaQuery::select();
    rows_query
        .column((rt.clone(), Alias::new("site_id")))
        .expr_as(
            Expr::col((s_.clone(), sites::Column::Name)),
            Alias::new("site_name"),
        )
        .column((rt.clone(), Alias::new("parameter_id")))
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::Name)),
            Alias::new("parameter_name"),
        )
        .expr_as(
            Expr::col((lr.clone(), Alias::new("value"))),
            Alias::new("current_value"),
        )
        .column((lr.clone(), Alias::new("time")))
        .columns([
            (rt.clone(), Alias::new("warning_min")),
            (rt.clone(), Alias::new("warning_max")),
            (rt.clone(), Alias::new("alarm_min")),
            (rt.clone(), Alias::new("alarm_max")),
        ])
        .expr_as(
            Expr::cust(format!("({sev_case})::smallint")),
            Alias::new("severity"),
        )
        .from_as(resolved.clone(), rt.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s_.clone(),
            Expr::col((s_.clone(), sites::Column::Id)).equals((rt.clone(), Alias::new("site_id"))),
        )
        .join_as(
            JoinType::InnerJoin,
            site_parameters::Entity,
            sp.clone(),
            Condition::all()
                .add(
                    Expr::col((sp.clone(), site_parameters::Column::SiteId))
                        .equals((rt.clone(), Alias::new("site_id"))),
                )
                .add(
                    Expr::col((sp.clone(), site_parameters::Column::ParameterId))
                        .equals((rt.clone(), Alias::new("parameter_id"))),
                )
                .add(Expr::col((sp.clone(), site_parameters::Column::IsActive)).eq(true)),
        )
        // `ON TRUE` rather than `JoinType::CrossJoin`, which the builder still writes an `ON`
        // clause after; the two mean the same thing.
        .join_lateral(
            JoinType::InnerJoin,
            latest,
            lr.clone(),
            Condition::all().add(Expr::cust("true")),
        )
        .cond_where(active)
        .order_by(Alias::new("severity"), Order::Desc)
        .order_by((s_.clone(), sites::Column::Name), Order::Asc)
        .order_by(Alias::new("parameter_name"), Order::Asc);

    // The single resolution definition across all active slots (no scope), as the CTE.
    let mut cte = CommonTableExpression::new();
    cte.table_name(resolved)
        .query(resolve_thresholds_query(None, None));
    let (sql, values) = rows_query
        .take()
        .with(WithClause::new().cte(cte).to_owned())
        .build(PostgresQueryBuilder);

    let rows: Vec<ActiveAlarmRow> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| ActiveAlarmRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows)
}
/// One slot whose latest served value is outside what the instrument that measured it can read.
#[derive(Debug, FromQueryResult)]
pub(crate) struct InstrumentRangeRow {
    pub(crate) site_id: Uuid,
    pub(crate) site_name: String,
    pub(crate) parameter_id: Uuid,
    pub(crate) parameter_name: String,
    pub(crate) sensor_id: Uuid,
    pub(crate) sensor_name: Option<String>,
    pub(crate) current_value: f64,
    pub(crate) time: chrono::DateTime<chrono::FixedOffset>,
    pub(crate) range_min: Option<f64>,
    pub(crate) range_max: Option<f64>,
}

/// The instrument-range breach set: the same shape as [`fetch_active_alarm_rows`], read against
/// the instrument's own bounds instead of the slot's thresholds. An instrument declaring no range
/// contributes nothing, so a database that has entered none behaves exactly as before.
pub(crate) async fn fetch_instrument_range_rows<C: ConnectionTrait>(
    db: &C,
    scope: &crate::common::authz::AccessScope,
    slots: Option<&[(Uuid, Uuid)]>,
    spot: bool,
) -> AppResult<Vec<InstrumentRangeRow>> {
    if matches!(slots, Some(s) if s.is_empty()) {
        return Ok(Vec::new());
    }

    let sp = Alias::new("sp");
    let s_ = Alias::new("s");
    let lr = Alias::new("lr");
    let sen = Alias::new("sen");

    let mut active = Condition::all().add(Expr::cust(instrument_range_condition(
        "lr.value",
        "sen.range_min",
        "sen.range_max",
    )));
    if let Some(predicate) = project_filter(scope, (s_.clone(), sites::Column::ProjectId)) {
        active = active.add(predicate);
    }
    if let Some(slots) = slots {
        let pairs: Vec<Expr> = slots
            .iter()
            .map(|(site_id, parameter_id)| {
                Expr::tuple([Expr::value(*site_id), Expr::value(*parameter_id)])
            })
            .collect();
        active = active.add(
            Expr::tuple([
                Expr::col((sp.clone(), site_parameters::Column::SiteId)),
                Expr::col((sp.clone(), site_parameters::Column::ParameterId)),
            ])
            .is_in(pairs),
        );
    }

    let latest = latest_served_query(
        spot,
        Expr::col((sp.clone(), site_parameters::Column::SiteId)),
        Expr::col((sp.clone(), site_parameters::Column::ParameterId)),
    );

    let mut rows_query = SeaQuery::select();
    rows_query
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::SiteId)),
            Alias::new("site_id"),
        )
        .expr_as(
            Expr::col((s_.clone(), sites::Column::Name)),
            Alias::new("site_name"),
        )
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::ParameterId)),
            Alias::new("parameter_id"),
        )
        .expr_as(
            Expr::col((sp.clone(), site_parameters::Column::Name)),
            Alias::new("parameter_name"),
        )
        .expr_as(
            Expr::col((sen.clone(), sensors::Column::Id)),
            Alias::new("sensor_id"),
        )
        .expr_as(
            Expr::col((sen.clone(), sensors::Column::Name)),
            Alias::new("sensor_name"),
        )
        .expr_as(
            Expr::col((lr.clone(), Alias::new("value"))),
            Alias::new("current_value"),
        )
        .column((lr.clone(), Alias::new("time")))
        .columns([
            (sen.clone(), sensors::Column::RangeMin),
            (sen.clone(), sensors::Column::RangeMax),
        ])
        .from_as(site_parameters::Entity, sp.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s_.clone(),
            Expr::col((s_.clone(), sites::Column::Id))
                .equals((sp.clone(), site_parameters::Column::SiteId)),
        )
        .join_lateral(
            JoinType::InnerJoin,
            latest,
            lr.clone(),
            Condition::all().add(Expr::cust("true")),
        )
        .join_as(
            JoinType::InnerJoin,
            sensors::Entity,
            sen.clone(),
            Expr::col((sen.clone(), sensors::Column::Id))
                .equals((lr.clone(), readings::Column::SensorId)),
        )
        .and_where(Expr::col((sp.clone(), site_parameters::Column::IsActive)).eq(true))
        .cond_where(active)
        .order_by((s_.clone(), sites::Column::Name), Order::Asc)
        .order_by(Alias::new("parameter_name"), Order::Asc);

    let (sql, values) = rows_query.build(PostgresQueryBuilder);
    let rows: Vec<InstrumentRangeRow> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| InstrumentRangeRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows)
}

/// Open persisted alarm event, keyed by (site, parameter, measurement_type, kind) for annotating
/// the live feed. The kind is in the key because one slot holds one open episode of each.
#[derive(Debug, FromQueryResult)]
pub(super) struct OpenEventRow {
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
    pub(super) measurement_type: String,
    pub(super) kind: String,
    pub(super) id: Uuid,
    pub(super) started_at: chrono::DateTime<chrono::FixedOffset>,
    pub(super) acknowledged_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub(super) acknowledged_by: Option<String>,
    pub(super) max_severity: i16,
}
/// Fetch the currently-open alarm events as a map keyed by (site_id, parameter_id,
/// measurement_type, kind). Used to attach `event_id` + acknowledgement state to the (stateless)
/// current-breach feed.
pub(super) async fn fetch_open_events(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<(Uuid, Uuid, String, String), OpenEventRow>> {
    let ae = Alias::new("ae");
    let s_ = Alias::new("s");
    let mut open =
        Condition::all().add(Expr::col((ae.clone(), alarm_event::Column::ResolvedAt)).is_null());
    if let Some(predicate) = project_filter(scope, (s_.clone(), sites::Column::ProjectId)) {
        open = open.add(predicate);
    }
    let (sql, values) = SeaQuery::select()
        .columns([
            (ae.clone(), alarm_event::Column::SiteId),
            (ae.clone(), alarm_event::Column::ParameterId),
            (ae.clone(), alarm_event::Column::MeasurementType),
            (ae.clone(), alarm_event::Column::Kind),
            (ae.clone(), alarm_event::Column::Id),
            (ae.clone(), alarm_event::Column::StartedAt),
            (ae.clone(), alarm_event::Column::AcknowledgedAt),
            (ae.clone(), alarm_event::Column::AcknowledgedBy),
            (ae.clone(), alarm_event::Column::MaxSeverity),
        ])
        .from_as(alarm_event::Entity, ae.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s_.clone(),
            Expr::col((s_.clone(), sites::Column::Id))
                .equals((ae.clone(), alarm_event::Column::SiteId)),
        )
        .cond_where(open)
        .take()
        .build(PostgresQueryBuilder);
    let mut map = HashMap::new();
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
    {
        if let Ok(r) = OpenEventRow::from_query_result(&row, "") {
            map.insert(
                (
                    r.site_id,
                    r.parameter_id,
                    r.measurement_type.clone(),
                    r.kind.clone(),
                ),
                r,
            );
        }
    }
    Ok(map)
}
/// Confine an id-addressed alarm event to the caller's projects. An event outside them and an event
/// that does not exist answer identically, so acknowledging by id cannot be used to discover that
/// another project's alarm exists. The listing (`/alarms/events`) is filtered by the same rule.
pub(super) async fn confine_alarm_event(
    state: &AppState,
    scope: &crate::common::authz::AccessScope,
    event_id: Uuid,
) -> AppResult<()> {
    let row = project_of_alarm_event(&state.db, event_id).await?;
    require_row_in_scope(scope, &row, Unowned::Deny, "Alarm event")
}
/// Row from the latest reading time query
#[derive(Debug, FromQueryResult)]
pub(super) struct LatestReadingTimeRow {
    pub(super) site_id: Uuid,
    pub(super) site_name: String,
    pub(super) latest_time: chrono::DateTime<chrono::FixedOffset>,
}
/// Fetch per-site latest reading time across all paired readings
pub(super) async fn fetch_latest_reading_times(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<Uuid, (String, DateTime<Utc>)>> {
    let s_ = Alias::new("s");
    let r_ = Alias::new("r");
    let mut scoped = Condition::all();
    if let Some(predicate) = project_filter(scope, (s_.clone(), sites::Column::ProjectId)) {
        scoped = scoped.add(predicate);
    }
    // Continuous-only: a monthly grab must not make a dead logger look alive.
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((s_.clone(), sites::Column::Id)),
            Alias::new("site_id"),
        )
        .expr_as(
            Expr::col((s_.clone(), sites::Column::Name)),
            Alias::new("site_name"),
        )
        .expr_as(
            Func::max(Expr::col((r_.clone(), readings::Column::Time))),
            Alias::new("latest_time"),
        )
        .from_as(sites::Entity, s_.clone())
        .join_as(
            JoinType::InnerJoin,
            readings::Entity,
            r_.clone(),
            Condition::all()
                .add(
                    Expr::col((r_.clone(), readings::Column::SiteId))
                        .equals((s_.clone(), sites::Column::Id)),
                )
                .add(Expr::cust("r.measurement_type IS DISTINCT FROM 'spot'")),
        )
        .cond_where(scoped)
        .add_group_by([
            Expr::col((s_.clone(), sites::Column::Id)),
            Expr::col((s_.clone(), sites::Column::Name)),
        ])
        .take()
        .build(PostgresQueryBuilder);

    let rows: Vec<LatestReadingTimeRow> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| LatestReadingTimeRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows
        .into_iter()
        .map(|r| (r.site_id, (r.site_name, r.latest_time.with_timezone(&Utc))))
        .collect())
}
/// Row from the per-site last warning/alarm event query
#[derive(Debug, FromQueryResult)]
pub(super) struct LastAlarmWarningRow {
    pub(super) site_id: Uuid,
    pub(super) last_warning_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub(super) last_alarm_at: Option<chrono::DateTime<chrono::FixedOffset>>,
}
/// Fetch per-site last persisted warning (max_severity = 1) and alarm (max_severity = 2) timestamps
/// from `alarm_events`, keyed by site_id as `(last_warning_at, last_alarm_at)`.
pub(super) async fn fetch_last_alarm_warning_times(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<Uuid, (Option<DateTime<Utc>>, Option<DateTime<Utc>>)>> {
    let s_ = Alias::new("s");
    let ae = Alias::new("ae");
    let mut scoped = Condition::all();
    if let Some(predicate) = project_filter(scope, (s_.clone(), sites::Column::ProjectId)) {
        scoped = scoped.add(predicate);
    }
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((s_.clone(), sites::Column::Id)),
            Alias::new("site_id"),
        )
        .expr_as(
            Expr::cust("MAX(ae.last_seen_at) FILTER (WHERE ae.max_severity = 1)"),
            Alias::new("last_warning_at"),
        )
        .expr_as(
            Expr::cust("MAX(ae.last_seen_at) FILTER (WHERE ae.max_severity = 2)"),
            Alias::new("last_alarm_at"),
        )
        .from_as(alarm_event::Entity, ae.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s_.clone(),
            Expr::col((s_.clone(), sites::Column::Id))
                .equals((ae.clone(), alarm_event::Column::SiteId)),
        )
        .cond_where(scoped)
        .add_group_by([Expr::col((s_.clone(), sites::Column::Id))])
        .take()
        .build(PostgresQueryBuilder);

    let rows: Vec<LastAlarmWarningRow> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|row| LastAlarmWarningRow::from_query_result(&row, "").ok())
        .collect();

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.site_id,
                (
                    r.last_warning_at.map(|t| t.with_timezone(&Utc)),
                    r.last_alarm_at.map(|t| t.with_timezone(&Utc)),
                ),
            )
        })
        .collect())
}
/// Row from the persisted alarm-events feed query
#[derive(Debug, FromQueryResult)]
pub(super) struct AlarmEventRow {
    pub(super) id: Uuid,
    pub(super) site_id: Uuid,
    pub(super) site_name: String,
    pub(super) parameter_id: Uuid,
    pub(super) parameter_name: String,
    pub(super) measurement_type: String,
    pub(super) kind: String,
    pub(super) sensor_id: Option<Uuid>,
    pub(super) severity: i16,
    pub(super) max_severity: i16,
    pub(super) started_at: chrono::DateTime<chrono::FixedOffset>,
    pub(super) last_seen_at: chrono::DateTime<chrono::FixedOffset>,
    pub(super) value_at_start: f64,
    pub(super) last_value: f64,
    pub(super) resolved_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub(super) resolved_value: Option<f64>,
    pub(super) acknowledged_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub(super) acknowledged_by: Option<String>,
}
#[derive(Debug, FromQueryResult)]
pub(super) struct EpisodeRow {
    pub(super) started_at: DateTime<FixedOffset>,
    pub(super) value_at_start: f64,
    pub(super) last_seen_at: DateTime<FixedOffset>,
    pub(super) last_value: f64,
    pub(super) max_severity: i16,
    pub(super) severity: i16,
    pub(super) resolved_at: Option<DateTime<FixedOffset>>,
    pub(super) resolved_value: Option<f64>,
}
/// The series one cadence's episodes are computed over: continuous and derived rows stay when
/// flagged, because an out-of-range value keeps alerting, while a spot instant is served at the
/// sample mean over its unflagged replicates, so a fully flagged group is skipped.
pub(crate) fn ordered_query(spot: bool) -> SelectStatement {
    let r = served::r();
    let in_window = || {
        Condition::all()
            .add(Expr::cust("r.site_id = $1"))
            .add(Expr::cust("r.parameter_id = $2"))
            .add(Expr::cust("r.time >= $3"))
            .add(Expr::cust("r.time <= $4"))
    };
    if spot {
        let smp = Alias::new("smp");
        let mut group = SeaQuery::select();
        group
            .distinct_on(served::spot_instant_key())
            .expr_as(
                Expr::col((r.clone(), readings::Column::Time)),
                Alias::new("t"),
            )
            .expr_as(served::spot_value(), Alias::new("v"))
            .from_as(readings::Entity, r.clone())
            .join_as(
                JoinType::LeftJoin,
                samples::Entity,
                smp.clone(),
                Expr::col((smp, samples::Column::Id))
                    .equals((r.clone(), readings::Column::SampleId)),
            )
            .cond_where(in_window().add(served::served_spot()));
        for (expr, order) in served::spot_instant_order() {
            group.order_by_expr(expr, order);
        }
        let sp = Alias::new("sp");
        SeaQuery::select()
            .columns([(sp.clone(), Alias::new("t")), (sp.clone(), Alias::new("v"))])
            .from_subquery(group.take(), sp)
            .take()
    } else {
        SeaQuery::select()
            .expr_as(
                Expr::col((r.clone(), readings::Column::Time)),
                Alias::new("t"),
            )
            .expr_as(served::continuous_value(), Alias::new("v"))
            .from_as(readings::Entity, r.clone())
            .cond_where(in_window().add(served::continuous_rows()))
            .take()
    }
}

/// One cadence's breach episodes over a window: per-instant severity, then gaps-and-islands.
///
/// Binds `$1` = site id, `$2` = parameter id, `$3` = start, `$4` = end, and whatever `sev_case`
/// reads for the four bounds.
pub(crate) fn episodes_query(sev_case: &str, spot: bool) -> WithQuery {
    // Per-instant severity, then gaps-and-islands. `ordered` is the served series for the
    // cadence: continuous and derived rows live at replicate_index 0 and stay when flagged (an
    // out-of-range value keeps alerting); a spot instant is the replicate group at its slot,
    // evaluated at the sample mean over its unflagged replicates (fallback: the lowest
    // unflagged replicate's own value when no sample row exists), so a fully flagged group is
    // skipped. `scored` applies the severity ladder; `marked` computes the LAG/LEAD neighbours;
    // `runs` then cumulatively sums the run-start flag (a window function can't be nested inside
    // another, so these must be separate CTEs). `run_id` increments at each breach that follows a
    // non-breach, so all consecutive breaching readings share one id. `next_t`/`next_v` from the
    // run's last row is the following in-range reading (NULL when the run reaches the window edge).
    let t = Alias::new("t");
    let v = Alias::new("v");
    let sev = Alias::new("sev");
    let ordered = Alias::new("ordered");
    let scored = Alias::new("scored");
    let marked = Alias::new("marked");
    let runs = Alias::new("runs");
    let w = Alias::new("w");

    let mut ordered_cte = CommonTableExpression::new();
    ordered_cte
        .table_name(ordered.clone())
        .query(ordered_query(spot));

    let mut scored_cte = CommonTableExpression::new();
    scored_cte.table_name(scored.clone()).query(
        SeaQuery::select()
            .columns([t.clone(), v.clone()])
            .expr_as(Expr::cust(sev_case.to_owned()), sev.clone())
            .from(ordered)
            .take(),
    );

    let mut marked_cte = CommonTableExpression::new();
    marked_cte.table_name(marked.clone()).query(
        SeaQuery::select()
            .columns([t.clone(), v.clone(), sev.clone()])
            .expr_as(Expr::cust("sev > 0"), Alias::new("breach"))
            .expr_as(
                Expr::cust(
                    "CASE WHEN sev > 0 AND COALESCE(LAG(sev) OVER w, 0) = 0 THEN 1 ELSE 0 END",
                ),
                Alias::new("run_start"),
            )
            .expr_as(Expr::cust("LEAD(t) OVER w"), Alias::new("next_t"))
            .expr_as(Expr::cust("LEAD(v) OVER w"), Alias::new("next_v"))
            .from(scored)
            .window(
                w,
                WindowStatement::new()
                    .order_by(t.clone(), Order::Asc)
                    .take(),
            )
            .take(),
    );

    let mut runs_cte = CommonTableExpression::new();
    runs_cte.table_name(runs.clone()).query(
        SeaQuery::select()
            .columns([
                t.clone(),
                v.clone(),
                sev,
                Alias::new("breach"),
                Alias::new("next_t"),
                Alias::new("next_v"),
            ])
            .expr_window_as(
                Expr::cust("SUM(run_start)"),
                WindowStatement::new()
                    .order_by(t.clone(), Order::Asc)
                    .frame_start(FrameType::Rows, Frame::UnboundedPreceding)
                    .take(),
                Alias::new("run_id"),
            )
            .from(marked)
            .take(),
    );

    let episodes_query = SeaQuery::select()
        .expr_as(Func::min(Expr::col(t.clone())), Alias::new("started_at"))
        .expr_as(
            Expr::cust("(ARRAY_AGG(v ORDER BY t ASC))[1]"),
            Alias::new("value_at_start"),
        )
        .expr_as(Func::max(Expr::col(t)), Alias::new("last_seen_at"))
        .expr_as(
            Expr::cust("(ARRAY_AGG(v ORDER BY t DESC))[1]"),
            Alias::new("last_value"),
        )
        .expr_as(Expr::cust("MAX(sev)::smallint"), Alias::new("max_severity"))
        .expr_as(
            Expr::cust("(ARRAY_AGG(sev ORDER BY t DESC))[1]::smallint"),
            Alias::new("severity"),
        )
        .expr_as(
            Expr::cust("(ARRAY_AGG(next_t ORDER BY t DESC))[1]"),
            Alias::new("resolved_at"),
        )
        .expr_as(
            Expr::cust("(ARRAY_AGG(next_v ORDER BY t DESC))[1]"),
            Alias::new("resolved_value"),
        )
        .from(runs)
        .and_where(Expr::col(Alias::new("breach")).eq(true))
        .add_group_by([Expr::col(Alias::new("run_id"))])
        .and_having(Expr::cust(
            "(ARRAY_AGG(next_t ORDER BY t DESC))[1] IS NOT NULL",
        ))
        .order_by(Alias::new("started_at"), Order::Asc)
        .take()
        .with(
            WithClause::new()
                .cte(ordered_cte)
                .cte(scored_cte)
                .cte(marked_cte)
                .cte(runs_cte)
                .to_owned(),
        );
    episodes_query
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn fetch_episodes(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    threshold: &ResolvedThreshold,
    sev_case: &str,
    spot: bool,
) -> Result<Vec<EpisodeRow>, sea_orm::DbErr> {
    let sql = episodes_query(sev_case, spot).to_string(PostgresQueryBuilder);

    let values: Vec<sea_orm::Value> = vec![
        site_id.into(),
        parameter_id.into(),
        start.into(),
        end.into(),
        threshold.warning_min.into(),
        threshold.warning_max.into(),
        threshold.alarm_min.into(),
        threshold.alarm_max.into(),
    ];

    let episodes: Vec<EpisodeRow> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?
        .into_iter()
        .filter_map(|r| EpisodeRow::from_query_result(&r, "").ok())
        .collect();

    Ok(episodes)
}
/// A slot the rebuild covers, and the extent of its readings. Every column is nullable: an
/// unattributed reading names no slot, and MIN/MAX over an empty one is NULL.
#[derive(sea_orm::FromQueryResult)]
pub(super) struct SlotRow {
    pub(super) site_id: Option<Uuid>,
    pub(super) parameter_id: Option<Uuid>,
}
#[derive(sea_orm::FromQueryResult)]
pub(super) struct ExtentRow {
    pub(super) lo: Option<DateTime<Utc>>,
    pub(super) hi: Option<DateTime<Utc>>,
}

#[cfg(test)]
#[path = "tests/service.rs"]
mod tests;
