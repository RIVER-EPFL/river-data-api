//! The two calibration surfaces a person drives: retiring a windowed curve and reading back the
//! window it covers.
//!
//! Q107's rule is that a curve which has corrected a reading is retired, never removed. Retirement
//! does what the delete did to the readings, moves each onto whichever remaining curve covers it
//! and rebuilds the value, and leaves the row, its coefficients and its provenance standing. The
//! move is recorded as one `reading_decisions` set of `curve_retire` decisions, so what changed is
//! readable per reading and the whole retirement is reversible.

use std::collections::HashSet;

use axum::Json;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use chrono::DateTime;
use chrono::Utc;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::EntityTrait;
use sea_orm::ExprTrait;
use sea_orm::FromQueryResult;
use sea_orm::Order;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::Statement;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Condition;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::Func;
use sea_orm::sea_query::JoinType;
use sea_orm::sea_query::PostgresQueryBuilder;
use sea_orm::sea_query::Query as SeaQuery;
use sea_orm::sea_query::SelectStatement;
use sea_orm::sea_query::SimpleExpr;
use sea_orm::sea_query::SubQueryStatement;
use sea_orm::sea_query::UnionType;
use serde::Deserialize;
use serde::Serialize;
use utoipa::IntoParams;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::AuthContext;
use crate::common::middleware::DenyScoped;
use crate::common::middleware::ProjectScope;
use crate::common::middleware::sensor_in_scope;
use crate::common::scope::Unowned;
use crate::common::scope::confine_target;
use crate::common::scope::project_filter;
use crate::common::scope::project_of_sensor;
use crate::common::scope::require_instrument_in_scope;
use crate::common::scope::require_named_target;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::data_streams;
use crate::routes::private::readings;
use crate::routes::private::readings::models::Kind;
use crate::routes::private::readings::models::Selection;
use crate::routes::private::readings::models::decision_set;
use crate::routes::private::readings::service;
use crate::routes::private::readings::service::NewValue;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_deployments;
use crate::routes::private::sites;
use crate::routes::private::standard_curves;

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

/// The one curve that would cover reading `r` once `retiring` is gone, as a scalar expression.
fn covering_curve(retiring: Uuid, sensor_id: Uuid) -> Expr {
    let c = Alias::new("c");
    let p = Alias::new("p");
    Expr::from(
        SeaQuery::select()
            .column((p.clone(), super::models::Column::Id))
            .from_subquery(
                super::resolver::pick_calibration_query_owned(
                    Expr::col((c.clone(), super::models::Column::SensorId)).eq(sensor_id),
                    Some(Expr::col((c, super::models::Column::Id)).ne(retiring)),
                ),
                p,
            )
            .take(),
    )
}

