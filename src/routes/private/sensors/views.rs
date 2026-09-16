//! The component's HTTP surface.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use river_data_core::models::MeasurementType;
use sea_orm::sea_query::{
    Alias, Expr, ExprTrait as _, Func, JoinType, OnConflict, PostgresQueryBuilder,
    Query as SeaQuery, SelectStatement, SimpleExpr, SubQueryStatement,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, FromQueryResult, Order,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, sensor_in_scope};
use crate::common::scope;
use crate::common::scope::{Unowned, confine_target, project_of_sensor, require_named_target};
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::models as readings;
use crate::routes::private::reprocessing_jobs::models::QueuedJobResponse;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::service::recompute_deployed_until;
use crate::routes::private::sensor_deployments as deployments;
use crate::routes::private::sites::service::{bucket_interval, resolution_of};
use crate::routes::private::standard_curves;
use crate::routes::private::{data_streams, parameters, sensors, site_parameters, sites};

use super::models::*;

/// Resolve the `(site, parameter)` site_parameter, creating it when missing (if allowed). Mirrors
/// the sync path's helper so an adopted sensor's data lands under the same junction config.
async fn resolve_or_create_site_parameter<C: ConnectionTrait>(
    db: &C,
    site_id: Uuid,
    parameter_id: Uuid,
    create: bool,
) -> AppResult<(Uuid, bool)> {
    let existing = site_parameters::Entity::find()
        .filter(
            Condition::all()
                .add(site_parameters::Column::SiteId.eq(site_id))
                .add(site_parameters::Column::ParameterId.eq(parameter_id)),
        )
        .one(db)
        .await?;
    if let Some(existing) = existing {
        return Ok((existing.id, false));
    }
    if !create {
        return Err(AppError::BadRequest(
            "No site_parameter exists for this (site, parameter); pass create_site_parameter=true"
                .to_string(),
        ));
    }
    let param = parameters::Entity::find_by_id(parameter_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Parameter not found".to_string()))?;
    let sp = site_parameters::ActiveModel {
        id: Set(Uuid::new_v4()),
        instrument_sensor_id: Set(None),
        site_id: Set(site_id),
        parameter_id: Set(parameter_id),
        name: Set(param.name),
        sensor_type: Set(String::new()),
        display_units: Set(None),
        units_name: Set(None),
        units_min: Set(None),
        units_max: Set(None),
        decimal_places: Set(None),
        channel_id: Set(None),
        sample_interval_sec: Set(None),
        is_active: Set(Some(true)),
        is_public: Set(Some(false)),
        needs_review: Set(false),
        entry_mode: Set("manual".to_string()),
        variable_mappings: Set(None),
        created_at: Set(Some(Utc::now())),
        updated_at: Set(Some(Utc::now())),
        discovered_at: Set(Some(Utc::now())),
    };
    let inserted = sp.insert(db).await?;
    Ok((inserted.id, true))
}

fn is_slot_conflict(err: &sea_orm::DbErr) -> bool {
    let msg = err.to_string();
    msg.contains("excl_deployment_site_param_slot") || msg.contains("23P01")
}

/// Resolve the parameter to bind an adopt to. A sensor has no intrinsic parameter, so use the
/// explicit `override_param` when given; otherwise derive it from the sensor's existing deployment /
/// calibration parameters (unambiguous only when the sensor covers exactly one parameter).
async fn resolve_sensor_parameter<C: ConnectionTrait>(
    db: &C,
    sensor_id: Uuid,
    override_param: Option<Uuid>,
) -> AppResult<Uuid> {
    if let Some(p) = override_param {
        return Ok(p);
    }
    if sensors::models::Entity::find_by_id(sensor_id)
        .select_only()
        .column(sensors::models::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }
    // The parameters this instrument is already bound to, from either side: a deployment names
    // one, and so does a calibration.
    let mut params: Vec<Uuid> = deployments::Entity::find()
        .filter(deployments::Column::SensorId.eq(sensor_id))
        .select_only()
        .column(deployments::Column::ParameterId)
        .distinct()
        .into_tuple::<Uuid>()
        .all(db)
        .await?;
    params.extend(
        sensor_calibrations::Entity::find()
            .filter(sensor_calibrations::Column::SensorId.eq(sensor_id))
            .select_only()
            .column(sensor_calibrations::Column::ParameterId)
            .distinct()
            .into_tuple::<Option<Uuid>>()
            .all(db)
            .await?
            .into_iter()
            .flatten(),
    );
    params.sort();
    params.dedup();
    match params.as_slice() {
        [one] => Ok(*one),
        [] => Err(AppError::BadRequest(
            "This sensor has no parameter yet; pass parameter_id".to_string(),
        )),
        _ => Err(AppError::BadRequest(
            "This sensor measures multiple parameters; pass parameter_id to pick the slot"
                .to_string(),
        )),
    }
}

/// Resolve the (site, parameter) slot a swap targets: the parameter the outgoing sensor is deployed
/// for at the site, unless overridden.
async fn resolve_swap_parameter<C: ConnectionTrait>(
    db: &C,
    outgoing_sensor_id: Uuid,
    site_id: Uuid,
    override_param: Option<Uuid>,
) -> AppResult<Uuid> {
    if let Some(p) = override_param {
        return Ok(p);
    }
    // The open deployment first, then the most recent closed one.
    deployments::Entity::find()
        .filter(deployments::Column::SensorId.eq(outgoing_sensor_id))
        .filter(deployments::Column::SiteId.eq(site_id))
        .select_only()
        .column(deployments::Column::ParameterId)
        .order_by_desc(deployments::Column::DeployedUntil.is_null())
        .order_by_desc(deployments::Column::DeployedFrom)
        .into_tuple::<Uuid>()
        .one(db)
        .await?
        .ok_or_else(|| {
            AppError::BadRequest(
                "Outgoing sensor is not deployed at this site; pass parameter_id".to_string(),
            )
        })
}

/// Give the sensor's still-unparameterised readings the slot's parameter. The reprocess sets
/// `site_id` and `deployment_id` by window and leaves `parameter_id` alone, and the aggregates
/// group by it, so adopt and swap both claim it here inside their own transaction.
async fn claim_unparameterised<C: ConnectionTrait>(
    conn: &C,
    sensor_id: Uuid,
    parameter_id: Uuid,
) -> AppResult<()> {
    readings::Entity::update_many()
        .col_expr(readings::Column::ParameterId, Expr::value(parameter_id))
        .filter(readings::Column::SensorId.eq(sensor_id))
        .filter(readings::Column::ParameterId.is_null())
        .exec(conn)
        .await?;
    Ok(())
}

/// Adopt (deploy) a sensor to a site slot for a window. Auto-creates the site_parameter if missing,
/// then re-derives the sensor's readings by window (tracked job). Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sensors/{sensor_id}/adopt",
    params(("sensor_id" = Uuid, Path, description = "Sensor UUID")),
    request_body = AdoptRequest,
    responses(
        (status = 200, description = "Sensor adopted; returns deployment + tracked job id", body = AdoptResponse),
        (status = 404, description = "Sensor or site not found"),
        (status = 409, description = "Slot occupied by another sensor over an overlapping window"),
    ),
    tag = "sensors"
)]
pub async fn adopt_sensor(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
    Json(payload): Json<AdoptRequest>,
) -> AppResult<Json<AdoptResponse>> {
    let db = &app_state.db;
    // Confine to the target site before any write: deploying an instrument into a project the
    // caller has no grant on is the write this refuses.
    scope::require_sites_in_scope(db, &scope, &[payload.site_id]).await?;
    // A deployment is a statement that this instrument measures at this site, so a bookkeeping
    // row cannot hold one: deploying one gives it a current site and a window that resolution
    // would then honour.
    crate::routes::private::sensors::service::require_measuring_instrument(
        db,
        sensor_id,
        "deployed to a site",
    )
    .await?;
    let parameter_id = resolve_sensor_parameter(db, sensor_id, payload.parameter_id).await?;

    if sites::models::Entity::find_by_id(payload.site_id)
        .select_only()
        .column(sites::models::Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Site not found".to_string()));
    }

    let deployed_from = payload.deployed_from.unwrap_or_else(Utc::now);
    if let Some(until) = payload.deployed_until
        && until <= deployed_from
    {
        return Err(AppError::BadRequest(
            "deployed_until must be after deployed_from".to_string(),
        ));
    }

    let txn = db.begin().await?;
    // The change-audit trigger reads the writer from the transaction it fires in.
    crate::common::actor::declare(&txn).await?;
    // The readings parameter_id backfill below reaches compressed chunks.
    crate::common::bulk_write::lift_decompression_cap(&txn).await?;
    let (site_parameter_id, site_parameter_created) = resolve_or_create_site_parameter(
        &txn,
        payload.site_id,
        parameter_id,
        payload.create_site_parameter,
    )
    .await?;

    // Auto-recall this sensor's currently-open deployment FOR THIS PARAMETER at the new start (twin of
    // the sensor_deployments before_create hook). Scoped to the parameter so adopting one channel of a
    // multi-channel instrument doesn't recall its other channels.
    deployments::Entity::update_many()
        .col_expr(
            deployments::Column::DeployedUntil,
            Expr::value(Some(deployed_from)),
        )
        .filter(deployments::Column::SensorId.eq(sensor_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .filter(deployments::Column::DeployedUntil.is_null())
        .exec(&txn)
        .await?;

    // Insert the deployment, authoring parameter_id (the derive-from-sensor trigger was dropped);
    // the excl_deployment_site_param_slot constraint is the atomic cross-sensor guard.
    let dep_id = Uuid::new_v4();
    let dep_type = payload
        .deployment_type
        .clone()
        .unwrap_or_else(|| "permanent".to_string());
    let notes = payload
        .notes
        .clone()
        .unwrap_or_else(|| "Adopted via /sensors/{id}/adopt".to_string());
    let insert = deployments::ActiveModel {
        id: Set(dep_id),
        sensor_id: Set(sensor_id),
        site_id: Set(payload.site_id),
        parameter_id: Set(parameter_id),
        deployed_from: Set(deployed_from),
        deployed_until: Set(payload.deployed_until),
        deployment_type: Set(dep_type),
        notes: Set(Some(notes)),
        ..Default::default()
    }
    .insert(&txn)
    .await;
    if let Err(e) = insert {
        txn.rollback().await.ok();
        if is_slot_conflict(&e) {
            return Err(AppError::Conflict(
                "Another sensor is deployed to this site for this parameter over an overlapping \
                 period. Recall it first."
                    .to_string(),
            ));
        }
        return Err(AppError::Database(e));
    }
    // Re-chain the timeline and backfill parameter_id (reprocess sets site_id/deployment_id but not
    // parameter_id, aggregates group by parameter) inside the same transaction as the deployment
    // insert, so a failure can't leave a half-applied adopt. The reprocess itself is a post-commit
    // tracked job (heavy, async, retryable).
    recompute_deployed_until(&txn, sensor_id).await?;
    claim_unparameterised(&txn, sensor_id, parameter_id).await?;
    txn.commit().await?;

    // Slot-scoped reprocess re-attributes the (site, parameter) by deployment window, so a backdated
    // deployed_from stamps the sensor onto previously unattributed (sensor_id NULL) history. The
    // per-sensor pass then reconciles the sensor's own rows at any vacated slot.
    let adopt_site_id = payload.site_id;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "manual_adopt",
        Some(sensor_id),
        Some(dep_id),
        &serde_json::json!({
            "site_id": adopt_site_id,
            "parameter_id": parameter_id,
            "sensor_id": sensor_id,
        }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue adopt job".to_string()))?;

    Ok(Json(AdoptResponse {
        deployment_id: dep_id,
        sensor_id,
        site_id: payload.site_id,
        parameter_id,
        site_parameter_id,
        site_parameter_created,
        deployed_from,
        deployed_until: payload.deployed_until,
        job_id,
    }))
}

/// Suggested deploy dates for a sensor: now, the end of its last deployment, and its first reading.
/// The two dates an adopt suggestion is drawn from, either of which may be absent.
#[derive(FromQueryResult)]
struct SuggestionRow {
    end_last: Option<DateTime<chrono::FixedOffset>>,
    first_reading: Option<DateTime<chrono::FixedOffset>>,
}

#[utoipa::path(
    get,
    path = "/api/sensors/{sensor_id}/adopt_suggestions",
    params(("sensor_id" = Uuid, Path, description = "Sensor UUID")),
    responses((status = 200, description = "Suggested dates", body = AdoptSuggestion)),
    tag = "sensors"
)]
pub async fn adopt_suggestions(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
) -> AppResult<Json<AdoptSuggestion>> {
    let db = &app_state.db;
    // A project-scoped key only sees adopt suggestions for a sensor deployed within its project.
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }
    let scalar = |q: SelectStatement| {
        SimpleExpr::SubQuery(None, Box::new(SubQueryStatement::SelectStatement(q)))
    };
    let (sql, values) = SeaQuery::select()
        .expr_as(
            scalar(
                SeaQuery::select()
                    .expr(Func::max(Expr::cust(
                        "COALESCE(deployed_until, deployed_from)",
                    )))
                    .from(deployments::Entity)
                    .and_where(Expr::col(deployments::Column::SensorId).eq(sensor_id))
                    .take(),
            ),
            Alias::new("end_last"),
        )
        .expr_as(
            scalar(
                SeaQuery::select()
                    .expr(Func::min(Expr::col(readings::Column::Time)))
                    .from(readings::Entity)
                    .and_where(Expr::col(readings::Column::SensorId).eq(sensor_id))
                    .take(),
            ),
            Alias::new("first_reading"),
        )
        .take()
        .build(PostgresQueryBuilder);
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    // Both columns are nullable: a sensor with no prior deployment and no readings has neither.
    let suggestions = row
        .map(|r| SuggestionRow::from_query_result(&r, ""))
        .transpose()?;
    let (end_of_last_deployment, first_reading) = suggestions.map_or((None, None), |s| {
        (
            s.end_last.map(|t| t.with_timezone(&Utc)),
            s.first_reading.map(|t| t.with_timezone(&Utc)),
        )
    });
    Ok(Json(AdoptSuggestion {
        now: Utc::now(),
        end_of_last_deployment,
        first_reading,
    }))
}

