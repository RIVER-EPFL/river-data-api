//! The one answer to "which calibration covers this reading, and what does it do to the raw value".
//!
//! Every path that needs the answer, the write paths (`/ingest`, `/grab_samples`, stream import)
//! and the set-based reprocess UPDATEs, ranks the candidate curves with the SQL
//! [`pick_calibration_lateral`] emits. There is one ranking, so a value stored at write time and the
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
    Alias, Condition, Expr, ExprTrait as _, IntoIden, IntoTableRef, JoinType, PostgresQueryBuilder,
    Query as SeaQuery, QueryStatementWriter, SelectStatement, TableRef, UpdateStatement,
};

use super::model;
use crate::routes::private::readings::models as readings;
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use std::collections::HashMap;
use uuid::Uuid;

use super::service::{Curve, calibrated_value_sql};
use crate::common::bulk_write;
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
        exclude_expr,
    )
}

/// [`pick_calibration_query`] with the owning instrument as an expression rather than a spelled
/// one, so a built statement can bind it instead of naming a placeholder the builder renumbers.
#[must_use]
pub fn pick_calibration_query_owned(owner: Expr, exclude_expr: Option<&str>) -> SelectStatement {
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
    if let Some(e) = exclude_expr {
        pick.and_where(Expr::cust(format!("c.id <> {e}")));
    }
    pick.take()
}

/// [`pick_calibration_query`] rendered, for a caller whose own statement is still SQL text.
#[must_use]
pub fn pick_calibration_lateral(sensor_expr: &str) -> String {
    pick_calibration_query(sensor_expr).to_string(PostgresQueryBuilder)
}

/// [`pick_calibration_query_excluding`] rendered, for the same reason.
#[must_use]
pub fn pick_calibration_lateral_excluding(sensor_expr: &str, exclude_expr: Option<&str>) -> String {
    pick_calibration_query_excluding(sensor_expr, exclude_expr).to_string(PostgresQueryBuilder)
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

    // Timestamps travel as RFC3339 text and are cast server-side: sea-query's timestamptz array
    // binding rejects chrono values here.
    let mut wanted: Vec<String> = times.iter().map(DateTime::to_rfc3339).collect();
    wanted.sort_unstable();
    wanted.dedup();

    let sql = format!(
        r"SELECT r.time AS t, cw.id AS cal_id, cw.slope AS slope, cw.intercept AS intercept
          FROM (
              SELECT $1::uuid AS sensor_id, $2::uuid AS parameter_id, q.time AS time
              FROM unnest($3::text[]::timestamptz[]) AS q(time)
          ) r
          JOIN LATERAL ({pick}) cw ON true",
        pick = pick_calibration_lateral("r.sensor_id")
    );

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            [sensor_id.into(), parameter_id.into(), wanted.into()],
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

/// Attribute a stream's readings to `sensor_id`, by window, and report how many that moved.
///
/// `POST /streams/{id}/import` adopts a stream's instrument into inventory; this is the readings
/// half. Each reading takes the curve whose window covers its own time, not the sensor's newest
/// curve, and is corrected with that curve's coefficients. No curve is created for the history being
/// adopted: an instrument with none, or with none covering this stretch of time, is an ordinary
/// state, and those readings simply keep what they had until a reprocess resolves them.
///
/// Since registration attaches an instrument and the insert trigger stamps every row from its
/// stream, an unowned reading is the exception rather than the rule, so the row set is the stream's
/// own rows and the count is what actually changed: a row newly owned, or one whose curve the
/// window resolved differently. A second import reports nothing.
///
/// Spot rows take the owner and nothing else: a grab is corrected at entry (`/grab_samples`),
/// against the base curve resolved then and the standard curve the operator picked, and re-stamping
/// a windowed curve here would claim provenance the served value does not carry.
pub async fn attribute_stream_by_window<C>(
    db: &C,
    stream_id: Uuid,
    sensor_id: Uuid,
) -> AppResult<u64>
where
    C: ConnectionTrait + sea_orm::TransactionTrait,
{
    let touched =
        bulk_write::guarded_mutation(db, attribute_by_window_query(stream_id, sensor_id)).await?;
    Ok(touched.rows)
}

/// The statement `attribute_stream_by_window` runs, built so its row set and its change predicate
/// can be read without a database.
fn attribute_by_window_query(stream_id: Uuid, sensor_id: Uuid) -> UpdateStatement {
    let tgt = Alias::new("tgt");
    let r = Alias::new("r");
    let cw = Alias::new("cw");
    let windowed = super::service::calibration_derivable("tgt");
    let value = calibrated_value_sql("tgt.raw_value", "picked.slope", "picked.intercept");

    // One row per reading of the stream, with the curve whose window covers its own time.
    let picked = SeaQuery::select()
        .expr_as(Expr::cust("r.stream_id"), Alias::new("p_stream_id"))
        .expr_as(Expr::cust("r.time"), Alias::new("p_time"))
        .expr_as(
            Expr::cust("r.replicate_index"),
            Alias::new("p_replicate_index"),
        )
        .expr_as(Expr::cust("cw.id"), Alias::new("cal_id"))
        .expr(Expr::cust("cw.slope"))
        .expr(Expr::cust("cw.intercept"))
        .from_as(readings::Entity, r.clone())
        .join_lateral(
            JoinType::LeftJoin,
            pick_calibration_query_owned(
                Expr::cust_with_values("c.sensor_id = $1", [sensor_id]),
                None,
            ),
            cw.clone(),
            Condition::all().add(Expr::cust("true")),
        )
        .and_where(Expr::cust_with_values("r.stream_id = $1", [stream_id]))
        .and_where(Expr::cust_with_values(
            "(r.sensor_id IS NULL OR r.sensor_id = $1)",
            [sensor_id],
        ))
        .take();

    SeaQuery::update()
        .table(readings::Entity.into_table_ref().alias(tgt))
        .value(Alias::new("sensor_id"), sensor_id)
        .value(
            Alias::new("calibration_id"),
            Expr::cust(format!(
                "CASE WHEN {windowed} THEN COALESCE(picked.cal_id, tgt.calibration_id) \
                 ELSE tgt.calibration_id END"
            )),
        )
        .value(
            Alias::new("calibrated_value"),
            Expr::cust(format!(
                "CASE WHEN picked.cal_id IS NOT NULL AND {windowed} THEN {value} \
                 ELSE tgt.calibrated_value END"
            )),
        )
        .from(TableRef::SubQuery(
            Box::new(picked),
            Alias::new("picked").into_iden(),
        ))
        .and_where(Expr::cust("tgt.stream_id = picked.p_stream_id"))
        .and_where(Expr::cust("tgt.time = picked.p_time"))
        .and_where(Expr::cust("tgt.replicate_index = picked.p_replicate_index"))
        .and_where(Expr::cust_with_values(
            format!(
                "(tgt.sensor_id IS DISTINCT FROM $1 \
                  OR (picked.cal_id IS NOT NULL AND {windowed} \
                      AND tgt.calibration_id IS DISTINCT FROM picked.cal_id))"
            ),
            [sensor_id],
        ))
        .take()
}

#[cfg(test)]
#[path = "tests/resolver.rs"]
mod tests;
