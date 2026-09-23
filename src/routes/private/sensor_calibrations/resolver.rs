//! The one answer to "which calibration covers this reading, and what does it do to the raw value".
//!
//! Every path that needs the answer, the write paths (`/ingest`, `/grab_samples`) and the
//! set-based reprocess UPDATEs, ranks the candidate curves with the SQL
//! [`pick_calibration_query`] emits. There is one ranking, so a value stored at write time and the
//! value a later reprocess would recompute are the same number by construction rather than by
//! agreement between two hand-kept implementations.
//!
//! The window is half-open, `[valid_from, COALESCE(valid_until, 'infinity'))`. Ranking, most
//! specific first:
//!
//! 1. a curve authored for the reading's own parameter,
//! 2. a parameter-bearing curve over one that names no parameter,
//! 3. the latest `valid_from`, then the highest `id` so a pair sharing an instant still resolves to
//!    one row rather than to whichever the planner reached first.
//!
//! Standard curves take no part: they live in their own table, apply only to the reading that names
//! one, and are matched by `readings.standard_curve_id`, never by window.

use chrono::{DateTime, Utc};
use sea_orm::Order;
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait as _, Func, IntoIden, JoinType, PostgresQueryBuilder, Query as SeaQuery,
    SelectStatement, TableRef,
};

use super::models as model;
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use std::collections::HashMap;
use uuid::Uuid;

use super::service::Curve;
use crate::error::AppResult;

/// A `LATERAL` subquery selecting `(id, slope, intercept)` of the one calibration covering reading
/// row `r`, where `r` exposes `time` and `parameter_id`. `sensor_expr` is whatever names the owning
/// sensor in the caller's query, a bind placeholder (`$1`) or a column (`r.sensor_id`).
///
/// Join it `LEFT JOIN LATERAL (...) cw ON true` when rows outside every window must survive the
/// join, `JOIN LATERAL` when they must not.
///
/// A retired curve is never a candidate (M146): retirement is what takes a curve out of
/// circulation, and this is the one producer of the ranking, so the predicate has one home.
#[must_use]
pub fn pick_calibration_query(sensor_expr: &str) -> SelectStatement {
    pick_calibration_query_excluding(sensor_expr, None)
}

/// [`pick_calibration_query`] with one curve held out of the candidates. The delete path is the
/// only caller: a curve on its way out must not be the answer to what covers a reading now.
#[must_use]
pub fn pick_calibration_query_excluding(
    sensor_expr: &str,
    exclude_expr: Option<&str>,
) -> SelectStatement {
    pick_calibration_query_owned(
        Expr::cust(format!("c.sensor_id = {sensor_expr}")),
        exclude_expr.map(|e| Expr::cust(format!("c.id <> {e}"))),
    )
}

/// [`pick_calibration_query`] with the owning instrument as an expression rather than a spelled
/// one, so a built statement can bind it instead of naming a placeholder the builder renumbers.
#[must_use]
pub fn pick_calibration_query_owned(owner: Expr, exclude: Option<Expr>) -> SelectStatement {
    let c = Alias::new("c");
    let mut pick = SeaQuery::select();
    pick.columns([
        (c.clone(), model::Column::Id),
        (c.clone(), model::Column::Slope),
        (c.clone(), model::Column::Intercept),
    ])
    .from_as(model::Entity, c.clone())
    .and_where(owner)
    .and_where(Expr::col((c.clone(), model::Column::RetiredAt)).is_null())
    .and_where(Expr::cust(
        "(c.parameter_id = r.parameter_id OR c.parameter_id IS NULL OR r.parameter_id IS NULL)",
    ))
    .and_where(Expr::cust("r.time >= c.valid_from"))
    .and_where(Expr::cust(
        "r.time < COALESCE(c.valid_until, 'infinity'::timestamptz)",
    ))
    .order_by_expr(
        Expr::cust("(c.parameter_id IS NOT DISTINCT FROM r.parameter_id)"),
        Order::Desc,
    )
    .order_by_expr(Expr::cust("(c.parameter_id IS NOT NULL)"), Order::Desc)
    .order_by((c.clone(), model::Column::ValidFrom), Order::Desc)
    .order_by((c.clone(), model::Column::Id), Order::Desc)
    .limit(1);
    if let Some(e) = exclude {
        pick.and_where(e);
    }
    pick.take()
}