/// Swap one sensor for another in a (site, parameter) slot: end the outgoing sensor's deployment and
/// start the incoming sensor's at the same instant, in one transaction. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/actions/swap",
    request_body = SwapRequest,
    responses(
        (status = 200, description = "Swap complete; returns deployments + tracked jobs", body = SwapResponse),
        (status = 400, description = "Sensors measure different parameters"),
        (status = 409, description = "Slot conflict"),
    ),
    tag = "sensors"
)]
pub async fn swap_sensors(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<SwapRequest>,
) -> AppResult<Json<SwapResponse>> {
    let db = &app_state.db;
    // The target slot, plus the incoming instrument, which this call recalls from wherever it is
    // deployed now. Both are refused outside the caller's grants, before any write.
    scope::require_sites_in_scope(db, &scope, &[payload.site_id]).await?;
    scope::require_target_in_scope(
        &scope,
        &scope::project_of_sensor(db, payload.incoming_sensor_id).await?,
        scope::Unowned::Allow,
        "Sensor",
    )?;
    crate::routes::private::sensors::service::require_measuring_instrument(
        db,
        payload.incoming_sensor_id,
        "deployed to a site",
    )
    .await?;
    let parameter_id = resolve_swap_parameter(
        db,
        payload.outgoing_sensor_id,
        payload.site_id,
        payload.parameter_id,
    )
    .await?;
    let at = payload.at.unwrap_or_else(Utc::now);

    let txn = db.begin().await?;
    // The change-audit trigger reads the writer from the transaction it fires in.
    crate::common::actor::declare(&txn).await?;
    // The readings parameter_id backfill below reaches compressed chunks.
    crate::common::bulk_write::lift_decompression_cap(&txn).await?;
    let (site_parameter_id, _created) = resolve_or_create_site_parameter(
        &txn,
        payload.site_id,
        parameter_id,
        payload.create_site_parameter,
    )
    .await?;

    // Recall the INCOMING sensor's open deployment for THIS PARAMETER at the swap instant, so it can't
    // end up double-open for the channel (twin of the outgoing recall + the adopt before_create hook).
    // Scoped to the parameter so swapping one channel doesn't recall the instrument's other channels.
    deployments::Entity::update_many()
        .col_expr(deployments::Column::DeployedUntil, Expr::value(Some(at)))
        .filter(deployments::Column::SensorId.eq(payload.incoming_sensor_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .filter(deployments::Column::DeployedUntil.is_null())
        .exec(&txn)
        .await?;

    // End the outgoing sensor's open deployment at THIS (site, parameter) slot only, a multi-channel
    // outgoing instrument keeps its other channels running.
    // No row is no deployment to end, which is the open case the swap allows.
    let ended_deployment_id: Option<Uuid> = deployments::Entity::find()
        .filter(deployments::Column::SensorId.eq(payload.outgoing_sensor_id))
        .filter(deployments::Column::SiteId.eq(payload.site_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .filter(deployments::Column::DeployedUntil.is_null())
        .select_only()
        .column(deployments::Column::Id)
        .into_tuple::<Uuid>()
        .one(&txn)
        .await?;
    if let Some(id) = ended_deployment_id {
        deployments::Entity::update_many()
            .col_expr(deployments::Column::DeployedUntil, Expr::value(Some(at)))
            .filter(deployments::Column::Id.eq(id))
            .exec(&txn)
            .await?;
    }

    // Start the incoming sensor at the same instant; half-open windows mean no overlap.
    let started_id = Uuid::new_v4();
    let insert = deployments::ActiveModel {
        id: Set(started_id),
        sensor_id: Set(payload.incoming_sensor_id),
        site_id: Set(payload.site_id),
        parameter_id: Set(parameter_id),
        deployed_from: Set(at),
        deployment_type: Set("permanent".to_string()),
        notes: Set(Some("Swapped in via /actions/swap".to_string())),
        ..Default::default()
    }
    .insert(&txn)
    .await;
    if let Err(e) = insert {
        txn.rollback().await.ok();
        if is_slot_conflict(&e) {
            return Err(AppError::Conflict(
                "Slot still occupied at the swap instant; recall the incumbent first.".to_string(),
            ));
        }
        return Err(AppError::Database(e));
    }
    // Re-chain both sensors' timelines, relink the feed to the incoming sensor (so FUTURE ingest
    // stamps B, the stream's frozen sensor_id is only a hint; the deployment timeline is
    // authoritative), and backfill parameter_id, all inside the swap transaction so a failure can't
    // leave a half-applied swap. The handover reprocess is a post-commit tracked job.
    recompute_deployed_until(&txn, payload.outgoing_sensor_id).await?;
    recompute_deployed_until(&txn, payload.incoming_sensor_id).await?;
    data_streams::models::Entity::update_many()
        .col_expr(
            data_streams::models::Column::SensorId,
            Expr::value(Some(payload.incoming_sensor_id)),
        )
        .col_expr(
            data_streams::models::Column::UpdatedAt,
            Expr::current_timestamp(),
        )
        .filter(data_streams::models::Column::SiteParameterId.eq(site_parameter_id))
        .exec(&txn)
        .await?;
    claim_unparameterised(&txn, payload.incoming_sensor_id, parameter_id).await?;
    txn.commit().await?;

    // Per-(site,parameter) handover reprocess: re-owns existing readings to whichever sensor's
    // deployment window covers each time, so the outgoing sensor's post-swap readings re-attribute
    // to the incoming sensor (a per-sensor reprocess can't, since those rows still carry sensor A).
    let site_id = payload.site_id;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "sensor_swap",
        None,
        Some(site_parameter_id),
        &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue swap job".to_string()))?;

    Ok(Json(SwapResponse {
        ended_deployment_id,
        started_deployment_id: started_id,
        site_id: payload.site_id,
        parameter_id,
        at,
        outgoing_job_id: None,
        incoming_job_id: job_id,
    }))
}

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct CurveUsageRow {
    id: Uuid,
    n: i64,
    first: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    last: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

#[derive(FromQueryResult)]
struct StreamRefRow {
    sensor_id: Uuid,
    id: Uuid,
    source_system: String,
    source_key: String,
    measurement_type: Option<String>,
    site_name: Option<String>,
    parameter_code: Option<String>,
}

#[derive(FromQueryResult)]
struct CurvePointRow {
    time: sea_orm::prelude::DateTimeWithTimeZone,
    replicate_index: i16,
    raw_value: f64,
    calibrated_value: Option<f64>,
    is_flagged: bool,
    site_name: Option<String>,
    parameter_code: Option<String>,
}

#[derive(FromQueryResult)]
struct SensorCurveUsageRow {
    curve_id: Uuid,
    n: i64,
    first: Option<sea_orm::prelude::DateTimeWithTimeZone>,
    last: Option<sea_orm::prelude::DateTimeWithTimeZone>,
}

/// Every instrument that owns a standard curve or feeds a stream, with its curves' usage counts
/// and the streams naming it. `read_data`.
#[utoipa::path(
    get,
    path = "/api/instruments/overview",
    responses((status = 200, body = InstrumentsOverviewResponse)),
    tag = "sensors"
)]
pub async fn get_instruments_overview(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<InstrumentsOverviewResponse>> {
    let db = &state.db;
    if scope.sql_project_array().is_some() {
        return Err(AppError::Forbidden(
            "The instruments overview is a cross-project view; a project-scoped token cannot read it"
                .to_string(),
        ));
    }

    // Per-curve usage in one pass over the partial index (standard_curve_id IS NOT NULL is a
    // small fraction of readings).
    #[derive(Debug)]
    struct Usage {
        count: i64,
        first: Option<DateTime<Utc>>,
        last: Option<DateTime<Utc>>,
    }
    let mut usage: HashMap<Uuid, Usage> = HashMap::new();
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col(readings::Column::StandardCurveId),
            Alias::new("id"),
        )
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("n"))
        .expr_as(
            Func::min(Expr::col(readings::Column::Time)),
            Alias::new("first"),
        )
        .expr_as(
            Func::max(Expr::col(readings::Column::Time)),
            Alias::new("last"),
        )
        .from(readings::Entity)
        .and_where(Expr::col(readings::Column::StandardCurveId).is_not_null())
        .add_group_by([Expr::col(readings::Column::StandardCurveId)])
        .take()
        .build(PostgresQueryBuilder);
    for row in db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
    {
        let row = CurveUsageRow::from_query_result(&row, "")?;
        usage.insert(
            row.id,
            Usage {
                count: row.n,
                first: row.first.map(|t| t.with_timezone(&Utc)),
                last: row.last.map(|t| t.with_timezone(&Utc)),
            },
        );
    }

    let mut curves_by_sensor: HashMap<Uuid, Vec<CurveOverview>> = HashMap::new();
    for c in standard_curves::Entity::find().all(db).await? {
        let u = usage.get(&c.id);
        curves_by_sensor
            .entry(c.sensor_id)
            .or_default()
            .push(CurveOverview {
                id: c.id,
                name: c.name,
                slope: c.slope,
                intercept: c.intercept,
                r_squared: c.r_squared,
                source_system: c.source_system,
                source_key: c.source_key,
                created_at: Some(c.created_at.with_timezone(&Utc)),
                reading_count: u.map_or(0, |u| u.count),
                first_used: u.and_then(|u| u.first),
                last_used: u.and_then(|u| u.last),
            });
    }
    for curves in curves_by_sensor.values_mut() {
        curves.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    }

    // Streams naming an instrument, with the paired slot's names resolved in the same pass.
    let mut streams_by_sensor: HashMap<Uuid, Vec<InstrumentStreamRef>> = HashMap::new();
    for row in db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT ds.sensor_id, ds.id, ds.source_system, ds.source_key, ds.measurement_type,
                    s.name AS site_name, p.code AS parameter_code
             FROM data_streams ds
             LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
             LEFT JOIN sites s ON s.id = sp.site_id
             LEFT JOIN parameters p ON p.id = sp.parameter_id
             WHERE ds.sensor_id IS NOT NULL
             ORDER BY ds.source_system, ds.source_key"
                .to_string(),
        ))
        .await?
    {
        let row = StreamRefRow::from_query_result(&row, "")?;
        streams_by_sensor
            .entry(row.sensor_id)
            .or_default()
            .push(InstrumentStreamRef {
                id: row.id,
                source_system: row.source_system,
                source_key: row.source_key,
                measurement_type: row.measurement_type,
                site_name: row.site_name,
                parameter_code: row.parameter_code,
            });
    }

    let relevant: Vec<sensors::Model> = sensors::Entity::find()
        .all(db)
        .await?
        .into_iter()
        .filter(|s| curves_by_sensor.contains_key(&s.id) || streams_by_sensor.contains_key(&s.id))
        .collect();
    let mut instruments: Vec<InstrumentOverview> = relevant
        .into_iter()
        .map(|s| {
            let kind = InstrumentKind::of(Some(s.kind.as_str()), s.is_lab_instrument);
            InstrumentOverview {
                curves: curves_by_sensor.remove(&s.id).unwrap_or_default(),
                streams: streams_by_sensor.remove(&s.id).unwrap_or_default(),
                id: s.id,
                name: s.name,
                serial_number: s.serial_number,
                manufacturer: s.manufacturer,
                model: s.model,
                is_lab_instrument: kind.is_lab_instrument(),
                kind: kind.as_str().to_string(),
                source_system: s.source_system,
                source_key: s.source_key,
            }
        })
        .collect();
    // Lab instruments first: they own the curves this tab exists for. `kind`, not the flag, which
    // reads every bookkeeping row as lab too and puts them in with the spectrophotometers.
    instruments.sort_by(|a, b| {
        (b.kind == "lab")
            .cmp(&(a.kind == "lab"))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(Json(InstrumentsOverviewResponse { instruments }))
}

const MAX_POINTS: i64 = 2000;

/// `GET /standard_curves/{id}/usage`: the readings a curve corrected. `read_data`.
#[utoipa::path(
    get,
    path = "/api/standard_curves/{id}/usage",
    params(("id" = Uuid, Path, description = "Standard curve UUID")),
    responses(
        (status = 200, body = CurveUsageResponse),
        (status = 404, description = "Curve not found"),
    ),
    tag = "sensors"
)]
pub async fn get_curve_usage(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(curve_id): Path<Uuid>,
) -> AppResult<Json<CurveUsageResponse>> {
    let db = &state.db;
    let curve = standard_curves::Entity::find_by_id(curve_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Standard curve not found".to_string()))?;
    if !sensor_in_scope(db, &scope, curve.sensor_id).await? {
        return Err(AppError::NotFound("Standard curve not found".to_string()));
    }

    let count = i64::try_from(
        readings::Entity::find()
            .filter(readings::Column::StandardCurveId.eq(curve_id))
            .count(db)
            .await?,
    )
    .unwrap_or(i64::MAX);

    let r = Alias::new("r");
    let site = Alias::new("s");
    let param = Alias::new("p");
    let (sql, values) = SeaQuery::select()
        .column((r.clone(), readings::Column::Time))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::RawValue))
        .column((r.clone(), readings::Column::CalibratedValue))
        .expr_as(
            Func::coalesce([
                Expr::col((r.clone(), readings::Column::IsFlagged)),
                Expr::value(false),
            ]),
            Alias::new("is_flagged"),
        )
        .expr_as(
            Expr::col((site.clone(), sites::models::Column::Name)),
            Alias::new("site_name"),
        )
        .expr_as(
            Expr::col((param.clone(), parameters::models::Column::Code)),
            Alias::new("parameter_code"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::LeftJoin,
            sites::models::Entity,
            site.clone(),
            Expr::col((site, sites::models::Column::Id))
                .equals((r.clone(), readings::Column::SiteId)),
        )
        .join_as(
            JoinType::LeftJoin,
            parameters::models::Entity,
            param.clone(),
            Expr::col((param, parameters::models::Column::Id))
                .equals((r.clone(), readings::Column::ParameterId)),
        )
        .and_where(Expr::col((r.clone(), readings::Column::StandardCurveId)).eq(curve_id))
        .order_by((r.clone(), readings::Column::Time), Order::Desc)
        .order_by((r, readings::Column::ReplicateIndex), Order::Asc)
        .limit(MAX_POINTS.unsigned_abs())
        .take()
        .build(PostgresQueryBuilder);
    let points = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .into_iter()
        .map(|row| {
            let row = CurvePointRow::from_query_result(&row, "")?;
            Ok(CurveUsagePoint {
                time: row.time.with_timezone(&Utc),
                replicate_index: row.replicate_index,
                raw_value: row.raw_value,
                calibrated_value: row.calibrated_value,
                is_flagged: row.is_flagged,
                site_name: row.site_name,
                parameter_code: row.parameter_code,
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(CurveUsageResponse {
        curve_id,
        sensor_id: curve.sensor_id,
        slope: curve.slope,
        intercept: curve.intercept,
        reading_count: count,
        points,
    }))
}

/// `GET /sensors/{id}/curve_usage`: the usage figures the overview reports, for one instrument.
/// A curve nothing was corrected with is reported at zero rather than omitted. `read_data`.
#[utoipa::path(
    get,
    path = "/api/sensors/{id}/curve_usage",
    params(("id" = Uuid, Path, description = "Sensor UUID")),
    responses(
        (status = 200, body = SensorCurveUsageResponse),
        (status = 404, description = "Sensor not found"),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_curve_usage(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
) -> AppResult<Json<SensorCurveUsageResponse>> {
    let db = &state.db;
    if sensors::Entity::find_by_id(sensor_id)
        .one(db)
        .await?
        .is_none()
        || !sensor_in_scope(db, &scope, sensor_id).await?
    {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }

    let c = Alias::new("c");
    let r = Alias::new("r");
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((c.clone(), standard_curves::Column::Id)),
            Alias::new("curve_id"),
        )
        .expr_as(
            Func::count(Expr::col((r.clone(), readings::Column::StandardCurveId))),
            Alias::new("n"),
        )
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("first"),
        )
        .expr_as(
            Func::max(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("last"),
        )
        .from_as(standard_curves::Entity, c.clone())
        .join_as(
            JoinType::LeftJoin,
            readings::Entity,
            r.clone(),
            Expr::col((r, readings::Column::StandardCurveId))
                .equals((c.clone(), standard_curves::Column::Id)),
        )
        .and_where(Expr::col((c.clone(), standard_curves::Column::SensorId)).eq(sensor_id))
        .add_group_by([Expr::col((c, standard_curves::Column::Id))])
        .take()
        .build(PostgresQueryBuilder);
    let usage = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .into_iter()
        .map(|row| {
            let row = SensorCurveUsageRow::from_query_result(&row, "")?;
            Ok(SensorCurveUsage {
                curve_id: row.curve_id,
                reading_count: row.n,
                first_used: row.first.map(|t| t.with_timezone(&Utc)),
                last_used: row.last.map(|t| t.with_timezone(&Utc)),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(SensorCurveUsageResponse { sensor_id, usage }))
}

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
/// The series has two shapes: every point carries [`SeriesPoint`], and a bucketed one carries
/// [`BucketExtent`] as well, which is why they are two structs and not one with optional halves.
#[derive(FromQueryResult)]
struct SeriesPoint {
    time: DateTime<chrono::FixedOffset>,
    raw_value: Option<f64>,
    calibrated_value: Option<f64>,
    site_id: Option<Uuid>,
}

#[derive(FromQueryResult)]
struct BucketExtent {
    raw_min: Option<f64>,
    raw_max: Option<f64>,
    cal_min: Option<f64>,
    cal_max: Option<f64>,
}

#[derive(FromQueryResult)]
struct BandRow {
    deployment_id: Uuid,
    site_id: Uuid,
    site_name: Option<String>,
    deployed_from: DateTime<chrono::FixedOffset>,
    deployed_until: Option<DateTime<chrono::FixedOffset>>,
}

/// The parameter a series resolves to. `default_units` is nullable in the catalog; the id is not,
/// because both queries select it from the row they matched on.
#[derive(FromQueryResult)]
struct SeriesParameterRow {
    parameter_id: Uuid,
    default_units: Option<String>,
}

/// The parameter this response is about, and the units that go with it.
///
/// One resolution step feeds both the reported identity and every query's predicate, so the
/// series can no longer hold a different quantity from the one the response names. A sensor with
/// no deployment and no explicit request resolves to `None`, and the series is then unfiltered,
/// which keeps a never-deployed instrument's plot from coming back empty.
async fn resolve_series_parameter(
    db: &sea_orm::DatabaseConnection,
    sensor_id: Uuid,
    requested: Option<Uuid>,
) -> AppResult<(Option<Uuid>, Option<String>)> {
    if let Some(parameter_id) = requested {
        let parameter = parameters::Entity::find_by_id(parameter_id)
            .one(db)
            .await?
            .ok_or_else(|| AppError::BadRequest(format!("Unknown parameter {parameter_id}")))?;
        return Ok((Some(parameter.id), Some(parameter.default_units)));
    }
    let Some(row) = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT d.parameter_id, p.default_units
              FROM sensor_deployments d
              JOIN parameters p ON p.id = d.parameter_id
              WHERE d.sensor_id = $1
              ORDER BY d.deployed_from DESC
              LIMIT 1",
            [sensor_id.into()],
        ))
        .await?
    else {
        return Ok((None, None));
    };

    let resolved = SeriesParameterRow::from_query_result(&row, "")?;
    Ok((Some(resolved.parameter_id), resolved.default_units))
}

/// Per-sensor time series (raw + calibrated), for the sensor detail plot. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/sensors/{id}/readings",
    params(
        ("id" = Uuid, Path, description = "Sensor UUID"),
        SensorReadingsQuery
    ),
    responses(
        (status = 200, description = "Sensor readings (per-point, or time-bucketed when resolution is set)", body = SensorReadingsResponse),
        (status = 400, description = "Invalid resolution"),
        (status = 404, description = "Sensor not found"),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_readings(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
    Query(query): Query<SensorReadingsQuery>,
) -> AppResult<Json<SensorReadingsResponse>> {
    let db = &state.db;
    let include_raw = query.include_raw.unwrap_or(true);

    // A sensor has no intrinsic parameter; a multi-parameter instrument holds one deployment per
    // channel. The channel resolved here is the one the response names, the one whose units it
    // reports, and the one every query below filters on.
    if Entity::find_by_id(sensor_id)
        .select_only()
        .column(Column::Id)
        .into_tuple::<Uuid>()
        .one(db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }
    let (parameter_id, units) = resolve_series_parameter(db, sensor_id, query.parameter_id).await?;

    // A project-scoped key may only read a sensor that has been deployed within its project, and
    // even then only sees the readings attributed to in-project sites. A sensor never deployed in
    // the project is reported as not-found (no cross-project existence disclosure).
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }
    // `<col> IN (<scope project's sites>)` when the caller is confined to a project, nothing
    // otherwise. Applied to every readings query below so the temporal extent and per-point series
    // are both confined to the project.
    let scope_filter = |col: Expr| {
        scope.sql_project_array().map(|projects| {
            col.in_subquery(
                SeaQuery::select()
                    .column(sites::models::Column::Id)
                    .from(sites::models::Entity)
                    .and_where(Expr::cust_with_values("project_id = ANY($1)", [projects]))
                    .take(),
            )
        })
    };
    // Confines a query to the resolved channel. A sensor with no resolved parameter is unfiltered,
    // so its plot still shows whatever is attributed to it.
    let param_filter = |col: Expr| parameter_id.map(|pid| col.eq(pid));
    let with = |mut cond: Condition, predicate: Option<Expr>| {
        if let Some(predicate) = predicate {
            cond = cond.add(predicate);
        }
        cond
    };

    let resolution = query.resolution.as_deref().unwrap_or("raw");
    let bucket = if resolution == "raw" {
        None
    } else {
        Some(bucket_interval(resolution_of(resolution).ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid resolution '{resolution}' (expected raw|hourly|6hourly|12hourly|daily|weekly|monthly)"
            ))
        })?))
    };

    // Full reading extent for this sensor on the served channel (drives the UI slider bounds,
    // independent of the window).
    let mut extent_rows = Condition::all().add(Expr::col(readings::Column::SensorId).eq(sensor_id));
    extent_rows = with(
        extent_rows,
        scope_filter(Expr::col(readings::Column::SiteId)),
    );
    extent_rows = with(
        extent_rows,
        param_filter(Expr::col(readings::Column::ParameterId)),
    );
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Func::min(Expr::col(readings::Column::Time)),
            Alias::new("data_start"),
        )
        .expr_as(
            Func::max(Expr::col(readings::Column::Time)),
            Alias::new("data_end"),
        )
        .from(readings::Entity)
        .cond_where(extent_rows)
        .take()
        .build(PostgresQueryBuilder);
    let extent = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    let data_start = extent
        .as_ref()
        .and_then(|r| {
            r.try_get::<DateTime<chrono::FixedOffset>>("", "data_start")
                .ok()
        })
        .map(|t| t.with_timezone(&Utc));
    let data_end = extent
        .as_ref()
        .and_then(|r| {
            r.try_get::<DateTime<chrono::FixedOffset>>("", "data_end")
                .ok()
        })
        .map(|t| t.with_timezone(&Utc));

    // Earliest reading at the sensor's OPEN deployment slot (same site + parameter, any sensor_id),
    // the true backdate target. Unattributed history (sensor_id NULL) is invisible to `data_start`
    // but lives here, and backdating `deployed_from` to it lets the slot reprocess claim it.
    let r_ = Alias::new("r");
    let d_ = Alias::new("d");
    let mut slot_rows = Condition::all()
        .add(Expr::cust("r.site_id = d.site_id"))
        .add(Expr::cust("r.parameter_id = d.parameter_id"));
    slot_rows = with(
        slot_rows,
        scope_filter(Expr::col((r_.clone(), readings::Column::SiteId))),
    );
    slot_rows = with(
        slot_rows,
        param_filter(Expr::col((d_.clone(), deployments::Column::ParameterId))),
    );
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Func::min(Expr::col((r_.clone(), readings::Column::Time))),
            Alias::new("slot_start"),
        )
        .from_as(readings::Entity, r_.clone())
        .join_as(
            JoinType::InnerJoin,
            deployments::Entity,
            d_.clone(),
            Condition::all()
                .add(Expr::col((d_.clone(), deployments::Column::SensorId)).eq(sensor_id))
                .add(Expr::col((d_.clone(), deployments::Column::DeployedUntil)).is_null()),
        )
        .cond_where(slot_rows)
        .take()
        .build(PostgresQueryBuilder);
    let slot_data_start = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .and_then(|r| {
            r.try_get::<DateTime<chrono::FixedOffset>>("", "slot_start")
                .ok()
        })
        .map(|t| t.with_timezone(&Utc));

    crate::routes::private::readings::service::validate_measurement_type(
        query.measurement_type.as_deref().filter(|m| !m.is_empty()),
    )?;

    // Every arm shares the sensor, the caller's scope, the resolved channel and the window.
    let mut shared = Condition::all().add(Expr::col(readings::Column::SensorId).eq(sensor_id));
    shared = with(shared, scope_filter(Expr::col(readings::Column::SiteId)));
    shared = with(
        shared,
        param_filter(Expr::col(readings::Column::ParameterId)),
    );
    if let Some(start) = query.start {
        shared = shared.add(Expr::col(readings::Column::Time).gte(start));
    }
    if let Some(end) = query.end {
        shared = shared.add(Expr::col(readings::Column::Time).lte(end));
    }
    if bucket.is_none()
        && let Some(mt) = query.measurement_type.as_deref().filter(|m| !m.is_empty())
    {
        shared = shared.add(Expr::cust_with_values(
            "(measurement_type = $1 OR ($1 = 'continuous' AND measurement_type IS NULL))",
            [mt.to_string()],
        ));
    }
    // Continuous and derived rows live at replicate_index 0, so a plain equality keeps the
    // chunk-ordered scan.
    let continuous = || {
        Condition::all()
            .add(Expr::col(readings::Column::ReplicateIndex).eq(0))
            .add(Expr::cust("measurement_type IS DISTINCT FROM 'spot'"))
    };

    // Aggregated resolutions time-bucket the readings (avg + min/max of raw and calibrated),
    // excluding flagged points and spot readings to match continuous-aggregate semantics; raw mode
    // returns per-point values including flagged.
    let query_statement = if let Some(interval) = bucket {
        let bucketed =
            |sql: &str, name: &str| (Expr::cust(sql.to_string()), Alias::new(name.to_string()));
        let mut agg = SeaQuery::select();
        agg.expr_as(
            Expr::cust_with_values("time_bucket($1::interval, time)", [interval.to_string()]),
            Alias::new("time"),
        );
        for (expr, name) in [
            bucketed("avg(raw_value)", "raw_value"),
            bucketed("min(raw_value)", "raw_min"),
            bucketed("max(raw_value)", "raw_max"),
            bucketed("avg(calibrated_value)", "calibrated_value"),
            bucketed("min(calibrated_value)", "cal_min"),
            bucketed("max(calibrated_value)", "cal_max"),
            bucketed("last(site_id, time)", "site_id"),
        ] {
            agg.expr_as(expr, name);
        }
        agg.from(readings::Entity)
            .cond_where(
                shared
                    .clone()
                    .add(continuous())
                    .add(Expr::cust("is_flagged IS NOT TRUE")),
            )
            // By ordinal: `time` is both the bucket's alias and a base column, and Postgres
            // resolves the bare name to the column, which groups nothing.
            .add_group_by([Expr::cust("1")])
            .order_by_expr(Expr::cust("1"), Order::Asc);
        agg.take()
    } else {
        SeaQuery::select()
            .column(readings::Column::Time)
            .column(readings::Column::RawValue)
            .column(readings::Column::CalibratedValue)
            .column(readings::Column::SiteId)
            .from(readings::Entity)
            .cond_where(shared.clone().add(continuous()))
            .order_by(readings::Column::Time, Order::Asc)
            .take()
    };

    // The raw mode runs a second statement for the spot subset; both are time-ascending and are
    // merged below, so the continuous statement keeps the chunk-ordered scan a single UNION with
    // an outer sort would forfeit.
    //
    // A spot instant is the replicate group `(stream_id, time)`, collapsed to its lowest unflagged
    // replicate (flagged-only groups surface their flagged row: this is the instrument diagnostic
    // view, which keeps flagged points visible). The DISTINCT ON is confined to the spot subset,
    // whose row counts are small.
    let spot_statement = bucket.is_none().then(|| {
        let group = SeaQuery::select()
            .distinct_on([readings::Column::StreamId, readings::Column::Time])
            .column(readings::Column::Time)
            .column(readings::Column::RawValue)
            .column(readings::Column::CalibratedValue)
            .column(readings::Column::SiteId)
            .from(readings::Entity)
            .cond_where(
                shared
                    .clone()
                    .add(Expr::col(readings::Column::MeasurementType).eq("spot"))
                    .add(Expr::col(readings::Column::WithdrawnAt).is_null()),
            )
            .order_by(readings::Column::StreamId, Order::Asc)
            .order_by(readings::Column::Time, Order::Asc)
            .order_by_expr(Expr::cust("(is_flagged IS TRUE)"), Order::Asc)
            .order_by(readings::Column::ReplicateIndex, Order::Asc)
            .take();
        let sp = Alias::new("sp");
        SeaQuery::select()
            .columns([
                (sp.clone(), Alias::new("time")),
                (sp.clone(), Alias::new("raw_value")),
                (sp.clone(), Alias::new("calibrated_value")),
                (sp.clone(), Alias::new("site_id")),
            ])
            .from_subquery(group, sp)
            .order_by(Alias::new("time"), Order::Asc)
            .take()
    });

    let (sql, values) = query_statement.build(PostgresQueryBuilder);
    let mut rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    if let Some(spot_statement) = spot_statement {
        let (sql, values) = spot_statement.build(PostgresQueryBuilder);
        let spot_rows = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                sql,
                values,
            ))
            .await?;
        if !spot_rows.is_empty() {
            // Merge the two time-ascending statements into one time-ascending series.
            let mut merged = Vec::with_capacity(rows.len() + spot_rows.len());
            let time_of = |row: &sea_orm::QueryResult| {
                row.try_get::<DateTime<chrono::FixedOffset>>("", "time")
                    .map(|t| t.with_timezone(&Utc))
            };
            let (mut a, mut b) = (
                rows.into_iter().peekable(),
                spot_rows.into_iter().peekable(),
            );
            while let (Some(x), Some(y)) = (a.peek(), b.peek()) {
                if time_of(x)? <= time_of(y)? {
                    merged.push(a.next().unwrap());
                } else {
                    merged.push(b.next().unwrap());
                }
            }
            merged.extend(a);
            merged.extend(b);
            rows = merged;
        }
    }

    let aggregated = bucket.is_some();
    let mut times = Vec::with_capacity(rows.len());
    let mut raw = Vec::with_capacity(rows.len());
    let mut calibrated = Vec::with_capacity(rows.len());
    let mut raw_min = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut raw_max = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut calibrated_min = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut calibrated_max = Vec::with_capacity(if aggregated { rows.len() } else { 0 });
    let mut site_ids = Vec::with_capacity(rows.len());
    for row in &rows {
        let point = SeriesPoint::from_query_result(row, "")?;
        times.push(point.time.with_timezone(&Utc));
        raw.push(if include_raw { point.raw_value } else { None });
        calibrated.push(point.calibrated_value);
        site_ids.push(point.site_id);
        if aggregated {
            let extent = BucketExtent::from_query_result(row, "")?;
            raw_min.push(extent.raw_min);
            raw_max.push(extent.raw_max);
            calibrated_min.push(extent.cal_min);
            calibrated_max.push(extent.cal_max);
        }
    }

    Ok(Json(SensorReadingsResponse {
        sensor_id,
        parameter_id,
        units,
        resolution: if aggregated {
            resolution.to_string()
        } else {
            "raw".to_string()
        },
        times,
        raw,
        calibrated,
        raw_min,
        raw_max,
        calibrated_min,
        calibrated_max,
        site_ids,
        data_start,
        data_end,
        slot_data_start,
    }))
}