/// The readings this curve corrected that a retirement moves: its own, minus the ones a person
/// pinned to it, which keep the curve they were pinned to.
fn moved_rows(retiring: Uuid) -> Expr {
    Expr::col((Alias::new("r"), readings::Column::CalibrationId))
        .eq(retiring)
        .and(crate::routes::private::readings::service::not_pinned(
            "r",
            Kind::CalibrationPin,
        ))
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
    let per_reading = SeaQuery::select()
        .expr_as(moved_rows(id), Alias::new("moved"))
        .expr_as(covering_curve(id, sensor_id), Alias::new("covered"))
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
        (status = 403, description = "The calibration's instrument is deployed outside the caller's projects"),
        (status = 404, description = "No such calibration"),
        (status = 409, description = "Already retired"),
    ),
    tag = "sensors"
)]
pub async fn retire_calibration(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    Json(req): Json<RetireRequest>,
) -> AppResult<Json<RetireResponse>> {
    let actor = crate::common::actor::label(&auth);
    let origin = auth.origin();
    let (sensor_id, retired_at) = load(&state.db, id).await?;
    require_instrument_in_scope(&state.db, &scope, sensor_id).await?;
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
            Condition::all().add(moved_rows(id)),
            NewValue::Sql(covering_curve_object(id, sensor_id)),
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
            .col_expr(super::models::Column::RetiredAt, Expr::current_timestamp())
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
    crate::routes::private::reprocessing_jobs::service::enqueue(
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
fn covering_curve_object(retiring: Uuid, sensor_id: Uuid) -> Expr {
    Expr::from(Func::cust(Alias::new("jsonb_build_object")).args([
        Expr::val("calibration_id"),
        covering_curve(retiring, sensor_id),
    ]))
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
        (status = 403, description = "The calibration's instrument is deployed outside the caller's projects"),
        (status = 404, description = "No such calibration"),
        (status = 409, description = "Not retired"),
    ),
    tag = "sensors"
)]
pub async fn unretire_calibration(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<UnretireResponse>> {
    let actor = crate::common::actor::label(&auth);
    let (sensor_id, retired_at) = load(&state.db, id).await?;
    require_instrument_in_scope(&state.db, &scope, sensor_id).await?;
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
    crate::routes::private::reprocessing_jobs::service::enqueue(
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
    let flag = || Func::coalesce([Expr::col(readings::Column::IsFlagged), Expr::value(false)]);
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

#[derive(FromQueryResult)]
struct CandidateRow {
    sensor_id: Uuid,
    uncalibrated_count: i64,
    target_from: chrono::DateTime<chrono::FixedOffset>,
}

#[derive(FromQueryResult)]
struct ForeignCurveRow {
    sensor_id: Option<Uuid>,
    standard_curve_id: Uuid,
    curve_sensor_id: Uuid,
    curve_name: Option<String>,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    n: i64,
    first_time: chrono::DateTime<chrono::FixedOffset>,
    last_time: chrono::DateTime<chrono::FixedOffset>,
}

// ---------------------------------------------------------------------------
// Calibration backfill
// ---------------------------------------------------------------------------

// A reading no curve covers is not an anomaly: an instrument nobody has calibrated yet, and a gap
// between two calibration campaigns, are ordinary states, and the readings in them are served raw.
// What these two queries surface is the narrower set of rows the stored state cannot explain.
//
// Both read the readings hypertable without an index that fits them. `calibration_id IS NULL` is
// not served by `idx_readings_calibration_id`, which is partial on `IS NOT NULL`, and the
// orphaned-correction predicate compares two columns of the same row, which no index can answer.
// Each therefore reads every reading in its window, and since the auto-minted identity curves were
// retired `calibration_id IS NULL` is the ordinary state of an uncorrected reading rather than a
// rarity, so the rows that survive the predicate are many. A time floor is what holds that cost
// still while the hypertable grows: it is the one bound TimescaleDB can turn into chunk exclusion,
// so the chunks below it are never opened at all.

/// How much of the readings history the anomaly report reads when the caller names no floor.
///
/// Measured back from the newest reading rather than from `now()`: an installation whose ingestion
/// has stalled would otherwise report on an empty window and read as clean.
const CANDIDATE_SCAN_DAYS: i64 = 90;

/// The default floor: [`CANDIDATE_SCAN_DAYS`] before the newest reading in the database, or `None`
/// when there are no readings, where a floor would make no difference.
///
/// The newest reading is taken across the whole table rather than the caller's projects, so two
/// callers reading the same report read the same window.
async fn default_scan_floor(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<Option<chrono::DateTime<chrono::Utc>>> {
    // The stream ingest cursors carry the newest instant without touching the hypertable: an
    // unbounded MAX(time) over readings pays a planning cost proportional to the chunk count.
    // A batch-written reading newer than every cursor at most shifts the floor slightly later,
    // which the `since` parameter can always widen past. The unbounded probe remains only as
    // the fallback for a database with readings but no cursors at all.
    let mut newest: Option<chrono::DateTime<chrono::FixedOffset>> =
        crate::routes::private::data_streams::Entity::find()
            .select_only()
            .column_as(data_streams::Column::LastDataTime.max(), "newest")
            .into_tuple::<Option<chrono::DateTime<chrono::FixedOffset>>>()
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .flatten();
    if newest.is_none() {
        newest = readings::Entity::find()
            .select_only()
            .column_as(readings::Column::Time.max(), "newest")
            .into_tuple::<Option<chrono::DateTime<chrono::FixedOffset>>>()
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .flatten();
    }
    Ok(newest.map(|t| t.with_timezone(&chrono::Utc) - chrono::Duration::days(CANDIDATE_SCAN_DAYS)))
}

/// The caller's projects, reached through the sensor's deployments: an instrument deployed
/// nowhere resolves to no project and does not appear in a restricted caller's enumeration.
fn deployed_in_scope(scope: &AccessScope) -> Option<Expr> {
    let confine = project_filter(scope, (Alias::new("s"), sites::Column::ProjectId))?;
    Some(Expr::exists(
        SeaQuery::select()
            .expr(Expr::val(1))
            .from_as(sensor_deployments::Entity, Alias::new("d"))
            .join_as(
                JoinType::InnerJoin,
                sites::Entity,
                Alias::new("s"),
                Expr::col((Alias::new("s"), sites::Column::Id))
                    .equals((Alias::new("d"), sensor_deployments::Column::SiteId)),
            )
            .and_where(
                Expr::col((Alias::new("d"), sensor_deployments::Column::SensorId))
                    .equals((Alias::new("r"), readings::Column::SensorId)),
            )
            .and_where(confine)
            .to_owned(),
    ))
}

/// The caller's projects, reached through the reading's own site.
fn site_in_scope(scope: &AccessScope) -> Option<Expr> {
    let confine = project_filter(scope, (Alias::new("s"), sites::Column::ProjectId))?;
    Some(Expr::exists(
        SeaQuery::select()
            .expr(Expr::val(1))
            .from_as(sites::Entity, Alias::new("s"))
            .and_where(
                Expr::col((Alias::new("s"), sites::Column::Id))
                    .equals((Alias::new("r"), readings::Column::SiteId)),
            )
            .and_where(confine)
            .to_owned(),
    ))
}

/// The floor a scan reads from, or no condition at all for an unbounded scan.
fn scan_floor(since: Option<chrono::DateTime<chrono::Utc>>) -> Condition {
    match since {
        Some(t) => {
            Condition::all().add(Expr::col((Alias::new("r"), readings::Column::Time)).gte(t))
        }
        None => Condition::all(),
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct CalibrationCandidatesQuery {
    /// Read only readings at or after this instant (ISO 8601). Defaults to 90 days before the
    /// newest reading; pass an earlier instant to widen the report, which costs a proportionally
    /// longer read of the hypertable. Whatever is used comes back as `scanned_from`.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct CalibrationBackfillCandidate {
    pub sensor_id: Uuid,
    /// Readings whose time falls inside one of this sensor's calibration windows, yet which carry
    /// no `calibration_id`. A reprocess resolves every one of them.
    pub uncalibrated_count: i64,
    pub target_from: chrono::DateTime<chrono::Utc>,
    #[schema(required)]
    pub earliest_calibration_from: Option<chrono::DateTime<chrono::Utc>>,
}

/// Readings carrying a correction no curve accounts for: neither a calibration nor a standard
/// curve is named, yet `calibrated_value` differs from `raw_value`. The stored number is somebody's
/// measurement and this code cannot know how it was produced.
///
/// A recomposition driven by the row's own curves leaves them standing
/// (`service::orphaned_correction_rows`, the shared definition this query uses). A calibration
/// window that covers one is another matter: the reprocess recomputes it from that curve and
/// records the move in `reading_decisions`, so the row leaves this report as soon as a curve can
/// account for it (Q114).
#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct OrphanedCorrection {
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    pub count: i64,
    pub first_time: chrono::DateTime<chrono::Utc>,
    pub last_time: chrono::DateTime<chrono::Utc>,
}

/// Readings corrected by a curve their own instrument does not own, grouped by the pair.
#[derive(Debug, Serialize, ToSchema)]
pub struct ForeignCurveUse {
    /// The instrument the readings name.
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    /// The curve they are corrected by, which belongs to `curve_sensor_id`.
    pub standard_curve_id: Uuid,
    pub curve_sensor_id: Uuid,
    #[schema(required)]
    pub curve_name: Option<String>,
    #[schema(required)]
    pub site_id: Option<Uuid>,
    #[schema(required)]
    pub parameter_id: Option<Uuid>,
    pub count: i64,
    pub first_time: chrono::DateTime<chrono::Utc>,
    pub last_time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CalibrationBackfillCandidatesResponse {
    /// Readings corrected by a curve their own instrument does not own. Report-only.
    pub foreign_curve_uses: Vec<ForeignCurveUse>,
    pub total_foreign_curve_uses: i64,
    pub candidates: Vec<CalibrationBackfillCandidate>,
    pub total_candidates: usize,
    pub total_uncalibrated: i64,
    pub orphaned_corrections: Vec<OrphanedCorrection>,
    pub total_orphaned_corrections: i64,
    /// The earliest reading this report read. Every count in it is over `[scanned_from, ∞)` alone,
    /// so it is a floor and not a total: an anomaly in older data is not absent, it was not looked
    /// at. Widen the window with `since`. `null` means the whole history was read, which is also
    /// what an empty database reports.
    #[schema(required)]
    pub scanned_from: Option<chrono::DateTime<chrono::Utc>>,
}

/// Sensors carrying readings a calibration window covers but whose `calibration_id` was never
/// stamped. Repairable: `backfill_calibrations` reprocesses them against the windows that already
/// exist, and nothing is created.
///
/// Confined to the caller's projects by the sensor's deployments, the same rule the `sensors` CRUD
/// read applies. A sensor never deployed anywhere resolves to no project and so does not appear in
/// a restricted caller's enumeration, while `backfill_calibrations` still accepts it by name: an
/// instrument in inventory is nobody's project, and the enumeration is what must not leak.
///
/// `since` bounds the read, and with it the counts: `None` reads the whole history, which is what
/// `backfill_calibrations` asks for because the sensors it selects have to be all of them.
async fn fetch_calibration_candidates(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<CalibrationBackfillCandidate>> {
    use sea_orm::{ConnectionTrait, Statement};

    let r = Alias::new("r");
    let cw = Alias::new("cw");
    let mut scanned = scan_floor(since)
        .add(Expr::col((r.clone(), readings::Column::SensorId)).is_not_null())
        .add(Expr::col((r.clone(), readings::Column::CalibrationId)).is_null())
        .add(Expr::col((cw.clone(), Alias::new("id"))).is_not_null())
        .add(crate::routes::private::sensor_calibrations::service::window_resolved_rows("r"));
    if let Some(predicate) = deployed_in_scope(scope) {
        scanned = scanned.add(predicate);
    }
    // The lateral is the same window pick the reprocess engine runs, so `cw.id IS NOT NULL` means
    // exactly "a reprocess would stamp a curve here". Grabs resolve their curves by hand at entry
    // and are never windowed, hence `window_resolved_rows`. It runs once per row the scan keeps, so
    // the floor is what decides how often: every other predicate here is a filter, not a lookup.
    let pick = crate::routes::private::sensor_calibrations::resolver::pick_calibration_query(
        "r.sensor_id",
    );
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::SensorId))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("uncalibrated_count"))
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("target_from"),
        )
        .from_as(readings::Entity, r.clone())
        .join_lateral(
            JoinType::LeftJoin,
            pick,
            cw.clone(),
            Condition::all().add(Expr::cust("true")),
        )
        .cond_where(scanned)
        .add_group_by([Expr::col((r.clone(), readings::Column::SensorId))])
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    let mut candidates = Vec::with_capacity(rows.len());
    for row in &rows {
        let CandidateRow {
            sensor_id,
            uncalibrated_count,
            target_from,
        } = CandidateRow::from_query_result(row, "")?;

        let earliest_calibration_from = sensor_calibrations::Entity::find()
            .select_only()
            .column(sensor_calibrations::Column::ValidFrom)
            .filter(sensor_calibrations::Column::SensorId.eq(sensor_id))
            .order_by_asc(sensor_calibrations::Column::ValidFrom)
            .into_tuple::<chrono::DateTime<chrono::FixedOffset>>()
            .one(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
            .map(|vf| vf.with_timezone(&chrono::Utc));

        candidates.push(CalibrationBackfillCandidate {
            sensor_id,
            uncalibrated_count,
            target_from: target_from.with_timezone(&chrono::Utc),
            earliest_calibration_from,
        });
    }

    Ok(candidates)
}

/// Readings holding a correction with no curve behind it. Grouped by `(sensor, site, parameter)`
/// so an operator can see where they came from.
///
/// Derived readings are excluded by definition: a computed quantity is not an instrument
/// measurement plus a correction, so it names no curve on purpose. Confined to the caller's
/// projects through the reading's own site, which also drops site-less rows from a restricted
/// caller's enumeration.
///
/// `since` bounds the read, and with it the counts. This half has no selective predicate at all,
/// so it reads its whole window whether or not anything is wrong; the floor is the only thing
/// keeping that off the rest of the history.
/// Readings whose standard curve belongs to another instrument. Read-only: which side is wrong is
/// a judgement (the reading was split, or the curve was), and the repair is the pin's own
/// `curves` choice.
async fn fetch_foreign_curve_uses(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<ForeignCurveUse>> {
    use sea_orm::{ConnectionTrait, Statement};

    let r = Alias::new("r");
    let sc = Alias::new("sc");
    let mut scanned = scan_floor(since).add(Expr::cust(
        crate::routes::private::sensor_calibrations::service::foreign_curve_rows("r", "sc"),
    ));
    if let Some(predicate) = site_in_scope(scope) {
        scanned = scanned.add(predicate);
    }
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::SensorId))
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::Id)),
            Alias::new("standard_curve_id"),
        )
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::SensorId)),
            Alias::new("curve_sensor_id"),
        )
        .expr_as(
            Expr::col((sc.clone(), standard_curves::Column::Name)),
            Alias::new("curve_name"),
        )
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("first_time"),
        )
        .expr_as(
            Func::max(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("last_time"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            standard_curves::Entity,
            sc.clone(),
            Expr::col((sc.clone(), standard_curves::Column::Id))
                .equals((r.clone(), readings::Column::StandardCurveId)),
        )
        .cond_where(scanned)
        .add_group_by([
            Expr::col((r.clone(), readings::Column::SensorId)),
            Expr::col((sc.clone(), standard_curves::Column::Id)),
            Expr::col((sc.clone(), standard_curves::Column::SensorId)),
            Expr::col((sc.clone(), standard_curves::Column::Name)),
            Expr::col((r.clone(), readings::Column::SiteId)),
            Expr::col((r.clone(), readings::Column::ParameterId)),
        ])
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|r| {
            let row = ForeignCurveRow::from_query_result(r, "")?;
            Ok(ForeignCurveUse {
                sensor_id: row.sensor_id,
                standard_curve_id: row.standard_curve_id,
                curve_sensor_id: row.curve_sensor_id,
                curve_name: row.curve_name,
                site_id: row.site_id,
                parameter_id: row.parameter_id,
                count: row.n,
                first_time: row.first_time.with_timezone(&chrono::Utc),
                last_time: row.last_time.with_timezone(&chrono::Utc),
            })
        })
        .collect()
}

