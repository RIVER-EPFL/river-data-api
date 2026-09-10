use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{
    Alias, Condition, Expr, Func, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
    SimpleExpr, SubQueryStatement, UnionType,
};
use sea_orm::{ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, Order, Statement};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, sensor_in_scope};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::models as readings;
use crate::routes::private::sites::models as sites;

/// One reading the calibration's `[valid_from, valid_until)` window resolves.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationWindowPoint {
    pub time: DateTime<Utc>,
    pub raw_value: f64,
    #[schema(required)]
    pub calibrated_value: Option<f64>,
    pub is_flagged: bool,
}

/// The data a calibration's time window resolves, for the interactive calibration editor.
/// `points` is capped (most recent within the window) while `point_count` is the true total.
#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationWindowResponse {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    pub slope: f64,
    pub intercept: f64,
    pub valid_from: DateTime<Utc>,
    #[schema(required)]
    pub valid_until: Option<DateTime<Utc>>,
    pub point_count: i64,
    pub points: Vec<CalibrationWindowPoint>,
}

const MAX_POINTS: i64 = 2000;

/// `GET /sensor_calibrations/{id}/window`, the readings a calibration window resolves. `read_data`.
#[utoipa::path(
    get,
    path = "/api/sensor_calibrations/{id}/window",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Calibration window + resolved points", body = CalibrationWindowResponse),
        (status = 404, description = "Calibration not found"),
    ),
    tag = "sensors"
)]
pub async fn get_calibration_window(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(calibration_id): Path<Uuid>,
) -> AppResult<Json<CalibrationWindowResponse>> {
    let db = &state.db;

    let cal = super::model::Entity::find_by_id(calibration_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Calibration not found".to_string()))?;

    let sensor_id = cal.sensor_id;

    // A project-scoped key may only inspect a calibration whose sensor is deployed within its
    // project, and only sees the window's in-project readings.
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Calibration not found".to_string()));
    }
    // `site_id IN (<scope project's sites>)` when the caller is confined to a project.
    let scope_cond = scope.sql_project_array().map(|projects| {
        Expr::col(readings::Column::SiteId).in_subquery(
            SeaQuery::select()
                .column(sites::Column::Id)
                .from(sites::Entity)
                .and_where(Expr::cust_with_values("project_id = ANY($1)", [projects]))
                .take(),
        )
    });
    let super::model::Model {
        parameter_id,
        slope,
        intercept,
        valid_from,
        valid_until,
        ..
    } = cal;

    let vf: sea_orm::Value = valid_from.into();
    let vu: sea_orm::Value = match valid_until {
        Some(u) => u.into(),
        None => sea_orm::Value::ChronoDateTimeWithTimeZone(None),
    };

    // The window is [valid_from, COALESCE(valid_until, 'infinity')), and every arm below shares it.
    let in_window = || {
        let mut cond = Condition::all()
            .add(Expr::col(readings::Column::SensorId).eq(sensor_id))
            .add(Expr::col(readings::Column::Time).gte(vf.clone()))
            .add(Expr::cust_with_values(
                "time < COALESCE($1, 'infinity'::timestamptz)",
                [vu.clone()],
            ));
        if let Some(scope) = scope_cond.clone() {
            cond = cond.add(scope);
        }
        cond
    };
    let continuous = || {
        in_window()
            .add(Expr::col(readings::Column::ReplicateIndex).eq(0))
            .add(Expr::cust("measurement_type IS DISTINCT FROM 'spot'"))
    };
    let spot = || {
        in_window()
            .add(Expr::col(readings::Column::MeasurementType).eq("spot"))
            .add(Expr::col(readings::Column::WithdrawnAt).is_null())
    };

    // The count is per instant: continuous and derived rows live at replicate_index 0, so their
    // count is a plain COUNT(*) with no sort; a spot instant is the replicate group
    // `(stream_id, time)`, and the composite DISTINCT is confined to that small subset.
    let subquery = |q: SelectStatement| {
        SimpleExpr::SubQuery(None, Box::new(SubQueryStatement::SelectStatement(q)))
    };
    let count_query = SeaQuery::select()
        .expr_as(
            subquery(
                SeaQuery::select()
                    .expr(Func::count(Expr::cust("*")))
                    .from(readings::Entity)
                    .cond_where(continuous())
                    .take(),
            )
            .add(subquery(
                SeaQuery::select()
                    .expr(Expr::cust("COUNT(DISTINCT (stream_id, time))"))
                    .from(readings::Entity)
                    .cond_where(spot())
                    .take(),
            )),
            Alias::new("c"),
        )
        .take();
    let (count_sql, count_values) = count_query.build(PostgresQueryBuilder);
    let count_row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            count_sql,
            count_values,
        ))
        .await?;
    // The count query is an aggregate over a subquery, so it always returns a row; no row means no
    // calibration window, which is zero points rather than a number to guess at.
    let point_count: i64 = count_row
        .map(|r| r.try_get("", "c"))
        .transpose()?
        .unwrap_or(0);

    // Each arm carries its own LIMIT so the continuous arm keeps the index-backed early stop; the
    // outer sort then orders at most twice the cap. The spot arm collapses a replicate group to
    // its lowest unflagged replicate (a flagged-only group surfaces its flagged row: the editor
    // shows flagged points).
    let point_cols = [
        Alias::new("time"),
        Alias::new("raw_value"),
        Alias::new("calibrated_value"),
        Alias::new("is_flagged"),
    ];
    let flag = || {
        Func::coalesce([
            Expr::col(readings::Column::IsFlagged).into(),
            Expr::value(false),
        ])
    };
    let continuous_arm = SeaQuery::select()
        .column(readings::Column::Time)
        .column(readings::Column::RawValue)
        .column(readings::Column::CalibratedValue)
        .expr_as(flag(), Alias::new("is_flagged"))
        .from(readings::Entity)
        .cond_where(continuous())
        .order_by(readings::Column::Time, Order::Desc)
        .limit(MAX_POINTS.unsigned_abs())
        .take();
    // The spot arm collapses a replicate group to its lowest unflagged replicate (a flagged-only
    // group surfaces its flagged row: the editor shows flagged points).
    let spot_group = SeaQuery::select()
        .distinct_on([readings::Column::StreamId, readings::Column::Time])
        .column(readings::Column::Time)
        .column(readings::Column::RawValue)
        .column(readings::Column::CalibratedValue)
        .expr_as(flag(), Alias::new("is_flagged"))
        .from(readings::Entity)
        .cond_where(spot())
        .order_by(readings::Column::StreamId, Order::Asc)
        .order_by(readings::Column::Time, Order::Asc)
        .order_by_expr(Expr::cust("(is_flagged IS TRUE)"), Order::Asc)
        .order_by(readings::Column::ReplicateIndex, Order::Asc)
        .take();
    let spot_arm = SeaQuery::select()
        .columns(point_cols.clone())
        .from_subquery(spot_group, Alias::new("sp"))
        .order_by(Alias::new("time"), Order::Desc)
        .limit(MAX_POINTS.unsigned_abs())
        .take();
    // Each arm carries its own LIMIT so the continuous arm keeps the index-backed early stop; the
    // outer sort then orders at most twice the cap. Each is wrapped in its own derived table so
    // the union keeps those limits instead of hoisting one of them to the top.
    let wrap = |arm: SelectStatement, alias: &str| {
        SeaQuery::select()
            .columns(point_cols.clone())
            .from_subquery(arm, Alias::new(alias))
            .take()
    };
    let points_query = wrap(continuous_arm, "c")
        .union(UnionType::All, wrap(spot_arm, "s"))
        .order_by(Alias::new("time"), Order::Desc)
        .limit(MAX_POINTS.unsigned_abs())
        .take();
    let (points_sql, points_values) = points_query.build(PostgresQueryBuilder);
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            points_sql,
            points_values,
        ))
        .await?;

    let mut points = rows
        .iter()
        .map(|row| -> AppResult<CalibrationWindowPoint> {
            let row = WindowPointRow::from_query_result(row, "")?;
            Ok(CalibrationWindowPoint {
                time: row.time.with_timezone(&Utc),
                raw_value: row.raw_value,
                calibrated_value: row.calibrated_value,
                // Nothing has flagged the row, which reads as not flagged.
                is_flagged: row.is_flagged.unwrap_or(false),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;
    points.reverse(); // chronological for the scatter

    Ok(Json(CalibrationWindowResponse {
        calibration_id,
        sensor_id,
        parameter_id,
        slope,
        intercept,
        valid_from: valid_from.with_timezone(&Utc),
        valid_until: valid_until.map(|u| u.with_timezone(&Utc)),
        point_count,
        points,
    }))
}

/// One point of a calibration's window, as the two-armed query returns it.
#[derive(FromQueryResult)]
struct WindowPointRow {
    time: DateTime<chrono::FixedOffset>,
    raw_value: f64,
    calibrated_value: Option<f64>,
    is_flagged: Option<bool>,
}