/// Deployment timeline for a sensor (site-assignment bands), sourced from the deployment table so
/// it is correct mid-reprocess. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/sensors/{id}/deployment_bands",
    params(
        ("id" = Uuid, Path, description = "Sensor UUID"),
        SensorBandsQuery
    ),
    responses(
        (status = 200, description = "Deployment bands", body = SensorDeploymentBandsResponse),
    ),
    tag = "sensors"
)]
pub async fn get_sensor_deployment_bands(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(sensor_id): Path<Uuid>,
    Query(query): Query<SensorBandsQuery>,
) -> AppResult<Json<SensorDeploymentBandsResponse>> {
    let db = &state.db;

    // A project-scoped key only sees a sensor deployed within its project, and only the bands at
    // in-project sites (a field sensor can move between projects).
    if !sensor_in_scope(db, &scope, sensor_id).await? {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }

    let mut sql = String::from(
        r"SELECT d.id AS deployment_id, d.site_id, s.name AS site_name,
                 d.deployed_from, d.deployed_until
          FROM sensor_deployments d
          JOIN sites s ON s.id = d.site_id
          WHERE d.sensor_id = $1",
    );
    let mut values: Vec<sea_orm::Value> = vec![sensor_id.into()];
    if let Some(projects) = scope.sql_project_array() {
        values.push(projects);
        sql.push_str(&format!(" AND s.project_id = ANY(${})", values.len()));
    }
    if let Some(end) = query.end {
        values.push(end.into());
        sql.push_str(&format!(" AND d.deployed_from < ${}", values.len()));
    }
    if let Some(start) = query.start {
        values.push(start.into());
        sql.push_str(&format!(
            " AND COALESCE(d.deployed_until, 'infinity'::timestamptz) > ${}",
            values.len()
        ));
    }
    sql.push_str(" ORDER BY d.deployed_from ASC");

    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            &sql,
            values,
        ))
        .await?;

    let bands = rows
        .iter()
        .map(|row| -> AppResult<SensorDeploymentBand> {
            let band = BandRow::from_query_result(row, "")?;
            Ok(SensorDeploymentBand {
                deployment_id: band.deployment_id,
                site_id: band.site_id,
                site_name: band.site_name,
                from: band.deployed_from.with_timezone(&Utc),
                until: band.deployed_until.map(|u| u.with_timezone(&Utc)),
            })
        })
        .collect::<AppResult<Vec<_>>>()?;

    Ok(Json(SensorDeploymentBandsResponse { sensor_id, bands }))
}

