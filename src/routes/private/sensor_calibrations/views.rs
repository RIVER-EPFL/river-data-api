//! The two calibration surfaces a person drives: retiring a windowed curve and reading back the
//! window it covers.
//!
//! Q107's rule is that a curve which has corrected a reading is retired, never removed. Retirement
//! does what the delete did to the readings, moves each onto whichever remaining curve covers it
//! and rebuilds the value, and leaves the row, its coefficients and its provenance standing. The
//! move is recorded as one `reading_decisions` set of `curve_retire` decisions, so what changed is
//! readable per reading and the whole retirement is reversible.

use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{
    Alias, Condition, Expr, Func, PostgresQueryBuilder, Query as SeaQuery, SelectStatement,
    SimpleExpr, SubQueryStatement, UnionType,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, Order, QueryFilter,
    QueryOrder, QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{AuthContext, ProjectScope, sensor_in_scope};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::models::{self as readings, Kind, Selection, decision_set};
use crate::routes::private::readings::service::{self, NewValue, not_pinned_sql};
use crate::routes::private::sites::models as sites;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetireRequest {
    /// Why it is being taken out of circulation, recorded on the curve and on every decision.
    #[serde(default)]
    pub reason: Option<String>,
    /// Report what the retirement would do and change nothing.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetireResponse {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    #[schema(required)]
    pub retired_at: Option<DateTime<Utc>>,
    /// The decision set the move was recorded as, and what a rollback names. Absent on a dry run
    /// and on a curve no reading names.
    #[schema(required)]
    pub set_id: Option<Uuid>,
    /// Readings currently corrected by this curve.
    pub readings: i64,
    /// Of those, the ones another curve covers, which move onto it.
    pub repointed: i64,
    /// Of those, the ones no remaining curve covers, which are left uncorrected.
    pub uncorrected: i64,
    /// Readings pinned to this curve by a person: they keep it, and the curve keeps them.
    pub pinned: i64,
    pub dry_run: bool,
}

/// The one curve that would cover reading `r` once this one is gone, as a scalar expression.
/// `$1` is the curve being retired and `$2` its instrument.
fn covering_curve_sql() -> String {
    format!(
        "(SELECT p.id FROM ({pick}) p)",
        pick = super::resolver::pick_calibration_lateral_excluding("$2", Some("$1")),
    )
}

/// The readings this curve corrected that a retirement moves: its own, minus the ones a person
/// pinned to it, which keep the curve they were pinned to.
fn moved_rows_sql() -> String {
    format!(
        "r.calibration_id = $1 AND {not_pinned}",
        not_pinned = not_pinned_sql("r", Kind::CalibrationPin),
    )
}

/// What retiring a curve would move: the readings it corrected, and how they land.
#[derive(sea_orm::FromQueryResult)]
struct RetireCounts {
    readings: i64,
    repointed: i64,
    uncorrected: i64,
    pinned: i64,
}

async fn counts<C: ConnectionTrait>(
    conn: &C,
    id: Uuid,
    sensor_id: Uuid,
) -> AppResult<(i64, i64, i64, i64)> {
    // Each reading this curve corrects, with whether the retirement moves it and which curve
    // would then cover it. The two fragments carry their own binds, so the outer counts are a
    // built statement rather than a `format!` over them.
    let moved = moved_rows_sql();
    let covering = covering_curve_sql();
    let per_reading = SeaQuery::select()
        .expr_as(Expr::cust_with_values(moved, [id]), Alias::new("moved"))
        .expr_as(
            Expr::cust_with_values(covering, [id, sensor_id]),
            Alias::new("covered"),
        )
        .from_as(readings::Entity, Alias::new("r"))
        .and_where(ExprTrait::eq(
            Expr::col((Alias::new("r"), readings::Column::CalibrationId)),
            id,
        ))
        .take();
    let count_filtered = |cond: &str| Expr::cust(format!("count(*) FILTER (WHERE {cond})::bigint"));
    let query = SeaQuery::select()
        .expr_as(Expr::cust("count(*)::bigint"), Alias::new("readings"))
        .expr_as(
            count_filtered("moved AND covered IS NOT NULL"),
            Alias::new("repointed"),
        )
        .expr_as(
            count_filtered("moved AND covered IS NULL"),
            Alias::new("uncorrected"),
        )
        .expr_as(count_filtered("NOT moved"), Alias::new("pinned"))
        .from_subquery(per_reading, Alias::new("x"))
        .take();
    let (sql, values) = query.build(PostgresQueryBuilder);
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| {
            AppError::Internal("counting the curve's readings returned no row".into())
        })?;
    let counts = RetireCounts::from_query_result(&row, "")?;
    Ok((
        counts.readings,
        counts.repointed,
        counts.uncorrected,
        counts.pinned,
    ))
}

