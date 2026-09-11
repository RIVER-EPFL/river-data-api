//! The deployment actions: roll a slot back to the sensor that held it, and the bulk
//! attribution backfill that stamps the readings an open deployment already covers.

use std::collections::HashMap;
use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use sea_orm::ColumnTrait;
use sea_orm::EntityTrait;
use sea_orm::FromQueryResult;
use sea_orm::Order;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Condition;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::Func;
use sea_orm::sea_query::JoinType;
use sea_orm::sea_query::PostgresQueryBuilder;
use sea_orm::sea_query::Query as SeaQuery;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::AccessScope;
use crate::common::middleware::DenyScoped;
use crate::common::middleware::ProjectScope;
use crate::common::scope::Unowned;
use crate::common::scope::confine_target;
use crate::common::scope::project_filter_sql;
use crate::common::scope::project_of_site;
use crate::common::scope::require_named_target;
use crate::common::scope::require_sites_in_scope;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::readings;
use crate::routes::private::sensor_calibrations::service::recompute_deployed_until;
use crate::routes::private::sensor_deployments as deployments;
use crate::routes::private::sensor_deployments::flows as slots;
use crate::routes::private::sites;

/// The sites the named deployments sit at, for confining a deployment-addressed action. An id that
/// resolves to no row contributes nothing; the action's own candidate selection reports it.
async fn deployment_sites(
    db: &sea_orm::DatabaseConnection,
    deployment_ids: &[Uuid],
) -> AppResult<Vec<Uuid>> {
    if deployment_ids.is_empty() {
        return Ok(Vec::new());
    }
    deployments::Entity::find()
        .select_only()
        .column(deployments::Column::SiteId)
        .distinct()
        .filter(deployments::Column::Id.is_in(deployment_ids.to_vec()))
        .into_tuple::<Uuid>()
        .all(db)
        .await
        .map_err(AppError::Database)
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RollbackDeploymentRequest {
    pub deployment_id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RollbackDeploymentResponse {
    pub status: String,
    pub readings_reassigned: u64,
    #[schema(required)]
    pub previous_deployment_id: Option<Uuid>,
    /// The reprocess this rollback queued: the re-derivation runs as a tracked job, like every
    /// other caller's, rather than holding the request open for the whole cascade.
    pub job_id: Uuid,
}

/// Undo the most recent sensor deployment, reassigning its readings back to the previous
/// deployment's site. Used after an accidentally-created deployment. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/rollback_deployment",
    request_body = RollbackDeploymentRequest,
    responses(
        (status = 200, description = "Rollback complete with reassignment count", body = RollbackDeploymentResponse),
        (status = 403, description = "The deployment is outside the caller's projects"),
        (status = 404, description = "Deployment not found"),
    ),
    tag = "actions"
)]
pub async fn rollback_deployment(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<RollbackDeploymentRequest>,
) -> AppResult<Json<RollbackDeploymentResponse>> {
    use sea_orm::TransactionTrait;

    let db = &app_state.db;

    // 1. Load the target deployment
    //
    // A deployment is always at a site, so its project is the site's. This deletes the row, the
    // same destruction `DELETE /sensor_deployments/{id}` performs under `inject_project_scope`.
    // `deployed_until` is the boundary the rolled-back deployment vacates; the previous one
    // re-extends to it (NULL = the target was open-ended, so the previous reopens open-ended too).
    let target = deployments::Entity::find_by_id(payload.deployment_id)
        .one(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Deployment not found".into()))?;
    let owner = project_of_site(db, target.site_id).await?;
    confine_target(&scope, &owner, Unowned::Deny, "deployment")?;

    let sensor_id = target.sensor_id;
    let parameter_id = target.parameter_id;
    let target_deployed_from = target.deployed_from;
    let target_deployed_until = target.deployed_until;

    // 2. Find the previous deployment for the same sensor AND THE SAME PARAMETER, on a multi-channel
    //    instrument the immediately-prior deployment by time could belong to a different channel;
    //    reopening that one would extend the wrong channel's window.
    let previous = deployments::Entity::find()
        .filter(deployments::Column::SensorId.eq(sensor_id))
        .filter(deployments::Column::ParameterId.eq(parameter_id))
        .filter(deployments::Column::DeployedFrom.lt(target_deployed_from))
        .filter(deployments::Column::Id.ne(payload.deployment_id))
        .order_by_desc(deployments::Column::DeployedFrom)
        .one(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let previous_deployment_id: Option<Uuid> = previous.as_ref().map(|p| p.id);

    // 3-4. Clear readings' FK to the rolled-back deployment, delete it, and reopen the previous
    //    deployment, atomically, so a mid-operation failure can't leave the deployment deleted with
    //    nothing reopened (which would silently un-attribute its readings). The decompression cap is
    //    lifted so the readings FK-clear can't fail on old compressed chunks.
    let txn = db
        .begin()
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    crate::common::bulk_write::lift_decompression_cap(&txn).await?;

    let readings_reassigned = readings::Entity::update_many()
        .col_expr(
            readings::Column::DeploymentId,
            Expr::value(Option::<Uuid>::None),
        )
        .filter(readings::Column::DeploymentId.eq(payload.deployment_id))
        .exec(&txn)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .rows_affected;

    deployments::Entity::delete_by_id(payload.deployment_id)
        .exec(&txn)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // Reopen the previous deployment to absorb the vacated window. `recompute_deployed_until` only ever
    //    SHORTENS (LEAST), so without this the previous deployment, auto-closed when the rolled-back
    //    one was created, stays closed and the readings would un-attribute (site_id NULL) instead of
    //    reverting to the previous site. Reopening to the target's own `deployed_until` reclaims exactly
    //    the window the target held (NULL = open-ended), which can't overlap anything the slot
    //    constraint already excluded.
    //    Another instrument may have moved into the window the predecessor is about to reclaim. The
    //    check runs after the DELETE above so the deployment being rolled back is not itself
    //    reported as the occupant; a conflict aborts the transaction and nothing is destroyed.
    if let Some(prev) = &previous {
        let prev_id = prev.id;
        let prev_site = prev.site_id;
        let prev_from = prev.deployed_from;

        let request = slots::SlotRequest {
            site_id: prev_site,
            parameter_id,
            deployed_from: prev_from.with_timezone(&chrono::Utc),
            deployed_until: target_deployed_until.map(|t| t.with_timezone(&chrono::Utc)),
            exclude_deployment: Some(prev_id),
            recalled_sensor: None,
        };
        if let Some(occupant) = slots::find_occupant(&txn, &request)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        {
            return Err(AppError::Conflict(slots::conflict_message(
                &occupant,
                sensor_id,
                "roll back",
            )));
        }

        deployments::Entity::update_many()
            .col_expr(
                deployments::Column::DeployedUntil,
                Expr::value(target_deployed_until),
            )
            .filter(deployments::Column::Id.eq(prev_id))
            .exec(&txn)
            .await
            .map_err(|e| {
                if slots::is_slot_conflict(&e) {
                    AppError::Conflict(format!(
                        "Rolling back would extend deployment {prev_id} into a period another \
                     instrument now holds at this site and parameter."
                    ))
                } else {
                    AppError::Internal(format!("DB error: {e}"))
                }
            })?;
    }
    txn.commit()
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // 5. Re-chain the remaining timeline and re-derive every reading for the sensor by window. The
    //    rolled-back deployment's readings now fall in the reopened previous deployment's window (or
    //    in a gap → no site, if there was no previous). Re-chaining only ever shortens, so it can't
    //    violate the slot-exclusion constraint. Reprocess also refreshes the continuous aggregates.
    recompute_deployed_until(db, sensor_id)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "manual_reprocess",
        Some(sensor_id),
        None,
        &serde_json::json!({ "sensor_id": sensor_id }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue the rollback reprocess".to_string()))?;

    tracing::info!(
        deployment_id = %payload.deployment_id,
        sensor_id = %sensor_id,
        readings_reassigned,
        previous = ?previous_deployment_id,
        "Rolled back deployment"
    );

    Ok(Json(RollbackDeploymentResponse {
        status: "rolled_back".to_string(),
        readings_reassigned,
        previous_deployment_id,
        job_id,
    }))
}

// ---------------------------------------------------------------------------
// Bulk historical attribution (backfill)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct BackfillCandidate {
    pub deployment_id: Uuid,
    pub sensor_id: Uuid,
    pub site_id: Uuid,
    pub parameter_id: Uuid,
    /// The deployment's current start.
    pub deployed_from: chrono::DateTime<chrono::Utc>,
    /// Earliest claimable unattributed reading, the date `deployed_from` would move back to.
    pub target_from: chrono::DateTime<chrono::Utc>,
    /// Number of unattributed readings (`sensor_id IS NULL`) in `[target_from, deployed_from)`.
    pub claimable_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillSiteSummary {
    pub site_id: Uuid,
    pub deployments: i64,
    pub claimable_count: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillCandidatesResponse {
    pub candidates: Vec<BackfillCandidate>,
    pub by_site: Vec<BackfillSiteSummary>,
    pub total_candidates: usize,
    pub total_claimable: i64,
}

/// Open deployments that have claimable pre-start history: readings at the same `(site, parameter)`
/// with `sensor_id IS NULL` before `deployed_from`, bounded below by any prior deployment's end so
/// backdating can't overlap it. `target_from` is the earliest such reading.
///
/// Confined to the caller's projects. The rows carry the site, sensor and deployment ids the write
/// actions in this file take, so an unconfined enumeration hands out exactly what a cross-project
/// write needs; the CRUD reads of the same rows confine by the same project set.
async fn fetch_backfill_candidates(
    db: &sea_orm::DatabaseConnection,
    scope: &AccessScope,
) -> AppResult<Vec<BackfillCandidate>> {
    use sea_orm::{ConnectionTrait, Statement};
    let d = Alias::new("d");
    let s_ = Alias::new("s");
    let pe = Alias::new("pe");
    let c = Alias::new("c");
    let r = Alias::new("r");
    let p = Alias::new("p");

    let prior_end = SeaQuery::select()
        .expr_as(
            Func::max(Expr::col((p.clone(), deployments::Column::DeployedUntil))),
            Alias::new("prior_end"),
        )
        .from_as(deployments::Entity, p.clone())
        .and_where(Expr::cust("p.site_id = d.site_id"))
        .and_where(Expr::cust("p.parameter_id = d.parameter_id"))
        .and_where(Expr::cust("p.id <> d.id"))
        .and_where(Expr::col((p.clone(), deployments::Column::DeployedUntil)).is_not_null())
        .and_where(Expr::cust("p.deployed_until <= d.deployed_from"))
        .take();

    // Claimable is "no deployment covers this reading", not "no instrument names it": every
    // non-derived row names an instrument from the moment it is written
    // (`readings_instrument_required`), and it is the deployment the backdate supplies. A derived
    // value carries the slot and no instrument by design, so it is never claimed.
    let claimable = SeaQuery::select()
        .expr_as(
            Func::min(Expr::col((r.clone(), readings::Column::Time))),
            Alias::new("target_from"),
        )
        .expr_as(Expr::cust("COUNT(*)"), Alias::new("claimable_count"))
        .from_as(readings::Entity, r.clone())
        .and_where(Expr::cust("r.site_id = d.site_id"))
        .and_where(Expr::cust("r.parameter_id = d.parameter_id"))
        .and_where(Expr::col((r.clone(), readings::Column::DeploymentId)).is_null())
        .and_where(Expr::cust("r.measurement_type IS DISTINCT FROM 'derived'"))
        .and_where(Expr::cust("r.time < d.deployed_from"))
        .and_where(Expr::cust(
            "(pe.prior_end IS NULL OR r.time >= pe.prior_end)",
        ))
        .take();

    let mut open = Condition::all()
        .add(Expr::col((d.clone(), deployments::Column::DeployedUntil)).is_null())
        .add(Expr::cust("c.claimable_count > 0"));
    let mut values: Vec<sea_orm::Value> = Vec::new();
    if let Some(predicate) = project_filter_sql(scope, "s.project_id", &mut values) {
        open = open.add(Expr::cust_with_values(predicate, values));
    }

    let on_true = || Condition::all().add(Expr::cust("true"));
    let (sql, values) = SeaQuery::select()
        .expr_as(
            Expr::col((d.clone(), deployments::Column::Id)),
            Alias::new("deployment_id"),
        )
        .columns([
            (d.clone(), deployments::Column::SensorId),
            (d.clone(), deployments::Column::SiteId),
            (d.clone(), deployments::Column::ParameterId),
            (d.clone(), deployments::Column::DeployedFrom),
        ])
        .columns([
            (c.clone(), Alias::new("target_from")),
            (c.clone(), Alias::new("claimable_count")),
        ])
        .from_as(deployments::Entity, d.clone())
        .join_as(
            JoinType::InnerJoin,
            sites::Entity,
            s_.clone(),
            Expr::col((s_.clone(), sites::Column::Id))
                .equals((d.clone(), deployments::Column::SiteId)),
        )
        // `ON TRUE` rather than `JoinType::CrossJoin`, which the builder still writes an `ON`
        // clause after; the two mean the same thing.
        .join_lateral(JoinType::InnerJoin, prior_end, pe.clone(), on_true())
        .join_lateral(JoinType::InnerJoin, claimable, c.clone(), on_true())
        .cond_where(open)
        .order_by_expr(Expr::cust("c.claimable_count"), Order::Desc)
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
        .map(|r| -> AppResult<BackfillCandidate> {
            let row = BackfillCandidateRow::from_query_result(r, "")?;
            Ok(BackfillCandidate {
                deployment_id: row.deployment_id,
                sensor_id: row.sensor_id,
                site_id: row.site_id,
                parameter_id: row.parameter_id,
                deployed_from: row.deployed_from.with_timezone(&chrono::Utc),
                target_from: row.target_from.with_timezone(&chrono::Utc),
                claimable_count: row.claimable_count,
            })
        })
        .collect()
}

/// List open deployments with claimable pre-deployment history, rolled up per site. Requires
/// `read_metadata`.
#[utoipa::path(
    get,
    path = "/api/actions/backfill_candidates",
    responses((status = 200, description = "Backfill candidates", body = BackfillCandidatesResponse)),
    tag = "actions"
)]
pub async fn backfill_candidates(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    _: DenyScoped,
) -> AppResult<Json<BackfillCandidatesResponse>> {
    let candidates = fetch_backfill_candidates(&app_state.db, &scope).await?;

    let mut by_site_map: HashMap<Uuid, (i64, i64)> = HashMap::new();
    let mut total_claimable = 0i64;
    for c in &candidates {
        let e = by_site_map.entry(c.site_id).or_insert((0, 0));
        e.0 += 1;
        e.1 += c.claimable_count;
        total_claimable += c.claimable_count;
    }
    let by_site = by_site_map
        .into_iter()
        .map(
            |(site_id, (deployments, claimable_count))| BackfillSiteSummary {
                site_id,
                deployments,
                claimable_count,
            },
        )
        .collect();

    Ok(Json(BackfillCandidatesResponse {
        total_candidates: candidates.len(),
        total_claimable,
        by_site,
        candidates,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct BackfillAttributionRequest {
    /// Backfill every candidate.
    #[serde(default)]
    pub all: bool,
    /// Restrict to candidates at this site.
    pub site_id: Option<Uuid>,
    /// Restrict to these specific deployments.
    #[serde(default)]
    pub deployment_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BackfillAttributionResponse {
    pub job_id: Uuid,
    pub status: String,
    pub deployments_updated: usize,
    pub estimated_readings: i64,
}

/// Backdate the selected open deployments to their earliest claimable reading (bounded by any prior
/// deployment), then window-reprocess the affected slots so the previously-unattributed readings are
/// stamped with `sensor_id`/`deployment_id`/`calibration_id`. Runs as one tracked job. Requires
/// `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/backfill_attribution",
    request_body = BackfillAttributionRequest,
    responses(
        (status = 200, description = "Backfill triggered", body = BackfillAttributionResponse),
        (status = 403, description = "A named site or deployment is outside the caller's projects, or nothing was named"),
    ),
    tag = "actions"
)]
pub async fn backfill_attribution(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BackfillAttributionRequest>,
) -> AppResult<Json<BackfillAttributionResponse>> {
    let db = &app_state.db;

    // `all` with no site and no deployments is the whole installation; a request that names nothing
    // at all selects nothing and keeps its 400 below.
    let named = payload.site_id.is_some() || !payload.deployment_ids.is_empty() || !payload.all;
    require_named_target(&scope, named, "site or deployments")?;
    if let Some(site_id) = payload.site_id {
        require_sites_in_scope(db, &scope, &[site_id]).await?;
    }
    if !payload.deployment_ids.is_empty() {
        let sites = deployment_sites(db, &payload.deployment_ids).await?;
        require_sites_in_scope(db, &scope, &sites).await?;
    }

    let all_candidates = fetch_backfill_candidates(db, &scope).await?;
    let dep_filter: HashSet<Uuid> = payload.deployment_ids.iter().copied().collect();
    let selected: Vec<BackfillCandidate> = all_candidates
        .into_iter()
        .filter(|c| {
            if !dep_filter.is_empty() {
                dep_filter.contains(&c.deployment_id)
            } else if let Some(site) = payload.site_id {
                c.site_id == site
            } else {
                payload.all
            }
        })
        .collect();

    if selected.is_empty() {
        return Err(AppError::BadRequest(
            "No matching backfill candidates (pass all=true, a site_id, or deployment_ids)"
                .to_string(),
        ));
    }

    // Backdate each selected deployment to its target_from (>= prior end, so no slot overlap), then
    // re-chain that sensor's deployed_until. Collect the distinct slots + sensors touched.
    let estimated_readings: i64 = selected.iter().map(|c| c.claimable_count).sum();
    let mut slots: HashSet<(Uuid, Uuid)> = HashSet::new();
    let mut sensors: HashSet<Uuid> = HashSet::new();
    for c in &selected {
        // Idempotent backdate: only move `deployed_from` earlier. After the first apply the row sits
        // at `target_from`, so a client retry replayed against another replica matches no rows
        // (`deployed_from > target_from` is false) and can't double-apply or re-widen the window.
        deployments::Entity::update_many()
            .col_expr(
                deployments::Column::DeployedFrom,
                Expr::value(c.target_from),
            )
            .filter(deployments::Column::Id.eq(c.deployment_id))
            .filter(deployments::Column::DeployedFrom.gt(c.target_from))
            .exec(db)
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
        slots.insert((c.site_id, c.parameter_id));
        sensors.insert(c.sensor_id);
    }
    for sensor_id in &sensors {
        recompute_deployed_until(db, *sensor_id)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
    }

    let deployments_updated = selected.len();
    let slots_param: Vec<[Uuid; 2]> = slots.into_iter().map(|(s, p)| [s, p]).collect();
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "backfill_attribution",
        None,
        None,
        &serde_json::json!({ "slots": slots_param }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .ok_or_else(|| AppError::Internal("failed to enqueue backfill_attribution job".to_string()))?;

    Ok(Json(BackfillAttributionResponse {
        job_id,
        status: "queued".to_string(),
        deployments_updated,
        estimated_readings,
    }))
}

/// The row shapes the raw operator-action queries return, so each mapping is checked against the
/// SELECT that fills it rather than against a column name written twice.
#[derive(FromQueryResult)]
struct BackfillCandidateRow {
    deployment_id: Uuid,
    sensor_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    deployed_from: chrono::DateTime<chrono::FixedOffset>,
    target_from: chrono::DateTime<chrono::FixedOffset>,
    claimable_count: i64,
}