/// Store a source's instrument register as proposals. Requires `write_metadata`.
///
/// Nothing is created: an instrument exists once a pairing plan an operator validated creates it
/// (Q134). A row already admitted as an instrument under the same provenance is skipped rather than
/// re-proposed, so a sync does not offer back what the operator already took.
#[utoipa::path(
    post,
    path = "/api/sensors/proposals",
    request_body = ProposeInstrumentsRequest,
    responses((status = 200, description = "Register stored", body = ProposeInstrumentsResponse)),
    tag = "sensors"
)]
pub async fn propose_instruments(
    State(state): State<AppState>,
    Json(payload): Json<ProposeInstrumentsRequest>,
) -> AppResult<Json<ProposeInstrumentsResponse>> {
    let source_system = crate::common::provenance::source_system(&payload.source_system)?;
    let mut stored = 0usize;
    let mut already_admitted = 0usize;
    for instrument in &payload.instruments {
        let key = instrument.source_key.trim();
        if key.is_empty() {
            return Err(AppError::BadRequest(
                "source_key identifies the instrument and cannot be empty".to_string(),
            ));
        }
        let admitted = sensors::Entity::find()
            .filter(sensors::Column::SourceSystem.eq(source_system.clone()))
            .filter(sensors::Column::SourceKey.eq(key.to_string()))
            .one(&state.db)
            .await?;
        if admitted.is_some() {
            already_admitted += 1;
            continue;
        }
        proposal::Entity::insert(proposal::ActiveModel {
            id: Set(Uuid::new_v4()),
            source_system: Set(source_system.clone()),
            source_key: Set(key.to_string()),
            name: Set(instrument.name.clone()),
            serial_number: Set(instrument.serial_number.clone()),
            manufacturer: Set(instrument.manufacturer.clone()),
            model: Set(instrument.model.clone()),
            notes: Set(instrument.notes.clone()),
            is_lab_instrument: Set(instrument.is_lab_instrument),
            data_frequency: Set(instrument.data_frequency.clone()),
            metadata: Set(instrument.metadata.clone()),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::columns([proposal::Column::SourceSystem, proposal::Column::SourceKey])
                .update_columns([
                    proposal::Column::Name,
                    proposal::Column::SerialNumber,
                    proposal::Column::Manufacturer,
                    proposal::Column::Model,
                    proposal::Column::Notes,
                    proposal::Column::IsLabInstrument,
                    proposal::Column::DataFrequency,
                    proposal::Column::Metadata,
                ])
                .value(proposal::Column::LastSeenAt, Expr::current_timestamp())
                .to_owned(),
        )
        .exec(&state.db)
        .await?;
        stored += 1;
    }
    Ok(Json(ProposeInstrumentsResponse {
        stored,
        already_admitted,
    }))
}

/// Classify sensors as low- or high-frequency in bulk. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/sensors/retag_frequency",
    request_body = RetagFrequencyRequest,
    responses(
        (status = 200, description = "Sensors reclassified", body = RetagFrequencyResponse),
        (status = 400, description = "Invalid data_frequency or empty sensor list"),
    ),
    tag = "sensors"
)]
pub async fn retag_frequency(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<RetagFrequencyRequest>,
) -> AppResult<Json<RetagFrequencyResponse>> {
    if req.sensor_ids.is_empty() {
        return Err(AppError::BadRequest(
            "sensor_ids must not be empty".to_string(),
        ));
    }
    // A never-deployed instrument belongs to no project, so it stays reachable as inventory;
    // one deployed only outside the caller's grants does not.
    for sensor_id in &req.sensor_ids {
        scope::require_target_in_scope(
            &scope,
            &scope::project_of_sensor(&state.db, *sensor_id).await?,
            scope::Unowned::Allow,
            "Sensor",
        )?;
    }
    if !matches!(req.data_frequency.as_str(), "high" | "low") {
        return Err(AppError::BadRequest(format!(
            "invalid data_frequency '{}' (expected high or low)",
            req.data_frequency
        )));
    }

    // `retag_existing` on "high" enqueues a `measurement_retag` whose scope reaches every stream
    // these sensors own, a replicate family included.
    if req.data_frequency == "high" {
        let families = crate::routes::private::data_streams::service::family_keys_for_sensors(
            &state.db,
            &req.sensor_ids,
        )
        .await?;
        crate::routes::private::data_streams::service::refuse_family_retag(
            &families,
            MeasurementType::Continuous.as_str(),
        )?;
    }

    let sensors_updated = Entity::update_many()
        .col_expr(
            Column::DataFrequency,
            Expr::value(req.data_frequency.clone()),
        )
        .filter(Column::Id.is_in(req.sensor_ids.clone()))
        .exec(&state.db)
        .await?
        .rows_affected;

    let job_id = if req.retag_existing {
        let target = if req.data_frequency == "low" {
            MeasurementType::Spot
        } else {
            MeasurementType::Continuous
        }
        .as_str();
        crate::routes::private::reprocessing_jobs::service::enqueue(
            &state.db,
            "measurement_retag",
            None,
            None,
            &serde_json::json!({
                "sensor_ids": req.sensor_ids,
                "target": target,
            }),
            None,
        )
        .await?
    } else {
        None
    };

    Ok(Json(RetagFrequencyResponse {
        sensors_updated,
        data_frequency: req.data_frequency,
        job_id,
    }))
}