/// `POST /sensor_calibrations/{id}/retire`. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/sensor_calibrations/{id}/retire",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    request_body = RetireRequest,
    responses(
        (status = 200, description = "Retired, or reported for a dry run", body = RetireResponse),
        (status = 404, description = "No such calibration"),
        (status = 409, description = "Already retired"),
    ),
    tag = "sensors"
)]
pub async fn retire_calibration(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<RetireRequest>,
) -> AppResult<Json<RetireResponse>> {
    let actor = crate::common::actor::label(&auth);
    let origin = auth.origin();
    let (sensor_id, retired_at) = load(&state.db, id).await?;
    if retired_at.is_some() {
        return Err(AppError::Conflict(format!(
            "Calibration {id} is already retired"
        )));
    }
    let (readings, repointed, uncorrected, pinned) = counts(&state.db, id, sensor_id).await?;
    if req.dry_run {
        return Ok(Json(RetireResponse {
            calibration_id: id,
            sensor_id,
            retired_at: None,
            set_id: None,
            readings,
            repointed,
            uncorrected,
            pinned,
            dry_run: true,
        }));
    }

    let selection = Selection {
        calibration_id: Some(id),
        ..Default::default()
    };
    let set_id = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let set_id = service::open_set(
            txn,
            Kind::CurveRetire,
            &selection,
            serde_json::json!({ "retired_calibration_id": id }),
            &actor,
            req.reason.as_deref(),
        )
        .await?;
        let recorded = service::record_many(
            txn,
            Kind::CurveRetire,
            crate::routes::private::collection_events::flows::rows_matching(
                &moved_rows_sql(),
                vec![id.into(), sensor_id.into()],
            ),
            NewValue::Sql(sea_orm::sea_query::Expr::cust_with_values(
                covering_curve_sql_object(),
                [sea_orm::Value::from(id), sea_orm::Value::from(sensor_id)],
            )),
            &actor,
            req.reason.as_deref(),
            origin,
            Some(set_id),
        )
        .await?;
        service::close_set(txn, set_id, recorded.rows).await?;
        // The projection moved `calibration_id`; the value each reading serves is whatever the
        // curves it now names produce, including none at all.
        super::service::recompose_decided_rows(txn, set_id).await?;
        super::models::Entity::update_many()
            .col_expr(
                super::models::Column::RetiredAt,
                Expr::current_timestamp().into(),
            )
            .col_expr(
                super::models::Column::RetiredBy,
                Expr::value(Some(actor.clone())),
            )
            .col_expr(
                super::models::Column::RetiredReason,
                Expr::value(req.reason.clone()),
            )
            .filter(super::models::Column::Id.eq(id))
            .exec(txn)
            .await?;
        Ok(set_id)
    })
    .await?;

    // A retired curve no longer bounds its neighbours' windows, so the chain is rebuilt before the
    // slots reprocess under it.
    super::service::recompute_valid_until(&state.db, sensor_id).await?;
    crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "calibration_retire",
        Some(sensor_id),
        Some(id),
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await?;

    let retired_at = load(&state.db, id).await?.1;
    Ok(Json(RetireResponse {
        calibration_id: id,
        sensor_id,
        retired_at,
        set_id: Some(set_id),
        readings,
        repointed,
        uncorrected,
        pinned,
        dry_run: false,
    }))
}