async fn fetch_orphaned_corrections(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<Vec<OrphanedCorrection>> {
    use sea_orm::{ConnectionTrait, Statement};

    let r = Alias::new("r");
    let mut scanned = scan_floor(since)
        .add(crate::routes::private::sensor_calibrations::service::orphaned_correction_rows("r"))
        .add(Expr::cust("r.measurement_type IS DISTINCT FROM 'derived'"));
    if let Some(predicate) = site_in_scope(scope) {
        scanned = scanned.add(predicate);
    }
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::SensorId))
        .column((r.clone(), readings::Column::SiteId))
        .column((r.clone(), readings::Column::ParameterId))
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("orphan_count"))
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("first_time"),
        )
        .expr_as(
            Func::max(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("last_time"),
        )
        .from_as(readings::Entity, r.clone())
        .cond_where(scanned)
        .add_group_by([
            Expr::col((r.clone(), readings::Column::SensorId)),
            Expr::col((r.clone(), readings::Column::SiteId)),
            Expr::col((r.clone(), readings::Column::ParameterId)),
        ])
        .order_by_expr(Expr::cust("COUNT(*)"), Order::Desc)
        .take()
        .build(PostgresQueryBuilder);

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    rows.iter()
        .map(|r| {
            let row = OrphanedCorrectionRow::from_query_result(r, "")?;
            Ok(OrphanedCorrection {
                sensor_id: row.sensor_id,
                site_id: row.site_id,
                parameter_id: row.parameter_id,
                count: row.orphan_count,
                first_time: row.first_time.with_timezone(&chrono::Utc),
                last_time: row.last_time.with_timezone(&chrono::Utc),
            })
        })
        .collect()
}

