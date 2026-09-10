//! Retiring a windowed calibration: taking a curve out of circulation without deleting it.
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
use sea_orm::ExprTrait;
use sea_orm::sea_query::{Alias, Expr, PostgresQueryBuilder, Query as SeaQuery};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder,
    QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::models::{self as readings, Kind, Selection, decision_set};
use crate::routes::private::readings::service::{self, NewValue, not_pinned_sql};

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
            &moved_rows_sql(),
            vec![id.into(), sensor_id.into()],
            NewValue::Sql(covering_curve_sql_object()),
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
        super::model::Entity::update_many()
            .col_expr(
                super::model::Column::RetiredAt,
                Expr::current_timestamp().into(),
            )
            .col_expr(
                super::model::Column::RetiredBy,
                Expr::value(Some(actor.clone())),
            )
            .col_expr(
                super::model::Column::RetiredReason,
                Expr::value(req.reason.clone()),
            )
            .filter(super::model::Column::Id.eq(id))
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
        super::model::Entity::update_many()
            .col_expr(
                super::model::Column::RetiredAt,
                Expr::value(None::<chrono::DateTime<Utc>>),
            )
            .col_expr(super::model::Column::RetiredBy, Expr::value(None::<String>))
            .col_expr(
                super::model::Column::RetiredReason,
                Expr::value(None::<String>),
            )
            .filter(super::model::Column::Id.eq(id))
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
    super::model::Entity::find_by_id(id)
        .select_only()
        .column(super::model::Column::SensorId)
        .column(super::model::Column::RetiredAt)
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
