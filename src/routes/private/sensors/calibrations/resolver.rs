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
//! 2. a parameter-bearing curve over the parameter-less identity,
//! 3. the latest `valid_from`, then the highest `id` so a pair sharing an instant still resolves to
//!    one row rather than to whichever the planner reached first.
//!
//! Standard curves take no part: they live in their own table, apply only to the reading that names
//! one, and are matched by `readings.standard_curve_id`, never by window.

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, Statement};
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
#[must_use]
pub fn pick_calibration_lateral(sensor_expr: &str) -> String {
    pick_calibration_lateral_excluding(sensor_expr, None)
}

/// [`pick_calibration_lateral`] with one curve held out of the candidates. The delete path is the
/// only caller: a curve on its way out must not be the answer to what covers a reading now.
#[must_use]
pub fn pick_calibration_lateral_excluding(sensor_expr: &str, exclude_expr: Option<&str>) -> String {
    let exclude =
        exclude_expr.map_or_else(String::new, |e| format!("\n            AND c.id <> {e}"));
    format!(
        r"SELECT c.id, c.slope, c.intercept
          FROM sensor_calibrations c
          WHERE c.sensor_id = {sensor_expr}{exclude}
            AND (c.parameter_id = r.parameter_id OR c.parameter_id IS NULL OR r.parameter_id IS NULL)
            AND r.time >= c.valid_from
            AND r.time < COALESCE(c.valid_until, 'infinity'::timestamptz)
          ORDER BY (c.parameter_id IS NOT DISTINCT FROM r.parameter_id) DESC,
                   (c.parameter_id IS NOT NULL) DESC,
                   c.valid_from DESC,
                   c.id DESC
          LIMIT 1"
    )
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
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            [sensor_id.into(), parameter_id.into(), wanted.into()],
        ))
        .await?;

    for row in &rows {
        let t: DateTime<chrono::FixedOffset> = row.try_get("", "t")?;
        out.insert(
            t.with_timezone(&Utc),
            Curve {
                id: row.try_get("", "cal_id")?,
                slope: row.try_get("", "slope")?,
                intercept: row.try_get("", "intercept")?,
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

/// Attribute a stream's un-owned readings to `sensor_id`, by window.
///
/// `POST /streams/{id}/import` adopts a stream's instrument into inventory; this is the readings
/// half. Each reading takes the curve whose window covers its own time, not the sensor's newest
/// curve, and is corrected with that curve's coefficients. Rows outside every window keep whatever
/// calibration they had: reprocess would not re-stamp them either.
///
/// `sensor_id IS NULL` is the idempotence key, so a second import reports nothing attributed.
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
    // The identity curve the import mints must cover the history it is about to attribute, so it is
    // backdated to the stream's own earliest reading. `MIN(readings.time) WHERE sensor_id = $1` is
    // still NULL at this point, the readings are exactly what this call is about to own.
    let earliest = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT MIN(time) AS earliest FROM readings WHERE stream_id = $1",
            [stream_id.into()],
        ))
        .await?
        .and_then(|r| {
            r.try_get::<DateTime<chrono::FixedOffset>>("", "earliest")
                .ok()
        })
        .map(|t| t.with_timezone(&Utc));

    if let Some(earliest) = earliest {
        super::service::ensure_identity_covers(db, sensor_id, earliest).await?;
    }

    let sql = format!(
        r"UPDATE readings tgt
          SET sensor_id = $2,
              calibration_id = CASE
                  WHEN tgt.measurement_type IS DISTINCT FROM 'spot'
                      THEN COALESCE(picked.cal_id, tgt.calibration_id)
                  ELSE tgt.calibration_id
              END,
              calibrated_value = CASE
                  WHEN picked.cal_id IS NOT NULL AND tgt.measurement_type IS DISTINCT FROM 'spot'
                      THEN {value}
                  ELSE tgt.calibrated_value
              END
          FROM (
              SELECT r.stream_id AS p_stream_id, r.time AS p_time,
                     r.replicate_index AS p_replicate_index,
                     cw.id AS cal_id, cw.slope, cw.intercept
              FROM readings r
              LEFT JOIN LATERAL ({pick}) cw ON true
              WHERE r.stream_id = $1 AND r.sensor_id IS NULL
          ) picked
          WHERE tgt.stream_id = picked.p_stream_id
            AND tgt.time = picked.p_time
            AND tgt.replicate_index = picked.p_replicate_index",
        value = calibrated_value_sql("tgt.raw_value", "picked.slope", "picked.intercept"),
        pick = pick_calibration_lateral("$2")
    );

    let touched = bulk_write::guarded_mutation(
        db,
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            [stream_id.into(), sensor_id.into()],
        ),
    )
    .await?;
    Ok(touched.rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ranking_is_one_expression_parameterised_only_by_the_sensor() {
        let by_bind = pick_calibration_lateral("$1");
        let by_column = pick_calibration_lateral("r.sensor_id");
        assert_eq!(
            by_bind.replace("c.sensor_id = $1", "c.sensor_id = r.sensor_id"),
            by_column,
            "the two call shapes differ only in what names the sensor"
        );
    }

    #[test]
    fn the_ranking_is_deterministic_for_curves_sharing_a_valid_from() {
        let sql = pick_calibration_lateral("$1");
        assert!(
            sql.contains("c.valid_from DESC"),
            "recency ranks first: {sql}"
        );
        assert!(
            sql.contains("c.id DESC"),
            "and a tie on valid_from still resolves to one row: {sql}"
        );
    }

    #[test]
    fn the_window_is_half_open() {
        let sql = pick_calibration_lateral("$1");
        assert!(sql.contains("r.time >= c.valid_from"), "{sql}");
        assert!(
            sql.contains("r.time < COALESCE(c.valid_until, 'infinity'::timestamptz)"),
            "{sql}"
        );
    }

    #[test]
    fn applying_a_resolved_curve_is_slope_times_raw_plus_intercept() {
        let curve = Curve {
            id: Uuid::nil(),
            slope: 2.0,
            intercept: 5.0,
        };
        assert!((curve.apply(10.0) - 25.0).abs() < f64::EPSILON);
        let identity = Curve {
            id: Uuid::nil(),
            slope: 1.0,
            intercept: 0.0,
        };
        assert!((identity.apply(10.0) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_sql_and_rust_forms_name_the_same_operands_in_the_same_order() {
        assert_eq!(
            calibrated_value_sql("tgt.raw_value", "picked.slope", "picked.intercept"),
            "picked.slope * tgt.raw_value + picked.intercept",
            "the set-based writers correct a row the way `apply_calibration` does"
        );
    }

    #[test]
    fn a_standard_curve_corrects_what_the_base_calibration_produced() {
        use super::super::service::apply_curves;
        let base = Curve {
            id: Uuid::nil(),
            slope: 2.0,
            intercept: 5.0,
        };
        let standard = Curve {
            id: Uuid::nil(),
            slope: 10.0,
            intercept: 1.0,
        };
        assert!((apply_curves(10.0, Some(base), Some(standard)) - 251.0).abs() < f64::EPSILON);
        assert!((apply_curves(10.0, Some(base), None) - 25.0).abs() < f64::EPSILON);
        assert!((apply_curves(10.0, None, Some(standard)) - 101.0).abs() < f64::EPSILON);
        assert!((apply_curves(10.0, None, None) - 10.0).abs() < f64::EPSILON);
    }
}