/// The per-row assertion a retirement records: the curve that reading moves onto, `null` when
/// none covers it.
fn covering_curve_sql_object() -> String {
    format!(
        "jsonb_build_object('calibration_id', {covering})",
        covering = covering_curve_sql(),
    )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UnretireResponse {
    pub calibration_id: Uuid,
    pub sensor_id: Uuid,
    /// The decision set that was inverted, if the retirement moved any reading.
    #[schema(required)]
    pub set_id: Option<Uuid>,
    pub restored: usize,
}

/// `POST /sensor_calibrations/{id}/unretire`, the inverse: every reading returns to this curve and
/// to the value it produced, and the curve is offered again. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/sensor_calibrations/{id}/unretire",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Back in circulation", body = UnretireResponse),
        (status = 404, description = "No such calibration"),
        (status = 409, description = "Not retired"),
    ),
    tag = "sensors"
)]
pub async fn unretire_calibration(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<UnretireResponse>> {
    let actor = crate::common::actor::label(&auth);
    let (sensor_id, retired_at) = load(&state.db, id).await?;
    if retired_at.is_none() {
        return Err(AppError::Conflict(format!(
            "Calibration {id} is not retired"
        )));
    }
    let set = latest_set(&state.db, id).await?;
    let restored = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let restored = if let Some(set_id) = set {
            let (n, _) =
                service::rollback_set(txn, set_id, &actor, Some("curve unretired")).await?;
            super::service::recompose_decided_rows(txn, set_id).await?;
            n
        } else {
            0
        };
        super::models::Entity::update_many()
            .col_expr(
                super::models::Column::RetiredAt,
                Expr::value(None::<chrono::DateTime<Utc>>),
            )
            .col_expr(
                super::models::Column::RetiredBy,
                Expr::value(None::<String>),
            )
            .col_expr(
                super::models::Column::RetiredReason,
                Expr::value(None::<String>),
            )
            .filter(super::models::Column::Id.eq(id))
            .exec(txn)
            .await?;
        Ok(restored)
    })
    .await?;

    super::service::recompute_valid_until(&state.db, sensor_id).await?;
    crate::routes::private::reprocessing_jobs::worker::enqueue(
        &state.db,
        "calibration_unretire",
        Some(sensor_id),
        Some(id),
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await?;

    Ok(Json(UnretireResponse {
        calibration_id: id,
        sensor_id,
        set_id: set,
        restored,
    }))
}

/// A curve's instrument and whether it has been retired.
async fn load<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<(Uuid, Option<DateTime<Utc>>)> {
    super::models::Entity::find_by_id(id)
        .select_only()
        .column(super::models::Column::SensorId)
        .column(super::models::Column::RetiredAt)
        .into_tuple::<(Uuid, Option<DateTime<Utc>>)>()
        .one(conn)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Calibration {id} not found")))
}

/// The retirement's own decision set: the newest one this curve opened that has not been rolled
/// back.
async fn latest_set<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<Option<Uuid>> {
    Ok(decision_set::Entity::find()
        .filter(decision_set::Column::Kind.eq("curve_retire"))
        .filter(decision_set::Column::RolledBackAt.is_null())
        // The retired calibration is a field of the set's `new` blob, not a column of its own.
        .filter(Expr::cust_with_values(
            "new ->> 'retired_calibration_id' = $1",
            [id.to_string()],
        ))
        .order_by_desc(decision_set::Column::At)
        .select_only()
        .column(decision_set::Column::Id)
        .into_tuple::<Uuid>()
        .one(conn)
        .await?)
}

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

    let cal = super::models::Entity::find_by_id(calibration_id)
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
    let super::models::Model {
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
