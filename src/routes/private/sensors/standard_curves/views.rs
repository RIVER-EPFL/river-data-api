//! Provenance-keyed upsert of standard curves for sync services replicating a portal's own
//! curve table. Idempotent per `(source_system, source_key)`, and bound by the same freeze rule
//! as the CRUD surface: a curve any reading references never changes in place. A portal that
//! edits a used curve's coefficients gets a NEW row minted under the same provenance, so the
//! mapping follows the portal forward while history keeps the curve that produced it.

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{
    Alias, Condition, Expr, ExprTrait, Func, JoinType, Order, PostgresQueryBuilder, Query,
    SelectStatement,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Column, Entity, Model};
use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::annotations::models as annotations;
use crate::routes::private::parameters;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::service::SPOT;
use crate::routes::private::sensors;
use crate::routes::private::sensors::models::InstrumentKind;
use crate::routes::private::sensors::service::upsert_source_instrument;

/// One portal standard curve to register. The curve's own fields are
/// `river_data_core::models::StandardCurveUpsert`, which the sync services build from, so a field
/// the sender gains cannot be dropped here; the API adds the source the caller is speaking for.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterStandardCurveRequest {
    /// The sync source the curve comes from, e.g. "cnet".
    pub source_system: String,
    #[serde(flatten)]
    pub curve: river_data_core::models::StandardCurveUpsert,
}

impl<'de> Deserialize<'de> for RegisterStandardCurveRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (source_system, curve) =
            crate::routes::private::wire::with_source_system(deserializer, &[])?;
        Ok(Self {
            source_system,
            curve,
        })
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterStandardCurveResponse {
    pub id: Uuid,
    pub sensor_id: Uuid,
    /// True when the stored coefficients differed and the curve was already applied to readings,
    /// so a new row was minted under this provenance. History keeps the old row.
    pub superseded: bool,
}

/// Whether any reading was corrected with this curve, or any annotation records a source-side
/// correction made with it; a used curve's coefficients are frozen.
pub(crate) async fn curve_is_used<C: ConnectionTrait>(conn: &C, id: Uuid) -> AppResult<bool> {
    let on_a_reading = readings::Entity::find()
        .filter(readings::Column::StandardCurveId.eq(id))
        .one(conn)
        .await?
        .is_some();
    if on_a_reading {
        return Ok(true);
    }
    Ok(annotations::Entity::find()
        .filter(annotations::Column::StandardCurveId.eq(id))
        .one(conn)
        .await?
        .is_some())
}

/// Find or create the lab instrument a source's curves attach to, one per
/// (source_system, instrument_label). `(source_system, source_key)` is the identity, so
/// re-registration resolves the same instrument instead of minting one per cycle.
///
/// The serial lookup is the fallback for an instrument minted before provenance existed whose
/// curve registration never landed, so the migration's curve-join backfill could not reach it;
/// resolving it that way stamps the provenance it was missing.
pub(crate) async fn resolve_lab_instrument(
    state: &AppState,
    source_system: &str,
    instrument_label: &str,
) -> AppResult<Uuid> {
    let source_key = format!("{source_system}:{instrument_label}");
    if let Some(existing) = sensors::Entity::find()
        .filter(sensors::Column::SourceSystem.eq(source_system))
        .filter(sensors::Column::SourceKey.eq(source_key.clone()))
        .one(&state.db)
        .await?
    {
        return Ok(existing.id);
    }
    // Rows that predate the provenance columns carry the key in `serial_number`. Only an unclaimed
    // row may be adopted, so one source can never take over another's instrument, and the oldest
    // wins so the choice is deterministic now that a serial is no longer unique.
    if let Some(existing) = sensors::Entity::find()
        .filter(sensors::Column::SerialNumber.eq(source_key.clone()))
        .filter(sensors::Column::SourceSystem.is_null())
        .order_by_asc(sensors::Column::CreatedAt)
        .one(&state.db)
        .await?
    {
        let id = existing.id;
        let mut active: sensors::ActiveModel = existing.into();
        active.source_system = Set(Some(source_system.to_string()));
        active.source_key = Set(Some(source_key));
        active.update(&state.db).await?;
        return Ok(id);
    }
    // `serial_number` is left unset: it holds the lab's own serial for an instrument, never a
    // fabricated copy of the provenance key, which `source_key` already carries.
    upsert_source_instrument(
        &state.db,
        source_system,
        &source_key,
        &format!("{instrument_label} ({source_system})"),
        InstrumentKind::Lab,
        "low",
        None,
    )
    .await
}

