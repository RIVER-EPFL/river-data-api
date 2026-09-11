//! Alarm queries and the threshold resolution they share.
//!
//! The breach definition (which value counts as warning, which as alarm) and the two-tier
//! threshold fallback live here so the live evaluation and the historical episode rebuild cannot
//! drift apart.

use chrono::{DateTime, FixedOffset, Utc};
use crudcrate::{ApiError, CRUDOperations};
use sea_orm::sea_query::{
    Alias, CommonTableExpression, Condition, Expr, Func, JoinType, Order, PostgresQueryBuilder,
    Query as SeaQuery, SelectStatement, UnionType, WithClause, WithQuery,
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
use crate::routes::private::sites::models as sites;
use crate::routes::private::site_parameters::models as site_parameters;
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
pub(crate) fn violations_query(
    site_id: Uuid,
    param_ids: Option<Vec<Uuid>>,
    min_severity: i16,
) -> WithQuery {
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
    violations.with(WithClause::new().cte(cte).to_owned())
}

/// How many breaching readings each parameter contributes over a range, from the same definition
/// [`violations_sql`] serves, so a count and the export it gates cannot disagree.
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
    let sql = format!(
        "SELECT parameter_id AS pid, COUNT(*) AS n FROM ({}) v GROUP BY parameter_id",
        violations_query(site_id, None, 1).to_string(PostgresQueryBuilder)
    );
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
pub(crate) fn latest_served_query(spot: bool, site_col: &str, param_col: &str) -> SelectStatement {
    let r = served::r();
    let mut latest = SeaQuery::select();
    latest
        .column((r.clone(), readings::Column::Time))
        .from_as(readings::Entity, r.clone())
        .and_where(Expr::cust(format!("r.site_id = {site_col}")))
        .and_where(Expr::cust(format!("r.parameter_id = {param_col}")))
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
    let latest = latest_served_query(spot, "rt.site_id", "rt.parameter_id");

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
/// Open persisted alarm event, keyed by (site, parameter, measurement_type) for annotating the
/// live feed.
#[derive(Debug, FromQueryResult)]
pub(super) struct OpenEventRow {
    pub(super) site_id: Uuid,
    pub(super) parameter_id: Uuid,
    pub(super) measurement_type: String,
    pub(super) id: Uuid,
    pub(super) started_at: chrono::DateTime<chrono::FixedOffset>,
    pub(super) acknowledged_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub(super) acknowledged_by: Option<String>,
    pub(super) max_severity: i16,
}
/// Fetch the currently-open alarm events as a map keyed by (site_id, parameter_id,
/// measurement_type). Used to attach `event_id` + acknowledgement state to the (stateless)
/// current-breach feed.
pub(super) async fn fetch_open_events(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
) -> AppResult<HashMap<(Uuid, Uuid, String), OpenEventRow>> {
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
            map.insert((r.site_id, r.parameter_id, r.measurement_type.clone()), r);
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
            Expr::col((s_.clone(), sites::Column::Id)).into(),
            Expr::col((s_.clone(), sites::Column::Name)).into(),
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
        .add_group_by([Expr::col((s_.clone(), sites::Column::Id)).into()])
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
    let ordered = ordered_query(spot).to_string(PostgresQueryBuilder);
    let sql = format!(
        r"
        WITH ordered AS ({ordered}),
        scored AS (
            SELECT t, v, {sev_case} AS sev FROM ordered
        ),
        marked AS (
            SELECT t, v, sev,
                   (sev > 0) AS breach,
                   CASE WHEN sev > 0 AND COALESCE(LAG(sev) OVER w, 0) = 0 THEN 1 ELSE 0 END AS run_start,
                   LEAD(t) OVER w AS next_t,
                   LEAD(v) OVER w AS next_v
            FROM scored
            WINDOW w AS (ORDER BY t)
        ),
        runs AS (
            SELECT t, v, sev, breach, next_t, next_v,
                   SUM(run_start) OVER (ORDER BY t ROWS UNBOUNDED PRECEDING) AS run_id
            FROM marked
        )
        SELECT
            MIN(t) AS started_at,
            (ARRAY_AGG(v ORDER BY t ASC))[1] AS value_at_start,
            MAX(t) AS last_seen_at,
            (ARRAY_AGG(v ORDER BY t DESC))[1] AS last_value,
            MAX(sev)::smallint AS max_severity,
            (ARRAY_AGG(sev ORDER BY t DESC))[1]::smallint AS severity,
            (ARRAY_AGG(next_t ORDER BY t DESC))[1] AS resolved_at,
            (ARRAY_AGG(next_v ORDER BY t DESC))[1] AS resolved_value
        FROM runs
        WHERE breach
        GROUP BY run_id
        HAVING (ARRAY_AGG(next_t ORDER BY t DESC))[1] IS NOT NULL
        ORDER BY started_at
        "
    );

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