/// List calibration anomalies: readings a window covers but that carry no `calibration_id`, and
/// readings carrying a correction no curve accounts for. Requires `read_metadata`.
///
/// Both halves read the readings hypertable with no index behind them, so the report covers the
/// most recent 90 days of data unless `since` names an earlier floor. It is a report on a window,
/// not a census: `scanned_from` carries the floor that was used and every count is a floor for that
/// window alone. `backfill_calibrations` is not bounded this way, so a widened report and the
/// backfill it feeds agree on which sensors are repairable.
#[utoipa::path(
    get,
    path = "/api/actions/calibration_candidates",
    params(CalibrationCandidatesQuery),
    responses((status = 200, description = "Calibration backfill candidates", body = CalibrationBackfillCandidatesResponse)),
    tag = "actions"
)]
pub async fn calibration_candidates(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    _: DenyScoped,
    Query(query): Query<CalibrationCandidatesQuery>,
) -> AppResult<Json<CalibrationBackfillCandidatesResponse>> {
    let scanned_from = match query.since {
        Some(since) => Some(since),
        None => default_scan_floor(&app_state.db).await?,
    };
    let candidates = fetch_calibration_candidates(&app_state.db, &scope, scanned_from).await?;
    let orphaned_corrections =
        fetch_orphaned_corrections(&app_state.db, &scope, scanned_from).await?;
    let foreign_curve_uses = fetch_foreign_curve_uses(&app_state.db, &scope, scanned_from).await?;
    let total_uncalibrated: i64 = candidates.iter().map(|c| c.uncalibrated_count).sum();
    let total_orphaned_corrections: i64 = orphaned_corrections.iter().map(|c| c.count).sum();
    let total_foreign_curve_uses: i64 = foreign_curve_uses.iter().map(|c| c.count).sum();
    Ok(Json(CalibrationBackfillCandidatesResponse {
        total_candidates: candidates.len(),
        total_uncalibrated,
        candidates,
        total_orphaned_corrections,
        orphaned_corrections,
        total_foreign_curve_uses,
        foreign_curve_uses,
        scanned_from,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BackfillCalibrationsRequest {
    #[serde(default)]
    pub all: bool,
    pub sensor_id: Option<Uuid>,
    #[serde(default)]
    pub sensor_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillCalibrationsResponse {
    pub job_id: Uuid,
    pub status: String,
    pub sensors_updated: usize,
    pub estimated_readings: i64,
}

/// Reprocess the sensors whose readings a calibration window covers but whose `calibration_id` was
/// never stamped, so each row picks up the curve that already covers it. No calibration is created:
/// a reading no window covers stays uncorrected, which is what it is. Runs as one tracked job.
/// Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/backfill_calibrations",
    request_body = BackfillCalibrationsRequest,
    responses(
        (status = 200, description = "Calibration backfill triggered", body = BackfillCalibrationsResponse),
        (status = 403, description = "A named sensor is outside the caller's projects, or no sensor was named"),
        (status = 404, description = "No such sensor"),
    ),
    tag = "actions"
)]
pub async fn backfill_calibrations(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BackfillCalibrationsRequest>,
) -> AppResult<Json<BackfillCalibrationsResponse>> {
    let db = &app_state.db;

    // Named instruments are confined one by one, including inventory (`Unowned::Allow`): a sensor
    // with no deployment belongs to no project, and refusing it would put a newly imported
    // instrument out of reach of every member. `all` names nothing, so a restricted caller may not
    // use it.
    let named: Vec<Uuid> = payload
        .sensor_id
        .into_iter()
        .chain(payload.sensor_ids.iter().copied())
        .collect();
    require_named_target(&scope, !named.is_empty() || !payload.all, "sensor")?;
    for sensor_id in &named {
        let target = project_of_sensor(db, *sensor_id).await?;
        confine_target(&scope, &target, Unowned::Allow, "sensor")?;
    }

    // With instruments named, each was confined just above and the selection below keeps only
    // those, so the candidate query runs unfiltered and inventory stays reachable. With nothing
    // named the caller is unrestricted (`require_named_target`), and the query is unfiltered too.
    //
    // Unbounded in time as well, unlike the report: this selects the work rather than describing
    // it, and a sensor whose only unstamped readings are older than the report's window still has
    // to be repaired. One operator action pays for the whole-history read; the reads the dashboard
    // makes on every page load do not.
    let all_candidates = fetch_calibration_candidates(db, &AccessScope::Unrestricted, None).await?;
    let id_filter: HashSet<Uuid> = payload.sensor_ids.iter().copied().collect();
    let selected: Vec<CalibrationBackfillCandidate> = all_candidates
        .into_iter()
        .filter(|c| {
            if !id_filter.is_empty() {
                id_filter.contains(&c.sensor_id)
            } else if let Some(sid) = payload.sensor_id {
                c.sensor_id == sid
            } else {
                payload.all
            }
        })
        .collect();

    if selected.is_empty() {
        return Err(AppError::BadRequest(
            "No matching calibration backfill candidates (pass all=true, a sensor_id, or sensor_ids)".to_string(),
        ));
    }

    let estimated_readings: i64 = selected.iter().map(|c| c.uncalibrated_count).sum();
    // The candidate query is the whole selection: every sensor in it has readings an existing
    // window covers, so the reprocess alone resolves them. Orphaned corrections are reported by
    // `calibration_candidates` and are deliberately not enqueued here, a value nobody can trace to
    // a curve is an operator's question, not something to overwrite.
    let sensor_ids_touched: Vec<Uuid> = selected.iter().map(|c| c.sensor_id).collect();

    let sensors_updated = sensor_ids_touched.len();
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "backfill_calibrations",
        None,
        None,
        &serde_json::json!({ "sensors": sensor_ids_touched }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue backfill_calibrations job".to_string()))?;

    Ok(Json(BackfillCalibrationsResponse {
        job_id,
        status: "queued".to_string(),
        sensors_updated,
        estimated_readings,
    }))
}

#[derive(FromQueryResult)]
struct OrphanedCorrectionRow {
    sensor_id: Option<Uuid>,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    orphan_count: i64,
    first_time: chrono::DateTime<chrono::FixedOffset>,
    last_time: chrono::DateTime<chrono::FixedOffset>,
}

// --- Recalculate one curve's readings ---

/// The job a recalculation enqueued.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RecalculateResponse {
    pub job_id: Uuid,
}

/// Reprocess readings for the sensor owning a specific calibration.
/// Enqueues a tracked worker job and returns immediately.
#[utoipa::path(
    post,
    path = "/api/actions/sensor_calibrations/{id}/recalculate",
    params(("id" = Uuid, Path, description = "Calibration UUID")),
    responses(
        (status = 200, description = "Reprocessing job spawned", body = RecalculateResponse),
        (status = 403, description = "The calibration's instrument is deployed outside the caller's projects"),
        (status = 404, description = "Calibration not found"),
    ),
    tag = "actions"
)]
pub async fn recalculate_calibration(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RecalculateResponse>> {
    let row = crate::routes::private::sensor_calibrations::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(|e| crate::error::AppError::Internal(e.to_string()))?;

    let Some(row) = row else {
        return Err(crate::error::AppError::NotFound(format!(
            "sensor_calibration {id} not found"
        )));
    };
    let sensor_id = row.sensor_id;
    require_instrument_in_scope(&state.db, &scope, sensor_id).await?;

    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "calibration_recalculate",
        Some(sensor_id),
        Some(id),
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(|e| crate::error::AppError::Internal(e.to_string()))?
    .ok_or_else(|| crate::error::AppError::Internal("enqueue returned no id".into()))?;

    Ok(Json(RecalculateResponse { job_id }))
}

#[cfg(test)]
#[path = "tests/views.rs"]
mod tests;