/// One instant's resolved curve, as the timeline query returns it.
#[derive(sea_orm::FromQueryResult)]
struct ResolvedCurve {
    t: DateTime<chrono::FixedOffset>,
    cal_id: Uuid,
    slope: f64,
    intercept: f64,
}

/// Resolve the covering calibration for each of `times` on one `(sensor, parameter)` channel.
/// `parameter_id` is the reading's parameter, `None` for a reading that carries none (then any of
/// the sensor's curves may cover it, ranked as above).
///
/// One indexed query regardless of how many times are asked for. Times with no covering window are
/// absent from the map: the caller stores no `calibration_id` and leaves `calibrated_value` alone.
pub async fn resolve_for_times<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    parameter_id: Option<Uuid>,
    times: &[DateTime<Utc>],
) -> AppResult<HashMap<DateTime<Utc>, Curve>> {
    let mut out = HashMap::new();
    if times.is_empty() {
        return Ok(out);
    }

    let mut wanted: Vec<DateTime<Utc>> = times.to_vec();
    wanted.sort_unstable();
    wanted.dedup();

    let r = Alias::new("r");
    let q = Alias::new("q");
    let cw = Alias::new("cw");
    let uuid = Alias::new("uuid");
    let asked = SeaQuery::select()
        .expr_as(
            Expr::val(sensor_id).cast_as(uuid.clone()),
            Alias::new("sensor_id"),
        )
        .expr_as(
            Expr::val(parameter_id).cast_as(uuid),
            Alias::new("parameter_id"),
        )
        .expr_as(Expr::col((q.clone(), q.clone())), Alias::new("time"))
        .from(TableRef::FunctionCall(
            Func::cust(Alias::new("unnest")).arg(Expr::val(wanted)),
            q.into_iden(),
        ))
        .take();
    let (sql, values) = SeaQuery::select()
        .expr_as(Expr::col((r.clone(), Alias::new("time"))), Alias::new("t"))
        .expr_as(
            Expr::col((cw.clone(), model::Column::Id)),
            Alias::new("cal_id"),
        )
        .expr_as(
            Expr::col((cw.clone(), model::Column::Slope)),
            Alias::new("slope"),
        )
        .expr_as(
            Expr::col((cw.clone(), model::Column::Intercept)),
            Alias::new("intercept"),
        )
        .from_subquery(asked, r)
        .join_lateral(
            JoinType::InnerJoin,
            pick_calibration_query("r.sensor_id"),
            cw,
            Expr::cust("TRUE"),
        )
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;

    for row in &rows {
        let row = ResolvedCurve::from_query_result(row, "")?;
        out.insert(
            row.t.with_timezone(&Utc),
            Curve {
                id: row.cal_id,
                slope: row.slope,
                intercept: row.intercept,
            },
        );
    }
    Ok(out)
}

/// [`resolve_for_times`] over a mixed batch: readings from several sensors, or several channels of
/// one instrument, in a single call. Keyed by the request triple so a caller can look an individual
/// reading back up.
pub async fn resolve_many<C: ConnectionTrait>(
    db: &C,
    requests: &[(Uuid, Option<Uuid>, DateTime<Utc>)],
) -> AppResult<HashMap<(Uuid, Option<Uuid>, DateTime<Utc>), Curve>> {
    let mut by_channel: HashMap<(Uuid, Option<Uuid>), Vec<DateTime<Utc>>> = HashMap::new();
    for (sensor_id, parameter_id, time) in requests {
        by_channel
            .entry((*sensor_id, *parameter_id))
            .or_default()
            .push(*time);
    }

    let mut out = HashMap::new();
    for ((sensor_id, parameter_id), times) in by_channel {
        let resolved = resolve_for_times(db, sensor_id, parameter_id, &times).await?;
        for (time, calibration) in resolved {
            out.insert((sensor_id, parameter_id, time), calibration);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "tests/resolver.rs"]
mod tests;