/// Upsert a standard curve by provenance. Requires `write_metadata` (sync session tokens carry
/// it). Identical coefficients return the existing row; changed coefficients update in place only
/// while the curve is unused, and mint a successor row once any reading references it.
#[utoipa::path(
    post,
    path = "/api/standard_curves/register",
    request_body = RegisterStandardCurveRequest,
    responses(
        (status = 200, description = "Curve registered (created, unchanged, updated, or superseded)", body = RegisterStandardCurveResponse),
    ),
    tag = "sensors"
)]
pub async fn register_standard_curve(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(payload): Json<RegisterStandardCurveRequest>,
) -> AppResult<Json<RegisterStandardCurveResponse>> {
    let source_system = crate::common::provenance::source_system(&auth, &payload.source_system)?;
    if payload.curve.slope == 0.0 {
        return Err(AppError::BadRequest(
            "Slope cannot be zero: all readings would produce a constant value".to_string(),
        ));
    }
    if payload.curve.source_key.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_key identifies the curve and cannot be empty".to_string(),
        ));
    }

    let sensor_id =
        resolve_lab_instrument(&state, &source_system, &payload.curve.instrument_label).await?;

    let existing = Entity::find()
        .filter(Column::SourceSystem.eq(source_system.clone()))
        .filter(Column::SourceKey.eq(payload.curve.source_key.clone()))
        .one(&state.db)
        .await?;

    let coefficients_match = |c: &Model| {
        c.slope == payload.curve.slope
            && c.intercept == payload.curve.intercept
            && c.sensor_id == sensor_id
    };

    if let Some(current) = existing {
        if coefficients_match(&current) {
            return Ok(Json(RegisterStandardCurveResponse {
                id: current.id,
                sensor_id,
                superseded: false,
            }));
        }
        if !curve_is_used(&state.db, current.id).await? {
            let mut active: super::ActiveModel = current.into();
            active.sensor_id = Set(sensor_id);
            active.slope = Set(payload.curve.slope);
            active.intercept = Set(payload.curve.intercept);
            active.r_squared = Set(payload.curve.r_squared);
            if let Some(name) = payload.curve.name.clone() {
                active.name = Set(Some(name));
            }
            if let Some(fitted_on) = payload.curve.fitted_on {
                active.fitted_on = Set(Some(fitted_on));
            }
            let updated = active.update(&state.db).await?;
            return Ok(Json(RegisterStandardCurveResponse {
                id: updated.id,
                sensor_id,
                superseded: false,
            }));
        }
        // Used curve edited upstream: mint a successor, move the provenance to it and retire the
        // row it replaces, in one transaction. The old row keeps the readings it produced and the
        // system it came from; only its `source_key` is cleared, which is what the partial unique
        // index needs to admit the successor, and the clearing must come first, because while the
        // old row still holds the key the successor insert conflicts, does nothing, and resolves
        // back to the old row. Retiring it is what takes it out of the picker: without that, the
        // lab is offered both rows on one instrument, the same name and fit date on each, and
        // nothing saying which one the portal now holds.
        let old_id = current.id;
        let txn = state.db.begin().await?;
        Entity::update_many()
            .col_expr(Column::SourceKey, Expr::value(None::<String>))
            .filter(Column::Id.eq(old_id))
            .exec(&txn)
            .await?;
        let minted = insert_curve(&txn, &payload, &source_system, sensor_id)
            .await
            .map_err(|e| {
                AppError::Internal(format!("minting successor for edited curve {old_id}: {e}"))
            })?;
        Entity::update_many()
            .col_expr(Column::RetiredAt, Expr::current_timestamp())
            .col_expr(Column::RetiredBy, Expr::value(Some(source_system.clone())))
            .col_expr(
                Column::RetiredReason,
                Expr::value(Some(format!(
                    "Superseded by {minted}: {} re-registered {} with different coefficients",
                    payload.source_system, payload.curve.source_key
                ))),
            )
            .filter(Column::Id.eq(old_id))
            .filter(Column::RetiredAt.is_null())
            .exec(&txn)
            .await?;
        txn.commit().await?;
        tracing::warn!(
            source_system = %payload.source_system,
            source_key = %payload.curve.source_key,
            %old_id,
            new_id = %minted,
            "Portal edited a standard curve already applied to readings; minted a successor"
        );
        return Ok(Json(RegisterStandardCurveResponse {
            id: minted,
            sensor_id,
            superseded: true,
        }));
    }

    let id = insert_curve(&state.db, &payload, &source_system, sensor_id).await?;
    Ok(Json(RegisterStandardCurveResponse {
        id,
        sensor_id,
        superseded: false,
    }))
}