/// The instrument views the plots overlay: a sensor's readings and deployment bands, a
/// calibration's window, and which readings a curve accounts for.
///
/// Four prefixes, one component. `/sensors`, `/sensor_calibrations`, `/standard_curves` and
/// `/instruments` are one instrument seen from four sides, and keeping their gate in one function
/// is the point: it was spread over four blocks in `service/mod.rs`, and adding a view meant
/// finding which block already carried the right layer (Q143, C272).
pub fn read_routes() -> Router<AppState> {
    Router::new()
        .route("/sensors/{id}/readings", get(get_sensor_readings))
        .route(
            "/sensors/{id}/deployment_bands",
            get(get_sensor_deployment_bands),
        )
        .route(
            "/sensor_calibrations/{id}/window",
            get(crate::routes::private::sensor_calibrations::views::get_calibration_window),
        )
        .route("/sensors/{id}/curve_usage", get(get_sensor_curve_usage))
        .route("/instruments/overview", get(get_instruments_overview))
        .route("/sensors/last_used", get(last_used_instruments))
        .route("/standard_curves/{id}/usage", get(get_curve_usage))
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_data,
        ))
}

/// What an instrument could adopt, before adopting it.
pub fn adopt_read_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/sensors/{sensor_id}/adopt_suggestions",
            get(adopt_suggestions),
        )
        .layer(middleware::from_fn(
            crate::common::middleware::require_read_metadata,
        ))
}

