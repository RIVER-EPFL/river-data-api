//! Retiring a standard curve: taking it out of the picker without touching a stored value.
//!
//! A standard curve is named by the reading (`readings.standard_curve_id`) and never resolved by
//! window, so nothing re-derives one and the readings that carry it keep the value it produced.
//! Retirement is therefore a statement about the curve alone: it stops being offered, and no
//! `reading_decisions` row is owed because no reading moved.

use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Statement};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::model::{Column, Entity};
use crate::common::AppState;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RetireCurveRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RetireCurveResponse {
    pub standard_curve_id: Uuid,
    #[schema(required)]
    pub retired_at: Option<DateTime<Utc>>,
    /// Readings this curve corrected. They keep it and keep their values; the count is what the
    /// surface states before the action runs.
    pub readings: i64,
}

/// `POST /standard_curves/{id}/retire`. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/standard_curves/{id}/retire",
    params(("id" = Uuid, Path, description = "Standard curve UUID")),
    request_body = RetireCurveRequest,
    responses(
        (status = 200, description = "Out of circulation", body = RetireCurveResponse),
        (status = 404, description = "No such curve"),
        (status = 409, description = "Already retired"),
    ),
    tag = "sensors"
)]
pub async fn retire_standard_curve(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<RetireCurveRequest>,
) -> AppResult<Json<RetireCurveResponse>> {
    let actor = crate::common::actor::label(&auth);
    if retired_at(&state.db, id).await?.is_some() {
        return Err(AppError::Conflict(format!(
            "Standard curve {id} is already retired"
        )));
    }
    Entity::update_many()
        .col_expr(Column::RetiredAt, Expr::current_timestamp())
        .col_expr(Column::RetiredBy, Expr::value(Some(actor)))
        .col_expr(Column::RetiredReason, Expr::value(req.reason))
        .filter(Column::Id.eq(id))
        .exec(&state.db)
        .await?;
    Ok(Json(RetireCurveResponse {
        standard_curve_id: id,
        retired_at: retired_at(&state.db, id).await?,
        readings: readings_using(&state.db, id).await?,
    }))
}

/// `POST /standard_curves/{id}/unretire`, back in the picker. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/standard_curves/{id}/unretire",
    params(("id" = Uuid, Path, description = "Standard curve UUID")),
    responses(
        (status = 200, description = "Offered again", body = RetireCurveResponse),
        (status = 404, description = "No such curve"),
        (status = 409, description = "Not retired"),
    ),
    tag = "sensors"
)]
pub async fn unretire_standard_curve(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<RetireCurveResponse>> {
    if retired_at(&state.db, id).await?.is_none() {
        return Err(AppError::Conflict(format!(
            "Standard curve {id} is not retired"
        )));
    }
    Entity::update_many()
        .col_expr(Column::RetiredAt, Expr::value(None::<DateTime<Utc>>))
        .col_expr(Column::RetiredBy, Expr::value(None::<String>))
        .col_expr(Column::RetiredReason, Expr::value(None::<String>))
        .filter(Column::Id.eq(id))
        .exec(&state.db)
        .await?;
    Ok(Json(RetireCurveResponse {
        standard_curve_id: id,
        retired_at: None,
        readings: readings_using(&state.db, id).await?,
    }))
}

async fn retired_at<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<Option<DateTime<Utc>>> {
    let curve = Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Standard curve {id} not found")))?;
    Ok(curve.retired_at)
}

async fn readings_using<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<i64> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*)::bigint AS n FROM readings WHERE standard_curve_id = $1",
            [id.into()],
        ))
        .await?;
    Ok(match row {
        Some(row) => row.try_get("", "n")?,
        None => 0,
    })
}