async fn insert_curve<C: ConnectionTrait>(
    conn: &C,
    payload: &RegisterStandardCurveRequest,
    source_system: &str,
    sensor_id: Uuid,
) -> AppResult<Uuid> {
    let id = Uuid::new_v4();
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "INSERT INTO standard_curves
                 (id, sensor_id, name, fitted_on, slope, intercept, r_squared, notes, created_at,
                  source_system, source_key)
             VALUES ($1, $2, $3, COALESCE($4, CURRENT_DATE), $5, $6, $7, $8, NOW(), $9, $10)
             ON CONFLICT (source_system, source_key)
                 WHERE source_system IS NOT NULL AND source_key IS NOT NULL
                 DO NOTHING",
        [
            id.into(),
            sensor_id.into(),
            payload
                .curve
                .name
                .clone()
                .unwrap_or_else(|| payload.curve.source_key.clone())
                .into(),
            payload.curve.fitted_on.into(),
            payload.curve.slope.into(),
            payload.curve.intercept.into(),
            payload.curve.r_squared.into(),
            payload.curve.notes.clone().into(),
            source_system.into(),
            payload.curve.source_key.clone().into(),
        ],
    ))
    .await?;
    // A concurrent register of the same provenance wins the insert; resolve to whichever row holds
    // the key now.
    let row = Entity::find()
        .filter(Column::SourceSystem.eq(source_system))
        .filter(Column::SourceKey.eq(payload.curve.source_key.clone()))
        .one(conn)
        .await?
        .ok_or_else(|| AppError::Internal("registered curve not found after upsert".to_string()))?;
    Ok(row.id)
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct LastUsedCurveQuery {
    /// One of `parameter_id` and `parameter_code` is required.
    pub parameter_id: Option<Uuid>,
    pub parameter_code: Option<String>,
}

/// The instrument and standard curve the newest grab at a site and parameter recorded. Every
/// field but `method` is null when no grab there names either.
#[derive(Debug, Serialize, ToSchema)]
pub struct LastUsedCurveResponse {
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    #[schema(required)]
    pub sensor_id: Option<Uuid>,
    #[schema(required)]
    pub sensor_name: Option<String>,
    #[schema(required)]
    pub standard_curve_id: Option<Uuid>,
    #[schema(required)]
    pub curve_name: Option<String>,
    #[schema(required)]
    pub curve_created_at: Option<chrono::DateTime<Utc>>,
    /// The instant of the grab the answer was read from.
    #[schema(required)]
    pub used_at: Option<chrono::DateTime<Utc>>,
    /// How the answer was decided, for the picker to show beside it.
    pub method: String,
}

const LAST_USED_METHOD: &str = "The newest spot reading at this site and parameter that records \
    an instrument or a standard curve, withdrawn readings excluded. The instrument is the one the \
    reading names, or the curve's when the reading names none.";

/// The one reading a slot's last hand-picked curve is read from. `time` is the instant it was
/// used, which the response calls `used_at`.
#[derive(FromQueryResult)]
struct LastUsedRow {
    time: Option<DateTime<Utc>>,
    sensor_id: Option<Uuid>,
    sensor_name: Option<String>,
    standard_curve_id: Option<Uuid>,
    curve_name: Option<String>,
    curve_created_at: Option<DateTime<Utc>>,
}

