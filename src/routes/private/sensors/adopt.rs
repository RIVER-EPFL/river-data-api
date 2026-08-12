use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, Set,
    Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, sensor_in_scope};
use crate::error::{AppError, AppResult};
use crate::routes::private::sensors::calibrations::service::recompute_deployed_until;
use crate::routes::private::{parameters, sites::parameters as site_parameters};

fn default_true() -> bool {
    true
}

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
        is_derived: Set(Some(false)),
        derived_definition_id: Set(None),
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

// ---------------------------------------------------------------------------
// Adopt
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct AdoptRequest {
    pub site_id: Uuid,
    /// The parameter this deployment binds the sensor to. Optional: derived from the sensor's
    /// existing single parameter when omitted; required when the sensor covers several.
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Half-open window start. Defaults to now().
    #[serde(default)]
    pub deployed_from: Option<DateTime<Utc>>,
    /// Optional window end (recall). NULL = open-ended.
    #[serde(default)]
    pub deployed_until: Option<DateTime<Utc>>,
    /// Auto-create the (site, parameter) site_parameter if missing. Default true.
    #[serde(default = "default_true")]
    pub create_site_parameter: bool,
    #[serde(default)]
    pub deployment_type: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AdoptResponse {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub site_parameter_id: Uuid,
    pub site_parameter_created: bool,
    pub deployed_from: DateTime<Utc>,
    pub deployed_until: Option<DateTime<Utc>>,
    pub job_id: Uuid,
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
    let sensor_exists = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM sensors WHERE id = $1",
            [sensor_id.into()],
        ))
        .await?;
    if sensor_exists.is_none() {
        return Err(AppError::NotFound("Sensor not found".to_string()));
    }
    let rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT DISTINCT parameter_id FROM (
                  SELECT parameter_id FROM sensor_deployments WHERE sensor_id = $1
                  UNION ALL
                  SELECT parameter_id FROM sensor_calibrations WHERE sensor_id = $1
              ) x WHERE parameter_id IS NOT NULL",
            [sensor_id.into()],
        ))
        .await?;
    let params: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.try_get("", "parameter_id").ok())
        .collect();
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
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT parameter_id FROM sensor_deployments
              WHERE sensor_id = $1 AND site_id = $2
              ORDER BY (deployed_until IS NULL) DESC, deployed_from DESC
              LIMIT 1",
            [outgoing_sensor_id.into(), site_id.into()],
        ))
        .await?;
    row.and_then(|r| r.try_get("", "parameter_id").ok())
        .ok_or_else(|| {
            AppError::BadRequest(
                "Outgoing sensor is not deployed at this site; pass parameter_id".to_string(),
            )
        })
}

/// Adopt (deploy) a sensor to a site slot for a window. Auto-creates the site_parameter if missing,
/// then re-derives the sensor's readings by window (tracked job). Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/sensors/{sensor_id}/adopt",
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
    Path(sensor_id): Path<Uuid>,
    Json(payload): Json<AdoptRequest>,
) -> AppResult<Json<AdoptResponse>> {
    let db = &app_state.db;
    let parameter_id = resolve_sensor_parameter(db, sensor_id, payload.parameter_id).await?;

    let site_exists = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT 1 FROM sites WHERE id = $1",
            [payload.site_id.into()],
        ))
        .await?;
    if site_exists.is_none() {
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
    // Lift the decompression cap for the readings parameter_id backfill below (no-op on uncompressed
    // data; resets on commit). Applies to the whole transaction.
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;
    let (site_parameter_id, site_parameter_created) =
        resolve_or_create_site_parameter(&txn, payload.site_id, parameter_id, payload.create_site_parameter)
            .await?;

    // Auto-recall this sensor's currently-open deployment FOR THIS PARAMETER at the new start (twin of
    // the sensor_deployments before_create hook). Scoped to the parameter so adopting one channel of a
    // multi-channel instrument doesn't recall its other channels.
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE sensor_deployments SET deployed_until = $1
          WHERE sensor_id = $2 AND parameter_id = $3 AND deployed_until IS NULL",
        [deployed_from.into(), sensor_id.into(), parameter_id.into()],
    ))
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
    let until_val: sea_orm::Value = payload.deployed_until.into();
    let insert = txn
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO sensor_deployments
                  (id, sensor_id, site_id, parameter_id, deployed_from, deployed_until, deployment_type, notes)
              VALUES ($1, $2, $3, $8, $4, $5, $6, $7)",
            [
                dep_id.into(),
                sensor_id.into(),
                payload.site_id.into(),
                deployed_from.into(),
                until_val,
                dep_type.into(),
                notes.into(),
                parameter_id.into(),
            ],
        ))
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
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE readings SET parameter_id = $1 WHERE sensor_id = $2 AND parameter_id IS NULL",
        [parameter_id.into(), sensor_id.into()],
    ))
    .await?;
    txn.commit().await?;

    // Slot-scoped reprocess re-attributes the (site, parameter) by deployment window, so a backdated
    // deployed_from stamps the sensor onto previously unattributed (sensor_id NULL) history. The
    // per-sensor pass then reconciles the sensor's own rows at any vacated slot.
    let adopt_site_id = payload.site_id;
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
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

