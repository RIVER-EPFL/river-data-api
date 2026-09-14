//! The calculation actions: recompute one definition, compute a site's timestamps, and preview
//! what a definition would produce before it is saved.

use axum::extract::Path;
use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use sea_orm::EntityTrait;
use sea_orm::FromQueryResult;
use sea_orm::Order;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::JoinType;
use sea_orm::sea_query::PostgresQueryBuilder;
use sea_orm::sea_query::Query as SeaQuery;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::scope::require_named_target;
use crate::common::scope::require_sites_in_scope;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::derived_parameters::models::StepDependents;
use crate::routes::private::readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::reprocessing_jobs::models::QueuedJobResponse;
use crate::routes::private::sensor_calibrations::service::bind_derived_variables;
use crate::routes::private::sensor_calibrations::service::constants_named_by;
use crate::routes::private::sensor_calibrations::service::evaluate_formula;
use crate::routes::private::sensor_calibrations::service::site_property_values;

#[derive(FromQueryResult)]
struct SeriesPoint {
    time: chrono::DateTime<chrono::FixedOffset>,
    val: f64,
}

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct SlotRow {
    sp_id: Uuid,
    parameter_id: Uuid,
    units: String,
}

/// What a computation request enqueues: the job, and how many instants it covers.
#[derive(Debug, Serialize, ToSchema)]
pub struct ComputeDerivedResponse {
    /// The row enqueued, or null where an identical job was already queued.
    #[schema(required)]
    pub job_id: Option<Uuid>,
    /// `queued`, always.
    pub status: String,
    pub total_timestamps: usize,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ComputeDerivedRequest {
    pub site_timestamps: Vec<SiteTimestamps>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SiteTimestamps {
    pub site_id: Uuid,
    pub timestamps: Vec<chrono::DateTime<chrono::Utc>>,
}

/// Compute and upsert derived parameter values for the given (site, timestamp) pairs,
/// tracked as a `reprocessing_jobs` row (`readings_updated` = computed count). Runs derived
/// formula evaluation against source readings, then refreshes aggregates. Returns the job id
/// immediately. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/compute_derived",
    request_body = ComputeDerivedRequest,
    responses(
        (status = 200, description = "Computation triggered", body = ComputeDerivedResponse),
        (status = 403, description = "A named site is outside the caller's projects, or no site was named"),
    ),
    tag = "actions"
)]
pub async fn compute_derived(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<ComputeDerivedRequest>,
) -> AppResult<Json<ComputeDerivedResponse>> {
    let sites: Vec<Uuid> = payload
        .site_timestamps
        .iter()
        .map(|st| st.site_id)
        .collect();
    require_named_target(&scope, !sites.is_empty(), "site")?;
    require_sites_in_scope(&app_state.db, &scope, &sites).await?;

    let total_timestamps: usize = payload
        .site_timestamps
        .iter()
        .map(|st| st.timestamps.len())
        .sum();

    let site_timestamps: Vec<serde_json::Value> = payload
        .site_timestamps
        .iter()
        .map(|st| {
            serde_json::json!({
                "site_id": st.site_id,
                "timestamps": st.timestamps,
            })
        })
        .collect();

    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &app_state.db,
        "compute_derived",
        None,
        None,
        &serde_json::json!({ "site_timestamps": site_timestamps }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(ComputeDerivedResponse {
        job_id,
        status: "queued".to_string(),
        total_timestamps,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PreviewDerivedRequest {
    pub formula: String,
    pub site_id: Uuid,
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewDerivedResponse {
    pub site: PreviewSite,
    pub times: Vec<chrono::DateTime<chrono::Utc>>,
    pub source_parameters: Vec<SourceParameterSeries>,
    pub derived: DerivedSeries,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PreviewSite {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SourceParameterSeries {
    pub name: String,
    pub units: String,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DerivedSeries {
    pub name: String,
    pub formula: String,
    pub values: Vec<Option<f64>>,
    pub errors: Vec<Option<String>>,
}

/// Preview a derived parameter formula against historical source readings at a given site,
/// WITHOUT writing anything to the database. Used by the formula builder UI to validate
/// formulas before saving. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/actions/preview_derived",
    request_body = PreviewDerivedRequest,
    responses(
        (status = 200, description = "Computed values with per-timestamp errors", body = PreviewDerivedResponse),
        (status = 400, description = "Invalid formula syntax or unknown variables"),
        (status = 404, description = "Site not found"),
    ),
    tag = "actions"
)]
pub async fn preview_derived(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<PreviewDerivedRequest>,
) -> AppResult<Json<PreviewDerivedResponse>> {
    use sea_orm::{ConnectionTrait, Statement};

    // A restricted caller may only preview a derived computation against a site in its projects.
    require_sites_in_scope(&app_state.db, &scope, &[payload.site_id]).await?;

    // Validate formula
    if payload.formula.len() > 1000 {
        return Err(AppError::BadRequest(
            "Formula too long (max 1000 characters)".to_string(),
        ));
    }
    payload
        .formula
        .parse::<meval::Expr>()
        .map_err(|e| AppError::BadRequest(format!("Invalid formula: {e}")))?;

    let db = &app_state.db;

    // Get site name
    let site_name = crate::routes::private::sites::Entity::find_by_id(payload.site_id)
        .one(db)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?
        .ok_or_else(|| AppError::NotFound("Site not found".into()))?
        .name;

    // Extract variable names from formula
    let var_names = crate::routes::private::tools::service::free_identifiers(&payload.formula);

    if var_names.is_empty() {
        return Ok(Json(PreviewDerivedResponse {
            site: PreviewSite {
                id: payload.site_id,
                name: site_name,
            },
            times: vec![],
            source_parameters: vec![],
            derived: DerivedSeries {
                name: "preview".to_string(),
                formula: payload.formula,
                values: vec![],
                errors: vec![],
            },
        }));
    }

    // Resolve variable names → site_parameters at this site
    let mut param_info: Vec<(String, Uuid, Uuid, String)> = Vec::new();

    for var_name in &var_names {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                r"SELECT sp.id as sp_id, sp.parameter_id, COALESCE(sp.display_units, '') as units
                  FROM site_parameters sp
                  JOIN parameters pt ON pt.id = sp.parameter_id
                  WHERE sp.site_id = $1 AND pt.code = $2
                  LIMIT 1",
                [payload.site_id.into(), var_name.clone().into()],
            ))
            .await
            .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

        if let Some(row) = row {
            let SlotRow {
                sp_id,
                parameter_id,
                units,
            } = SlotRow::from_query_result(&row, "")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            param_info.push((var_name.clone(), sp_id, parameter_id, units));
        }
    }

    // A name that is not a slot at this site is a constant or a column of the site's row, bound
    // the way the continuous path binds them.
    let declared: Vec<String> = param_info.iter().map(|(name, ..)| name.clone()).collect();
    let constants = constants_named_by(db, &payload.formula, &declared)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
    let properties: Vec<(String, String)> = var_names
        .iter()
        .filter(|name| !declared.contains(name) && !constants.contains_key(name.as_str()))
        .map(|name| (name.clone(), name.clone()))
        .collect();
    let site_properties = site_property_values(db, payload.site_id, &properties)
        .await
        .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;

    // Fetch readings for all resolved parameters within time range
    let mut all_times: Vec<chrono::DateTime<chrono::Utc>> = Vec::new();
    let mut source_data: HashMap<String, HashMap<i64, f64>> = HashMap::new();
    let mut source_units: HashMap<String, String> = HashMap::new();

    for (var_name, _sp_id, parameter_id, units) in &param_info {
        source_units.insert(var_name.clone(), units.clone());

        let r = Alias::new("r");
        let smp = Alias::new("smp");
        let (sql, values) = SeaQuery::select()
            .distinct_on([(r.clone(), readings::Column::Time)])
            .column((r.clone(), readings::Column::Time))
            .expr_as(crate::common::served::spot_value(), Alias::new("val"))
            .from_as(readings::Entity, r.clone())
            .join_as(
                JoinType::LeftJoin,
                samples::Entity,
                smp.clone(),
                Expr::col((smp.clone(), samples::Column::Id))
                    .equals((r.clone(), readings::Column::SampleId)),
            )
            .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).eq(*parameter_id))
            .and_where(Expr::col((r.clone(), readings::Column::SiteId)).eq(payload.site_id))
            .and_where(Expr::col((r.clone(), readings::Column::Time)).gte(payload.start))
            .and_where(Expr::col((r.clone(), readings::Column::Time)).lte(payload.end))
            .order_by((r.clone(), readings::Column::Time), Order::Asc)
            .order_by_expr(
                Expr::cust("(r.measurement_type IS NOT DISTINCT FROM 'spot')"),
                Order::Asc,
            )
            .order_by((r.clone(), readings::Column::StreamId), Order::Asc)
            .order_by_expr(Expr::cust("(r.is_flagged IS TRUE)"), Order::Asc)
            .order_by((r.clone(), readings::Column::ReplicateIndex), Order::Asc)
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

        let map = source_data.entry(var_name.clone()).or_default();
        for row in rows {
            let SeriesPoint { time, val } = SeriesPoint::from_query_result(&row, "")
                .map_err(|e| AppError::Internal(format!("DB error: {e}")))?;
            let utc = time.with_timezone(&chrono::Utc);
            map.insert(utc.timestamp_millis(), val);
            all_times.push(utc);
        }
    }

    // Deduplicate and sort times
    let mut time_set: Vec<i64> = all_times
        .iter()
        .map(chrono::DateTime::timestamp_millis)
        .collect();
    time_set.sort_unstable();
    time_set.dedup();

    let times: Vec<chrono::DateTime<chrono::Utc>> = time_set
        .iter()
        .map(|ms| chrono::DateTime::from_timestamp_millis(*ms).unwrap_or_default())
        .collect();

    // Build source parameter series
    let source_parameters: Vec<SourceParameterSeries> = var_names
        .iter()
        .filter(|vn| param_info.iter().any(|(n, _, _, _)| n == *vn))
        .map(|var_name| {
            let data = source_data.get(var_name);
            let units = source_units.get(var_name).cloned().unwrap_or_default();
            let values: Vec<Option<f64>> = time_set
                .iter()
                .map(|ms| data.and_then(|d| d.get(ms).copied()))
                .collect();
            SourceParameterSeries {
                name: var_name.clone(),
                units,
                values,
            }
        })
        .collect();

    // Evaluate formula at each timestamp
    let mut derived_values: Vec<Option<f64>> = Vec::with_capacity(times.len());
    let mut derived_errors: Vec<Option<String>> = Vec::with_capacity(times.len());

    for ms in &time_set {
        let parameters: Vec<(String, Option<f64>)> = declared
            .iter()
            .map(|name| {
                let value = source_data.get(name).and_then(|data| data.get(ms)).copied();
                (name.clone(), value)
            })
            .collect();
        let Ok(vars) =
            bind_derived_variables(&payload.formula, &parameters, &site_properties, &constants)
        else {
            derived_values.push(None);
            derived_errors.push(None);
            continue;
        };

        match evaluate_formula(&payload.formula, &vars) {
            Ok(val) if val.is_finite() => {
                derived_values.push(Some(val));
                derived_errors.push(None);
            }
            Ok(val) => {
                derived_values.push(None);
                derived_errors.push(Some(format!("Non-finite result: {val}")));
            }
            Err(e) => {
                derived_values.push(None);
                derived_errors.push(Some(e));
            }
        }
    }

    Ok(Json(PreviewDerivedResponse {
        site: PreviewSite {
            id: payload.site_id,
            name: site_name,
        },
        times,
        source_parameters,
        derived: DerivedSeries {
            name: "preview".to_string(),
            formula: payload.formula,
            values: derived_values,
            errors: derived_errors,
        },
    }))
}

// --- What a step feeds ---

/// Every calculation that reads this step, and the formulas inside each that name it (M208). A
/// step mints no catalog parameter, so the parameter graph cannot answer this. Requires
/// `read_data`.
#[utoipa::path(
    get,
    path = "/api/derived_parameters/{id}/dependents",
    params(("id" = Uuid, Path, description = "Formula UUID")),
    responses(
        (status = 200, description = "The calculations and formulas that read the step", body = StepDependents),
        (status = 404, description = "No formula carries that id"),
    ),
    tag = "derived_parameters"
)]
pub async fn step_dependents(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<StepDependents>> {
    let dependents =
        crate::routes::private::derived_parameters::service::dependents_of_step(&state.db, id)
            .await
            .map_err(AppError::from)?;
    Ok(Json(dependents))
}

// --- Recompute one definition ---

/// Recompute every derived value for a given derived parameter definition. Backfills via
/// joining source readings; tracked as a `reprocessing_jobs` row. Refreshes continuous
/// aggregates on completion. Requires `write_metadata`.
#[utoipa::path(
    post,
    path = "/api/actions/derived_parameters/{id}/recompute",
    params(("id" = Uuid, Path, description = "Derived parameter definition UUID")),
    responses(
        (status = 200, description = "Background recompute job triggered", body = QueuedJobResponse),
        (status = 404, description = "Derived parameter definition not found"),
    ),
    tag = "actions"
)]
pub async fn recompute_derived(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<QueuedJobResponse>> {
    let job_id = spawn_recompute_derived(&state.db, state.events.clone(), id).await?;
    Ok(Json(QueuedJobResponse::queued(Some(job_id))))
}

/// Enqueue a durable `derived_recompute` job for one derived parameter definition. Runs on the
/// claim-based worker pool
/// (`DerivedRecompute`), reading `derived_definition_id` back from the job's params.
pub async fn spawn_recompute_derived(
    db: &sea_orm::DatabaseConnection,
    _events: crate::common::EventSender,
    id: Uuid,
) -> Result<Uuid, sea_orm::DbErr> {
    crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "derived_recompute",
        None,
        Some(id),
        &serde_json::json!({ "derived_definition_id": id }),
        None,
    )
    .await?
    .ok_or_else(|| sea_orm::DbErr::Custom("enqueue returned no id".into()))
}