/// The newest grab at a slot that names an instrument or a curve, withdrawn rows excluded. The
/// instrument is the reading's own where it has one and the curve's owner otherwise, so the join
/// resolves the same name either way.
fn last_curve_query(site_id: Uuid, parameter_id: Uuid) -> SelectStatement {
    let r = Alias::new("r");
    let c = Alias::new("c");
    let s = Alias::new("s");
    let owning_sensor: Expr = Func::coalesce([
        Expr::col((r.clone(), readings::Column::SensorId)),
        Expr::col((c.clone(), Column::SensorId)),
    ])
    .into();
    Query::select()
        .expr_as(
            Expr::col((r.clone(), readings::Column::Time)),
            Alias::new("time"),
        )
        .expr_as(owning_sensor.clone(), Alias::new("sensor_id"))
        .expr_as(
            Expr::col((s.clone(), sensors::Column::Name)),
            Alias::new("sensor_name"),
        )
        .expr_as(
            Expr::col((r.clone(), readings::Column::StandardCurveId)),
            Alias::new("standard_curve_id"),
        )
        .expr_as(
            Expr::col((c.clone(), Column::Name)),
            Alias::new("curve_name"),
        )
        .expr_as(
            Expr::col((c.clone(), Column::CreatedAt)),
            Alias::new("curve_created_at"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            Entity,
            c.clone(),
            Expr::col((c.clone(), Column::Id))
                .equals((r.clone(), readings::Column::StandardCurveId)),
        )
        .join_as(
            JoinType::LeftJoin,
            sensors::Entity,
            s.clone(),
            Expr::col((s.clone(), sensors::Column::Id)).eq(owning_sensor),
        )
        .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(site_id))
        .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).eq(parameter_id))
        .and_where(Expr::col((r.clone(), readings::Column::MeasurementType)).eq(SPOT))
        .and_where(Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_null())
        .cond_where(
            Condition::any()
                .add(Expr::col((r.clone(), readings::Column::StandardCurveId)).is_not_null())
                .add(Expr::col((r.clone(), readings::Column::SensorId)).is_not_null()),
        )
        .order_by((r.clone(), readings::Column::Time), Order::Desc)
        .order_by((r, readings::Column::ReplicateIndex), Order::Asc)
        .limit(1)
        .to_owned()
}

/// `GET /sites/{id}/last_curve`: what the last grab at a slot was measured on and corrected
/// with, so the picker opens where the previous batch left off. `read_data`.
#[utoipa::path(
    get,
    path = "/api/sites/{site_id}/last_curve",
    params(("site_id" = Uuid, Path, description = "Site UUID"), LastUsedCurveQuery),
    responses(
        (status = 200, body = LastUsedCurveResponse),
        (status = 400, description = "Neither parameter_id nor parameter_code given"),
        (status = 404, description = "Site or parameter not found"),
    ),
    tag = "sensors"
)]
pub async fn last_used_curve(
    State(state): State<AppState>,
    crate::common::middleware::ProjectScope(scope): crate::common::middleware::ProjectScope,
    axum::extract::Path(site_id): axum::extract::Path<Uuid>,
    axum::extract::Query(q): axum::extract::Query<LastUsedCurveQuery>,
) -> AppResult<Json<LastUsedCurveResponse>> {
    use crate::common::scope::{Unowned, project_of_site, require_row_in_scope};
    let db = &state.db;
    let site = project_of_site(db, site_id).await?;
    require_row_in_scope(&scope, &site, Unowned::Deny, "site")?;

    let parameter_id = match (q.parameter_id, q.parameter_code.as_deref()) {
        (Some(id), _) => id,
        // `lower(code) = lower($1)`, the shape of the catalog's unique index on the code.
        (None, Some(code)) => parameters::Entity::find()
            .filter(
                Expr::expr(Func::lower(Expr::col(parameters::Column::Code)))
                    .eq(code.to_lowercase()),
            )
            .select_only()
            .column(parameters::Column::Id)
            .into_tuple::<Uuid>()
            .one(db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Parameter '{code}' not found")))?,
        (None, None) => {
            return Err(AppError::BadRequest(
                "parameter_id or parameter_code is required".to_string(),
            ));
        }
    };

    let query = last_curve_query(site_id, parameter_id);
    let (sql, values) = query.build(PostgresQueryBuilder);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;

    let mut out = LastUsedCurveResponse {
        site_id,
        parameter_id,
        sensor_id: None,
        sensor_name: None,
        standard_curve_id: None,
        curve_name: None,
        curve_created_at: None,
        used_at: None,
        method: LAST_USED_METHOD.to_string(),
    };
    if let Some(row) = row {
        let last = LastUsedRow::from_query_result(&row, "")?;
        out.sensor_id = last.sensor_id;
        out.sensor_name = last.sensor_name;
        out.standard_curve_id = last.standard_curve_id;
        out.curve_name = last.curve_name;
        out.curve_created_at = last.curve_created_at;
        out.used_at = last.time;
    }
    Ok(Json(out))
}

#[cfg(test)]
#[path = "tests/views.rs"]
mod tests;