/// Moving an instrument: adopting a stream's history onto it, and reclassifying its cadence.
///
/// `/actions/swap` and `/actions/rollback_deployment` carry the same gate and are the same kind of
/// movement, but they are `/actions/*` and stay registered centrally with the rest of that prefix
/// (Q143).
///
/// MANAGER, or a token with `write_metadata`: deploying an instrument at a slot is instrument
/// movement, not data.
pub fn adopt_write_routes() -> Router<AppState> {
    Router::new()
        .route("/sensors/{sensor_id}/adopt", post(adopt_sensor))
        .route("/sensors/retag_frequency", post(retag_frequency))
        .layer(middleware::from_fn(
            crate::common::middleware::deny_scoped_token,
        ))
        .layer(middleware::from_fn(
            crate::common::middleware::require_manage_sensors,
        ))
}

/// `GET /sensors/last_used`, the instruments that recorded these parameters, most recently first.
///
/// Field instruments record `spot` readings, which the rollups exclude, so the answer lives in
/// `readings` and nowhere else. The ordering is done in SQL over
/// `idx_readings_spot_param_sensor_time` rather than by annotating a page that has already been
/// selected, which is what `enrich` does and why it cannot order (M204).
#[utoipa::path(
    get,
    path = "/api/sensors/last_used",
    params(LastUsedQuery),
    responses(
        (status = 200, description = "Instruments by last use", body = LastUsedResponse),
        (status = 400, description = "Neither parameter_ids nor parameter_codes given"),
    ),
    tag = "sensors"
)]
pub async fn last_used_instruments(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(query): Query<LastUsedQuery>,
) -> AppResult<Json<LastUsedResponse>> {
    let db = &state.db;
    let parameter_ids = resolve_parameter_ids(db, &query).await?;
    if parameter_ids.is_empty() {
        return Err(AppError::BadRequest(
            "Name the parameters to rank by, as parameter_ids or parameter_codes".to_string(),
        ));
    }
    let limit = query.limit.unwrap_or(20).clamp(1, 200);

    let mut conditions = Condition::all()
        .add(Expr::col(readings::Column::MeasurementType).eq("spot"))
        .add(Expr::col(readings::Column::SensorId).is_not_null())
        .add(Expr::col(readings::Column::ParameterId).is_in(parameter_ids));
    if let Some(site_id) = query.site_id {
        conditions = conditions.add(Expr::col(readings::Column::SiteId).eq(site_id));
    }
    // A project-scoped caller ranks only what its own sites recorded.
    if let Some(predicate) = crate::common::scope::project_filter(
        &scope,
        (
            crate::routes::private::sites::models::Entity,
            crate::routes::private::sites::models::Column::ProjectId,
        ),
    ) {
        conditions = conditions.add(
            Expr::col(readings::Column::SiteId).in_subquery(
                SeaQuery::select()
                    .column(crate::routes::private::sites::models::Column::Id)
                    .from(crate::routes::private::sites::models::Entity)
                    .cond_where(predicate)
                    .take(),
            ),
        );
    }

    // One row per (parameter, instrument) from the index, then the newest per instrument. The
    // group key leads with the index's own leading columns, so this is an index-only scan.
    let (sql, values) = SeaQuery::select()
        .column(readings::Column::SensorId)
        .column(readings::Column::ParameterId)
        .expr_as(
            Func::max(Expr::col(readings::Column::Time)),
            Alias::new("last_used_at"),
        )
        .from(readings::Entity)
        .cond_where(conditions)
        .add_group_by([
            Expr::col(readings::Column::SensorId),
            Expr::col(readings::Column::ParameterId),
        ])
        .order_by(Alias::new("last_used_at"), Order::Desc)
        .build(PostgresQueryBuilder);

    #[derive(FromQueryResult)]
    struct Row {
        sensor_id: Uuid,
        parameter_id: Uuid,
        last_used_at: DateTime<Utc>,
    }
    let rows = Row::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .all(db)
    .await?;

    // An instrument appears once, under whichever of the asked-about parameters it recorded most
    // recently; the rows arrive newest first, so the first one wins.
    let mut seen: HashMap<Uuid, (Uuid, DateTime<Utc>)> = HashMap::new();
    let mut order: Vec<Uuid> = Vec::new();
    for row in rows {
        if seen.contains_key(&row.sensor_id) {
            continue;
        }
        seen.insert(row.sensor_id, (row.parameter_id, row.last_used_at));
        order.push(row.sensor_id);
        if order.len() as u64 >= limit {
            break;
        }
    }

    let named: HashMap<Uuid, (Option<String>, Option<String>)> = sensors::Entity::find()
        .filter(sensors::Column::Id.is_in(order.clone()))
        .all(db)
        .await?
        .into_iter()
        .map(|s| (s.id, (s.serial_number, s.name)))
        .collect();

    let instruments = order
        .into_iter()
        .map(|sensor_id| {
            let (parameter_id, last_used_at) = seen[&sensor_id];
            let (serial_number, name) = named.get(&sensor_id).cloned().unwrap_or((None, None));
            LastUsedInstrument {
                sensor_id,
                serial_number,
                name,
                parameter_id,
                last_used_at,
            }
        })
        .collect();
    Ok(Json(LastUsedResponse { instruments }))
}

