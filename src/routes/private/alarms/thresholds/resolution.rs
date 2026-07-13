//! Shared alarm-threshold resolution and severity logic.
//!
//! The breach definition (which value counts as warning vs alarm) and the 3-priority threshold
//! fallback live here so the live evaluation (`views.rs`, `sweeper.rs`) and the historical episode
//! rebuild (`episodes.rs`) can't drift apart.

use sea_orm::sea_query::{
    Alias, Condition, Expr, JoinType, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
    UnionType,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use uuid::Uuid;

/// The four numeric bounds that define a breach for one (parameter, site) slot.
#[derive(Debug, Clone, Copy, FromQueryResult)]
pub struct ResolvedThreshold {
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
}

impl ResolvedThreshold {
    /// A threshold with every bound NULL never fires — this is the "Disabled" state written by the
    /// UI (a null-valued row at priority 1 that blocks the parameter-default fallback).
    pub fn is_disabled(&self) -> bool {
        self.warning_min.is_none()
            && self.warning_max.is_none()
            && self.alarm_min.is_none()
            && self.alarm_max.is_none()
    }
}

/// SQL `CASE` mapping a value to a severity (`2`=alarm, `1`=warning, `0`=ok). Callers pass the value
/// expression and the four bound expressions — column refs for the live queries (`t.alarm_min`,
/// `rt.alarm_min`), bind-param refs for the episode query (`$7::double precision`) — so the severity
/// ladder is defined in exactly one place. Result is a bare integer; cast to `smallint` at the call
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
/// so the bucket severity is the worse of the two — expressed via [`severity_of`] so the ladder is
/// defined once.
pub fn severity_of_range(min: Option<f64>, max: Option<f64>, t: &ResolvedThreshold) -> i16 {
    let lo = min.map(|v| severity_of(v, t)).unwrap_or(0);
    let hi = max.map(|v| severity_of(v, t)).unwrap_or(0);
    lo.max(hi)
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
    let alarm = format!("({amin} IS NOT NULL AND {val} < {amin}) OR ({amax} IS NOT NULL AND {val} > {amax})");
    if min_severity >= 2 {
        format!("({alarm})")
    } else {
        format!(
            "({alarm} OR ({wmin} IS NOT NULL AND {val} < {wmin}) OR ({wmax} IS NOT NULL AND {val} > {wmax}))"
        )
    }
}

/// One resolved row of the resolved-thresholds query: the winning threshold per active
/// `(site, parameter)` slot, with the tier it came from.
#[derive(Debug, Clone, FromQueryResult, serde::Serialize, utoipa::ToSchema)]
pub struct ThresholdRow {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
    /// `"site"` | `"global"` | `"default"` — which tier supplied this threshold.
    pub source: String,
}

fn col(table: &str, c: &str) -> Expr {
    Expr::col((Alias::new(table), Alias::new(c)))
}

/// THE single definition of the 3-tier resolution (site row → global row → parameter `default_*`).
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
            Expr::case(col("t", "site_id").equals((Alias::new("sp"), Alias::new("site_id"))), 1)
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

    // Tier 3: parameter defaults, for slots whose parameter carries any default bound.
    let mut defaults = SeaQuery::select();
    defaults
        .expr_as(col("sp", "site_id"), Alias::new("site_id"))
        .expr_as(col("sp", "parameter_id"), Alias::new("parameter_id"))
        .expr_as(col("p", "default_warning_min"), Alias::new("warning_min"))
        .expr_as(col("p", "default_warning_max"), Alias::new("warning_max"))
        .expr_as(col("p", "default_alarm_min"), Alias::new("alarm_min"))
        .expr_as(col("p", "default_alarm_max"), Alias::new("alarm_max"))
        .expr_as(Expr::val(3), Alias::new("priority"))
        .expr_as(Expr::val("default"), Alias::new("source"))
        .from_as(Alias::new("site_parameters"), Alias::new("sp"))
        .join_as(
            JoinType::Join,
            Alias::new("parameters"),
            Alias::new("p"),
            col("p", "id").equals((Alias::new("sp"), Alias::new("parameter_id"))),
        )
        .and_where(col("sp", "is_active").eq(true))
        .cond_where(
            Condition::any()
                .add(col("p", "default_warning_min").is_not_null())
                .add(col("p", "default_warning_max").is_not_null())
                .add(col("p", "default_alarm_min").is_not_null())
                .add(col("p", "default_alarm_max").is_not_null()),
        );

    if let Some(s) = site_id {
        rows.and_where(col("sp", "site_id").eq(s));
        defaults.and_where(col("sp", "site_id").eq(s));
    }
    if let Some(pids) = param_ids {
        rows.and_where(col("sp", "parameter_id").is_in(pids.clone()));
        defaults.and_where(col("sp", "parameter_id").is_in(pids));
    }

    rows.union(UnionType::All, defaults);

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

/// Render [`resolve_thresholds_query`] to a standalone SQL string (scope values inlined, so it
/// carries no bind params) for splicing as a CTE body into raw-SQL consumers without `$N` clashes.
pub fn resolve_thresholds_sql(site_id: Option<Uuid>, param_ids: Option<Vec<Uuid>>) -> String {
    resolve_thresholds_query(site_id, param_ids).to_string(PostgresQueryBuilder)
}

/// Resolve the resolved threshold for one `(site, parameter)` slot — the per-slot wrapper over the
/// single [`resolve_thresholds_query`] definition. Returns `None` when the slot has no threshold
/// at any tier. An all-NULL row (the "Disabled" state) resolves to `Some(..)` with every bound
/// `None` and suppresses alarms — see [`ResolvedThreshold::is_disabled`].
pub async fn resolve_threshold(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
) -> Result<Option<ResolvedThreshold>, sea_orm::DbErr> {
    let (sql, values) =
        resolve_thresholds_query(Some(site_id), Some(vec![parameter_id])).build(PostgresQueryBuilder);
    let row = db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            values.0,
        ))
        .await?;

    row.map(|r| ResolvedThreshold::from_query_result(&r, ""))
        .transpose()
}