#[derive(Debug, Serialize, ToSchema)]
pub struct AdoptSuggestion {
    pub now: DateTime<Utc>,
    pub end_of_last_deployment: Option<DateTime<Utc>>,
    pub first_reading: Option<DateTime<Utc>>,
}

/// Suggested deploy dates for a sensor: now, the end of its last deployment, and its first reading.
#[utoipa::path(
    get,
    path = "/sensors/{sensor_id}/adopt_suggestions",
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
    let row = db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT
                (SELECT MAX(COALESCE(deployed_until, deployed_from)) FROM sensor_deployments WHERE sensor_id = $1) AS end_last,
                (SELECT MIN(time) FROM readings WHERE sensor_id = $1) AS first_reading",
            [sensor_id.into()],
        ))
        .await?;
    let (end_of_last_deployment, first_reading) = match row {
        Some(r) => (
            r.try_get::<DateTime<chrono::FixedOffset>>("", "end_last")
                .ok()
                .map(|t| t.with_timezone(&Utc)),
            r.try_get::<DateTime<chrono::FixedOffset>>("", "first_reading")
                .ok()
                .map(|t| t.with_timezone(&Utc)),
        ),
        None => (None, None),
    };
    Ok(Json(AdoptSuggestion {
        now: Utc::now(),
        end_of_last_deployment,
        first_reading,
    }))
}

// ---------------------------------------------------------------------------
// Swap (end A, start B)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct SwapRequest {
    pub outgoing_sensor_id: Uuid,
    pub incoming_sensor_id: Uuid,
    pub site_id: Uuid,
    /// The (site, parameter) slot to swap. Optional: derived from the outgoing sensor's deployment at
    /// the site when omitted.
    #[serde(default)]
    pub parameter_id: Option<Uuid>,
    /// Instant of the swap. Defaults to now(). A ends at T, B starts at T (half-open => no overlap).
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    #[serde(default = "default_true")]
    pub create_site_parameter: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SwapResponse {
    pub ended_deployment_id: Option<Uuid>,
    pub started_deployment_id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    pub at: DateTime<Utc>,
    pub outgoing_job_id: Option<Uuid>,
    pub incoming_job_id: Uuid,
}

/// Swap one sensor for another in a (site, parameter) slot: end the outgoing sensor's deployment and
/// start the incoming sensor's at the same instant, in one transaction. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/actions/swap",
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
    Json(payload): Json<SwapRequest>,
) -> AppResult<Json<SwapResponse>> {
    let db = &app_state.db;
    let parameter_id = resolve_swap_parameter(
        db,
        payload.outgoing_sensor_id,
        payload.site_id,
        payload.parameter_id,
    )
    .await?;
    let at = payload.at.unwrap_or_else(Utc::now);

    let txn = db.begin().await?;
    // Lift the decompression cap for the readings parameter_id backfill below (resets on commit).
    txn.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0".to_owned(),
    ))
    .await?;
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
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        r"UPDATE sensor_deployments SET deployed_until = $1
          WHERE sensor_id = $2 AND parameter_id = $3 AND deployed_until IS NULL",
        [at.into(), payload.incoming_sensor_id.into(), parameter_id.into()],
    ))
    .await?;

    // End the outgoing sensor's open deployment at THIS (site, parameter) slot only, a multi-channel
    // outgoing instrument keeps its other channels running.
    let ended = txn
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE sensor_deployments SET deployed_until = $1
              WHERE sensor_id = $2 AND site_id = $3 AND parameter_id = $4 AND deployed_until IS NULL
              RETURNING id",
            [
                at.into(),
                payload.outgoing_sensor_id.into(),
                payload.site_id.into(),
                parameter_id.into(),
            ],
        ))
        .await?;
    let ended_deployment_id: Option<Uuid> = ended.and_then(|r| r.try_get("", "id").ok());

    // Start the incoming sensor at the same instant; half-open windows mean no overlap.
    let started_id = Uuid::new_v4();
    let insert = txn
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO sensor_deployments
                  (id, sensor_id, site_id, parameter_id, deployed_from, deployment_type, notes)
              VALUES ($1, $2, $3, $5, $4, 'permanent', 'Swapped in via /actions/swap')",
            [
                started_id.into(),
                payload.incoming_sensor_id.into(),
                payload.site_id.into(),
                at.into(),
                parameter_id.into(),
            ],
        ))
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
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE data_streams SET sensor_id = $1, updated_at = now() WHERE site_parameter_id = $2",
        [payload.incoming_sensor_id.into(), site_parameter_id.into()],
    ))
    .await?;
    txn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "UPDATE readings SET parameter_id = $1 WHERE sensor_id = $2 AND parameter_id IS NULL",
        [parameter_id.into(), payload.incoming_sensor_id.into()],
    ))
    .await?;
    txn.commit().await?;

    // Per-(site,parameter) handover reprocess: re-owns existing readings to whichever sensor's
    // deployment window covers each time, so the outgoing sensor's post-swap readings re-attribute
    // to the incoming sensor (a per-sensor reprocess can't, since those rows still carry sensor A).
    let site_id = payload.site_id;
    let job_id = crate::routes::private::reprocessing_jobs::worker::enqueue(
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