/// The parameters the caller named, by id or by code. Neither given is an empty list, which the
/// handler refuses rather than ranking every instrument in the catalog.
async fn resolve_parameter_ids<C: sea_orm::ConnectionTrait>(
    db: &C,
    query: &LastUsedQuery,
) -> AppResult<Vec<Uuid>> {
    let mut ids: Vec<Uuid> = query
        .parameter_ids
        .iter()
        .flat_map(|list| list.split(','))
        .filter_map(|part| Uuid::parse_str(part.trim()).ok())
        .collect();
    let codes: Vec<String> = query
        .parameter_codes
        .iter()
        .flat_map(|list| list.split(','))
        .map(|part| part.trim().to_lowercase())
        .filter(|part| !part.is_empty())
        .collect();
    if !codes.is_empty() {
        use crate::routes::private::parameters;
        let found = parameters::Entity::find()
            .filter(Expr::expr(Func::lower(Expr::col(parameters::Column::Code))).is_in(codes))
            .all(db)
            .await?;
        ids.extend(found.into_iter().map(|p| p.id));
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

// --- Re-deriving a sensor's readings ---

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReprocessSensorRequest {
    pub sensor_id: Uuid,
}

/// Re-derive `calibration_id`, `deployment_id`/`site_id`, and `calibrated_value` for every
/// reading owned by a sensor from its calibration and deployment windows, cascade to derived
/// parameters, and refresh aggregates. Tracked as a `reprocessing_jobs` row; returns the job
/// id immediately. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/actions/reprocess",
    request_body = ReprocessSensorRequest,
    responses(
        (status = 200, description = "Reprocessing triggered", body = QueuedJobResponse),
        (status = 403, description = "The sensor is deployed only outside the caller's projects"),
        (status = 404, description = "No such sensor"),
    ),
    tag = "actions"
)]
pub async fn reprocess_sensor(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<ReprocessSensorRequest>,
) -> AppResult<Json<QueuedJobResponse>> {
    let sensor_id = payload.sensor_id;
    // A sensor that has never been deployed belongs to no project, and neither do its readings, so
    // it stays reachable: an instrument sits in inventory before anyone decides where it goes.
    let target = project_of_sensor(&app_state.db, sensor_id).await?;
    confine_target(&scope, &target, Unowned::Allow, "sensor")?;

    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &app_state.db,
        "manual_reprocess",
        Some(sensor_id),
        None,
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(QueuedJobResponse::queued(job_id)))
}

/// One backdate is in flight at a time, so every request carries the same key.
const REPROCESS_ALL_DEDUPE_KEY: &str = "reprocess_all";

#[derive(Debug, Serialize, ToSchema)]
pub struct ReprocessAllResponse {
    pub job_id: Uuid,
    pub status: String,
    /// Number of (site, parameter) slots queued for re-derivation.
    pub slots: usize,
}

/// Re-derive `sensor_id`/`deployment_id`/`site_id`/`calibrated_value` for ALL historical readings
/// from the current deployment + calibration timelines, across every `(site, parameter)` slot that
/// has a deployment. Use after correcting deployment/calibration windows in bulk (the backdate of
/// historical attribution). Each slot is reprocessed via the decompression-safe
/// `reprocess_site_parameter_readings`; runs as one tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/reprocess_all",
    responses(
        (status = 200, description = "Backdate reprocessing triggered; returns job_id and slot count", body = ReprocessAllResponse),
        (status = 403, description = "The backdate names no target, so a caller confined to a project set is refused"),
    ),
    tag = "actions"
)]
pub async fn reprocess_all(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
) -> AppResult<Json<ReprocessAllResponse>> {
    // The backdate has no target field at all: it re-derives every slot in the installation.
    require_named_target(&scope, false, "sensor (POST /actions/reprocess)")?;

    let db = &app_state.db;
    // Count the slots only to report it back synchronously; the job re-reads `sensor_deployments`
    // itself, so a rerun reflects the current topology.
    let slot_count = deployments::Entity::find()
        .select_only()
        .column(deployments::Column::SiteId)
        .column(deployments::Column::ParameterId)
        .distinct()
        .into_tuple::<(Uuid, Uuid)>()
        .all(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .len();

    // One backdate at a time: a second request while one is queued joins it rather than starting a
    // concurrent pass over every slot. The claim releases the key, so a run already under way still
    // gets one follow-up.
    let queued = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "reprocess_all",
        None,
        None,
        &serde_json::json!({}),
        Some(REPROCESS_ALL_DEDUPE_KEY),
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let job_id = match queued {
        Some(id) => id,
        // The enqueue coalesced onto a run already queued under this key; that run is the answer.
        None => {
            crate::routes::private::reprocessing_jobs::models::job::Entity::find()
                .filter(
                    crate::routes::private::reprocessing_jobs::models::job::Column::DedupeKey
                        .eq(REPROCESS_ALL_DEDUPE_KEY),
                )
                .one(db)
                .await
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
                .ok_or_else(|| {
                    AppError::Internal("failed to enqueue reprocess_all job".to_string())
                })?
                .id
        }
    };

    Ok(Json(ReprocessAllResponse {
        job_id,
        status: "queued".to_string(),
        slots: slot_count,
    }))
}
