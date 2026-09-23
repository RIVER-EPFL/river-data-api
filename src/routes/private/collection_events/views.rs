//! The visits table: the portal's wide `data` view reborn. One row per collection event at a
//! site, and per event the grid of parameter cells a field date filled in, plus the handlers that
//! stage a visit and drive its recompute and audit.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::{
    Alias, Condition, Expr, JoinType, PostgresQueryBuilder, Query as SeaQuery,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, Order, PaginatorTrait,
    QueryFilter, Statement,
};
use uuid::Uuid;

use super::models::{
    CellFinding, CellReplicate, CellSample, EnqueuedJobResponse, Entity, EventAuditRequest,
    EventCell, EventDetailResponse, EventRecomputeRequest, ExpectedParameter, PreviewEventRequest,
    PreviewUnstagedRequest, StageEventRequest, StageEventsRequest, StageVisitRow, StagedEvent,
    VisitCell, VisitCellCurve, VisitListQuery, VisitListRow, VisitReplicate, VisitRow, VisitsQuery,
    VisitsResponse,
};
use super::service::{
    self, limit_clause, paging, range_clause, visit_count_columns, visit_list_order,
};
use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::common::paging::{Page, Window};
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::models as events;
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::sensors::models::{self as sensors, InstrumentKind};
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::standard_curves::models as standard_curves;
use crate::routes::private::sync::models::HoldStatus;
use crate::routes::private::tools::models::EventPreview;
use crate::routes::resolve_site;

/// Recompute a collection event's tool outputs on demand: the chain executor runs every active
/// tool whose inputs resolve at this event, in dependency order, and saves the outputs through
/// the grab write path with fresh server-built provenance. A visit the sync created is refused
/// (Q41). Tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/collection_events/{id}/recompute",
    params(("id" = Uuid, Path, description = "Collection event id")),
    responses(
        (status = 200, description = "The tracked recompute job", body = EnqueuedJobResponse),
        (status = 400, description = "The visit was created by the portal sync"),
        (status = 404, description = "Unknown collection event"),
    ),
    tag = "collection_events"
)]
pub async fn recompute_collection_event(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<EnqueuedJobResponse>> {
    let event = Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
    enforce_project_scope_for_sites(&state.db, &scope, &[event.site_id]).await?;
    if !service::chain_may_run(&event.source) {
        return Err(AppError::BadRequest(
            "This visit was created by the portal sync, so calculations do not run at it. \
             Correct the value in the portal, which recomputes its own outputs."
                .to_string(),
        ));
    }
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "event_recompute",
        None,
        Some(event.site_id),
        &serde_json::json!({
            "collection_event_id": id,
            "actor": crate::common::actor::label(&auth),
        }),
        None,
    )
    .await?;
    Ok(Json(EnqueuedJobResponse { job_id }))
}

/// Whether the field day this caller opens lands pending a manager's ruling (Q177): an intern
/// may open one, and it is not a visit until somebody senior says it was. The same rule as an
/// intern's measurement, read from the same place.
fn visit_lands_pending(auth: &crate::common::middleware::AuthContext) -> bool {
    crate::routes::private::readings::service::entry_state(auth.highest_role().as_ref()).is_some()
}

/// Find or create the visit, and file the manager's ruling when the field day lands pending: the
/// two go together or neither does, so a pending visit is never left out of the review queue.
async fn stage_and_queue<C: sea_orm::ConnectionTrait>(
    conn: &C,
    site_id: Uuid,
    collected_at: chrono::DateTime<chrono::Utc>,
    actor: &str,
    notes: Option<&str>,
    pending: bool,
) -> AppResult<StagedEvent> {
    let visit = service::stage_visit(conn, site_id, collected_at, actor, notes, pending).await?;
    if visit.created && visit.unverified {
        service::open_unverified_visit_hold(conn, visit.site_id, visit.collected_at, actor).await?;
    }
    Ok(visit)
}

/// Stage a field visit: the portal's New Entry, made idempotent. A visit already standing at
/// `(site_id, collected_at)` is returned as it is, so two tools entering the same visit land on
/// one row instead of racing the unique key. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/collection_events/stage",
    request_body = StageEventRequest,
    responses(
        (status = 200, description = "The staged visit", body = StagedEvent),
        (status = 404, description = "Unknown site"),
    ),
    tag = "collection_events"
)]
pub async fn stage_collection_event(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<StageEventRequest>,
) -> AppResult<Json<StagedEvent>> {
    if !service::missing_sites(&state.db, &[req.site_id])
        .await?
        .is_empty()
    {
        return Err(AppError::NotFound(format!(
            "Site {} not found",
            req.site_id
        )));
    }
    enforce_project_scope_for_sites(&state.db, &scope, &[req.site_id]).await?;
    let actor = crate::common::actor::label(&auth);
    let pending = visit_lands_pending(&auth);
    let txn = sea_orm::TransactionTrait::begin(&state.db).await?;
    let staged = stage_and_queue(
        &txn,
        req.site_id,
        req.collected_at,
        &actor,
        req.notes.as_deref(),
        pending,
    )
    .await?;
    txn.commit().await?;
    Ok(Json(staged))
}

/// What the calculation chain would produce at a visit, given the cells the operator has typed
/// and not saved. The same walk the recompute runs, against the same inputs, storing nothing: no
/// run, no reading, no decision, no finding, no output slot and no job, so no value it returns can
/// be cited as provenance (Q212). Save is what executes and stores. Any member down to intern (Q240).
#[utoipa::path(
    post,
    path = "/api/collection_events/{id}/preview",
    params(("id" = Uuid, Path, description = "Collection event id")),
    request_body = PreviewEventRequest,
    responses(
        (status = 200, description = "What the chain would produce", body = EventPreview),
        (status = 400, description = "A staged cell names a parameter the site does not carry"),
        (status = 404, description = "Unknown collection event"),
    ),
    tag = "collection_events"
)]
pub async fn preview_collection_event(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
    Json(req): Json<PreviewEventRequest>,
) -> AppResult<Json<EventPreview>> {
    let event = Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
    enforce_project_scope_for_sites(&state.db, &scope, &[event.site_id]).await?;
    Ok(Json(
        crate::routes::private::tools::flows::preview_event(&state, id, &req.staged).await?,
    ))
}

/// [`preview_collection_event`] at a site and instant no visit stands at yet: a row typed into the
/// grid's spare area, previewed before Save opens its visit. Opens nothing. Any member down to
/// intern.
#[utoipa::path(
    post,
    path = "/api/collection_events/preview",
    request_body = PreviewUnstagedRequest,
    responses(
        (status = 200, description = "What the chain would produce", body = EventPreview),
        (status = 400, description = "A staged cell names a parameter the site does not carry"),
    ),
    tag = "collection_events"
)]
pub async fn preview_unstaged_visit(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<PreviewUnstagedRequest>,
) -> AppResult<Json<EventPreview>> {
    enforce_project_scope_for_sites(&state.db, &scope, &[req.site_id]).await?;
    Ok(Json(
        crate::routes::private::tools::flows::preview_unstaged(
            &state,
            req.site_id,
            req.collected_at,
            &req.staged,
        )
        .await?,
    ))
}

/// The rows of a field day that repeat an earlier row's site and instant, each as
/// `(first, repeat)` by position.
pub(super) fn repeated_visits(rows: &[StageVisitRow]) -> Vec<(usize, usize)> {
    let mut repeats = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(first) = rows[..i]
            .iter()
            .position(|r| r.site_id == row.site_id && r.collected_at == row.collected_at)
        {
            repeats.push((first, i));
        }
    }
    repeats
}

/// Stage a field day: one visit per row, each at its own site and instant, in one transaction.
/// A row repeating another's site and instant refuses the day, as does an unknown site, so no
/// partial day lands. Each visit is find-or-create exactly as `/collection_events/stage`.
/// Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/collection_events/stage_many",
    request_body = StageEventsRequest,
    responses(
        (status = 200, description = "The staged visits, one per row in the order given", body = Vec<StagedEvent>),
        (status = 400, description = "No visit given, or a row repeats another's site and instant"),
        (status = 404, description = "Unknown site"),
    ),
    tag = "collection_events"
)]
pub async fn stage_collection_events(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<StageEventsRequest>,
) -> AppResult<Json<Vec<StagedEvent>>> {
    use sea_orm::TransactionTrait;

    if req.visits.is_empty() {
        return Err(AppError::BadRequest(
            "A field day names at least one visit".to_string(),
        ));
    }
    let repeats = repeated_visits(&req.visits);
    if !repeats.is_empty() {
        let named: Vec<String> = repeats
            .iter()
            .map(|(first, repeat)| format!("row {} repeats row {}", repeat + 1, first + 1))
            .collect();
        return Err(AppError::BadRequest(format!(
            "A site is visited once at an instant: {}",
            named.join(", ")
        )));
    }
    let mut site_ids: Vec<Uuid> = Vec::with_capacity(req.visits.len());
    for row in &req.visits {
        if !site_ids.contains(&row.site_id) {
            site_ids.push(row.site_id);
        }
    }
    let missing = service::missing_sites(&state.db, &site_ids).await?;
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(Uuid::to_string).collect();
        return Err(AppError::NotFound(format!(
            "Site {} not found",
            names.join(", ")
        )));
    }
    enforce_project_scope_for_sites(&state.db, &scope, &site_ids).await?;
    let actor = crate::common::actor::label(&auth);
    let pending = visit_lands_pending(&auth);
    let txn = state.db.begin().await?;
    let mut staged = Vec::with_capacity(req.visits.len());
    for row in &req.visits {
        staged.push(
            stage_and_queue(
                &txn,
                row.site_id,
                row.collected_at,
                &actor,
                req.notes.as_deref(),
                pending,
            )
            .await?,
        );
    }
    txn.commit().await?;
    Ok(Json(staged))
}

/// Hold a restricted caller to a site they were granted. A job over no site runs across every
/// project, which only unrestricted access may ask for.
async fn confine_to_scope(
    db: &sea_orm::DatabaseConnection,
    scope: &crate::common::authz::AccessScope,
    site_id: Option<Uuid>,
    what: &str,
) -> AppResult<()> {
    if !scope.is_restricted() {
        return Ok(());
    }
    let Some(site_id) = site_id else {
        return Err(AppError::Forbidden(format!(
            "Name a site in your projects: a {what} over every project is outside your access"
        )));
    };
    enforce_project_scope_for_sites(db, scope, &[site_id]).await
}

/// The scoped apply (ADR 0007): run the chain over every manual visit in a site and/or time
/// range, or over the visits with open event findings, in one tracked job. This is the repair
/// path for what the reactive hook does not see: a constant, a curve or a script activation. An
/// unbounded scope (no site, no range, not held to findings) is refused. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/event_recompute",
    request_body = EventRecomputeRequest,
    responses(
        (status = 200, description = "The tracked recompute job", body = EnqueuedJobResponse),
        (status = 400, description = "Unbounded scope, or end before start"),
        (status = 404, description = "Unknown site"),
    ),
    tag = "collection_events"
)]
pub async fn run_event_recompute(
    State(state): State<AppState>,
    ProjectScope(access): ProjectScope,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<EventRecomputeRequest>,
) -> AppResult<Json<EnqueuedJobResponse>> {
    let scope = crate::routes::private::tools::models::RecomputeScope {
        site_id: req.site_id,
        start: req.start,
        end: req.end,
        only_findings: req.only_findings,
        calculation: req.calculation.clone(),
        version: req.version,
        constant: req.constant.clone(),
    };
    if !scope.is_bounded() {
        return Err(AppError::BadRequest(
            "A recompute needs a scope: a site, a time range, a script version, a constant, or \
             only_findings"
                .to_string(),
        ));
    }
    if let (Some(start), Some(end)) = (req.start, req.end)
        && end < start
    {
        return Err(AppError::BadRequest("end must be >= start".to_string()));
    }
    if let Some(site_id) = req.site_id
        && crate::routes::private::sites::Entity::find_by_id(site_id)
            .one(&state.db)
            .await?
            .is_none()
    {
        return Err(AppError::NotFound(format!("Site {site_id} not found")));
    }
    confine_to_scope(&state.db, &access, req.site_id, "recompute").await?;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "event_recompute",
        None,
        req.site_id,
        &serde_json::json!({
            "site_id": req.site_id,
            "start": req.start,
            "end": req.end,
            "only_findings": req.only_findings,
            "calculation": req.calculation,
            "version": req.version,
            "constant": req.constant,
            "actor": crate::common::actor::label(&auth),
        }),
        None,
    )
    .await?;
    Ok(Json(EnqueuedJobResponse { job_id }))
}

/// Run the missing/stale audit (D6): per collection event and active tool, report outputs missing
/// where the declared inputs exist, and outputs that disagree with a recompute under their pinned
/// script version. Findings land in the review queue (`replicate_audit_holds`, event kinds); the
/// auditor never writes values. Tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/actions/event_audit",
    request_body = EventAuditRequest,
    responses((status = 200, description = "The tracked audit job", body = EnqueuedJobResponse)),
    tag = "collection_events"
)]
pub async fn run_event_audit(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<EventAuditRequest>,
) -> AppResult<Json<EnqueuedJobResponse>> {
    let mut site_id = req.site_id;
    if let Some(id) = req.collection_event_id {
        let event = Entity::find_by_id(id)
            .one(&state.db)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
        site_id = site_id.or(Some(event.site_id));
        enforce_project_scope_for_sites(&state.db, &scope, &[event.site_id]).await?;
    }
    confine_to_scope(&state.db, &scope, site_id, "audit").await?;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "event_audit",
        None,
        req.site_id,
        &serde_json::json!({
            "site_id": req.site_id,
            "collection_event_id": req.collection_event_id,
        }),
        None,
    )
    .await?;
    Ok(Json(EnqueuedJobResponse { job_id }))
}

use crate::common::csv::field as csv_field;

/// The grid as CSV: `collected_at,source,parameters_filled,<code>…`, one line per visit in the
/// listed order, the served value in each cell and an empty cell where the grid shows none.
fn visits_csv(expected: &[ExpectedParameter], visits: &[VisitRow]) -> String {
    let mut csv = String::from("collected_at,source,parameters_filled");
    for col in expected {
        csv.push(',');
        csv.push_str(&csv_field(&col.code));
    }
    csv.push('\n');
    for v in visits {
        csv.push_str(
            &v.collected_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        csv.push(',');
        csv.push_str(&csv_field(&v.source));
        csv.push(',');
        csv.push_str(&v.parameters_filled.to_string());
        for col in expected {
            csv.push(',');
            if let Some(value) = v
                .cells
                .iter()
                .find(|c| c.parameter_id == col.parameter_id)
                .and_then(|c| c.value)
            {
                csv.push_str(&value.to_string());
            }
        }
        csv.push('\n');
    }
    csv
}

/// `{site}_visits_{first}_{last}.csv`, the range being the query's bounds when given and the
/// listed visits' extent otherwise.
fn visits_filename(site_name: &str, q: &VisitsQuery, visits: &[VisitRow]) -> String {
    let day = |t: DateTime<Utc>| t.format("%Y-%m-%d").to_string();
    let first = q
        .start
        .or_else(|| visits.last().map(|v| v.collected_at))
        .map(day)
        .unwrap_or_default();
    let last = q
        .end
        .or_else(|| visits.first().map(|v| v.collected_at))
        .map(day)
        .unwrap_or_default();
    let site: String = site_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{site}_visits_{first}_{last}.csv")
}

/// List a site's visits (collection events), newest first, with per-visit fill and finding
/// counts. Every visit unless a page is asked for. `format=csv` returns the grid as displayed.
/// Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/sites/{id}/visits",
    params(("id" = String, Path, description = "Site UUID or name"), VisitsQuery),
    responses(
        (status = 200, description = "Visits, newest first", body = VisitsResponse),
        (status = 404, description = "Site not found"),
    ),
    tag = "collection_events"
)]
pub async fn list_site_visits(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(site_id): Path<String>,
    Query(q): Query<VisitsQuery>,
) -> AppResult<Response> {
    let site = resolve_site(&state.db, &site_id).await?;
    if scope.is_restricted() && !scope.allows_project_opt(site.project_id) {
        return Err(AppError::NotFound("Site not found".to_string()));
    }

    let paging = paging(q.page, q.page_size);
    let mut binds: Vec<sea_orm::Value> = vec![site.id.into()];
    let range = range_clause(q.start, q.end, &mut binds);

    let mut count_query = super::Entity::find().filter(super::Column::SiteId.eq(site.id));
    if let Some(start) = q.start {
        count_query = count_query.filter(super::Column::CollectedAt.gte(start));
    }
    if let Some(end) = q.end {
        count_query = count_query.filter(super::Column::CollectedAt.lte(end));
    }
    let total = i64::try_from(count_query.count(&state.db).await?).unwrap_or(i64::MAX);

    // The column set is every parameter the site's visits carry, plus every active slot configured
    // on it, at any cadence. The first arm reads the readings through
    // `collection_events` rather than by an unbounded DISTINCT over the hypertable, which pays a
    // planning cost proportional to the chunk count on every page load; `samples` cannot speak for
    // it, since a measurement taken once forms no row there. Taking the union means a parameter
    // with no reading yet still gets a column, so its value has somewhere to render and the fill
    // ratio cannot exceed its own denominator.
    let p = Alias::new("p");
    let sp = Alias::new("sp");
    let measured_here = SeaQuery::select()
        .column((Alias::new("r"), readings::Column::ParameterId))
        .from_as(readings::Entity, Alias::new("r"))
        .inner_join(
            events::Entity,
            Expr::col((events::Entity, events::Column::Id))
                .equals((Alias::new("r"), readings::Column::CollectionEventId)),
        )
        .and_where(Expr::col((events::Entity, events::Column::SiteId)).eq(site.id))
        .take();
    let declared_here = SeaQuery::select()
        .column(site_parameters::Column::ParameterId)
        .from(site_parameters::Entity)
        .and_where(Expr::col(site_parameters::Column::SiteId).eq(site.id))
        .and_where(Expr::cust("COALESCE(is_active, true) = true"))
        .take();
    let expected_query = SeaQuery::select()
        .distinct_on([(p.clone(), parameters::Column::Code)])
        .column((p.clone(), parameters::Column::Id))
        .column((p.clone(), parameters::Column::Code))
        .column((p.clone(), parameters::Column::Name))
        .expr_as(
            Expr::col((p.clone(), parameters::Column::DefaultUnits)),
            Alias::new("units"),
        )
        .column((sp.clone(), site_parameters::Column::DecimalPlaces))
        .from_as(parameters::Entity, p.clone())
        .join_as(
            JoinType::LeftJoin,
            site_parameters::Entity,
            sp.clone(),
            Condition::all()
                .add(
                    Expr::col((sp.clone(), site_parameters::Column::ParameterId))
                        .equals((p.clone(), parameters::Column::Id)),
                )
                .add(Expr::col((sp.clone(), site_parameters::Column::SiteId)).eq(site.id)),
        )
        .cond_where(
            Condition::any()
                .add(Expr::col((p.clone(), parameters::Column::Id)).in_subquery(measured_here))
                .add(Expr::col((p.clone(), parameters::Column::Id)).in_subquery(declared_here)),
        )
        .order_by((p.clone(), parameters::Column::Code), Order::Asc)
        .order_by((sp.clone(), site_parameters::Column::Id), Order::Asc)
        .take();
    let (expected_sql, expected_values) = expected_query.build(PostgresQueryBuilder);
    let expected_rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            expected_sql,
            expected_values,
        ))
        .await?;
    let mut expected_parameters = Vec::with_capacity(expected_rows.len());
    for r in &expected_rows {
        let r = ExpectedRow::from_query_result(r, "")?;
        expected_parameters.push(ExpectedParameter {
            parameter_id: r.id,
            code: r.code,
            name: r.name,
            units: r.units,
            decimal_places: r.decimal_places,
            written_by: None,
            read_by: Vec::new(),
        });
    }
    let columns: Vec<Uuid> = expected_parameters.iter().map(|p| p.parameter_id).collect();
    let impacts =
        crate::routes::private::tools::service::calculations_fed_by(&state.db, &columns).await?;
    for column in &mut expected_parameters {
        (column.read_by, column.written_by) =
            service::parameter_roles(&impacts, column.parameter_id);
    }

    let mut page_binds = binds;
    let limit = limit_clause(paging, &mut page_binds);
    let counts = visit_count_columns();
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT ce.id, ce.collected_at, ce.source, ce.created_by, ce.notes, \
                        ce.unverified, ce.withdrawn_at, {counts} \
                 FROM collection_events ce \
                 WHERE ce.site_id = $1{range} \
                 ORDER BY ce.collected_at DESC{limit}"
            ),
            page_binds,
        ))
        .await?;

    let mut visits: Vec<VisitRow> = rows
        .iter()
        .map(|r| SiteVisitHeader::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|r| VisitRow {
            id: r.id,
            collected_at: r.collected_at,
            source: r.source,
            created_by: r.created_by,
            notes: r.notes,
            parameters_filled: r.filled,
            findings_open: r.findings_open,
            unverified: r.unverified,
            withdrawn_at: r.withdrawn_at,
            recompute: String::new(),
            cells: Vec::new(),
        })
        .collect();
    let event_ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    let recompute = service::status_for(&state.db, &event_ids).await?;
    for visit in &mut visits {
        visit.recompute = recompute
            .get(&visit.id)
            .cloned()
            .unwrap_or_else(|| "current".to_string());
    }

    // One pass over the page's events fills the grid cells: served value per (event, parameter)
    // plus the all-flagged/all-withdrawn state, then the open finding kinds.
    if !event_ids.is_empty() {
        // The served value is the sample mean where a replicate group formed one, else the lowest
        // unflagged replicate's own value. The aggregates stay `Expr::cust`: FILTER, BOOL_AND and
        // the array subscript have no builder form.
        let r = Alias::new("r");
        let s_ = Alias::new("s");
        let agg =
            |sql: &str, name: &str| (Expr::cust(sql.to_string()), Alias::new(name.to_string()));
        let mut cell_query = SeaQuery::select();
        cell_query
            .expr_as(
                Expr::col((r.clone(), readings::Column::CollectionEventId)),
                Alias::new("event_id"),
            )
            .column((r.clone(), readings::Column::ParameterId));
        for (expr, name) in [
            agg(
                "COALESCE(MAX(s.mean), \
                 (ARRAY_AGG(COALESCE(r.calibrated_value, r.raw_value) ORDER BY r.replicate_index) \
                  FILTER (WHERE r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL))[1])",
                "value",
            ),
            agg("BOOL_AND(r.is_flagged IS TRUE)", "all_flagged"),
            agg("BOOL_AND(r.withdrawn_at IS NOT NULL)", "all_withdrawn"),
            agg("COUNT(*)::bigint", "n_total"),
            agg(
                "COUNT(*) FILTER (WHERE r.is_flagged IS TRUE)::bigint",
                "n_flagged",
            ),
            agg(
                "COUNT(*) FILTER (WHERE r.withdrawn_at IS NOT NULL)::bigint",
                "n_withdrawn",
            ),
            agg(
                "COUNT(*) FILTER (WHERE r.unverified IS TRUE)::bigint",
                "n_unverified",
            ),
            // A single measurement forms no `samples` row, and the serving arm reports it as
            // n = 1 (`routes/public/views.rs`). Count what the mean would stand on, so the two
            // surfaces agree and a lone excluded replicate still says zero.
            agg(
                "COALESCE(MAX(s.n), COUNT(*) FILTER (WHERE r.unverified IS NOT TRUE \
                   AND r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL)::int)",
                "sample_n",
            ),
            agg("MAX(s.stdev)", "stdev"),
            agg("MAX(s.median)", "median"),
            agg("MAX(s.min_value)", "min_value"),
            agg("MAX(s.max_value)", "max_value"),
            // The replicates themselves, as parallel arrays in one index order: a composite array
            // would decode by hand, and the four are read back together or not at all.
            agg(
                "ARRAY_AGG(r.replicate_index ORDER BY r.replicate_index)",
                "replicate_indexes",
            ),
            agg(
                "ARRAY_AGG(COALESCE(r.calibrated_value, r.raw_value) ORDER BY r.replicate_index)",
                "replicate_values",
            ),
            agg(
                "ARRAY_AGG(r.is_flagged IS TRUE ORDER BY r.replicate_index)",
                "replicate_flagged",
            ),
            agg(
                "ARRAY_AGG(r.withdrawn_at IS NOT NULL ORDER BY r.replicate_index)",
                "replicate_withdrawn",
            ),
            agg(
                "ARRAY_AGG(r.unverified IS TRUE ORDER BY r.replicate_index)",
                "replicate_unverified",
            ),
            agg(
                "ARRAY_AGG(r.stream_id ORDER BY r.replicate_index)",
                "replicate_streams",
            ),
            agg("BOOL_OR(r.provenance IS NOT NULL)", "has_provenance"),
            agg("MAX(r.provenance ->> 'tool')", "tool"),
            agg("(MAX(r.provenance ->> 'run_id'))::uuid", "tool_run_id"),
            agg(
                "ARRAY_AGG(r.standard_curve_id ORDER BY r.replicate_index) \
                 FILTER (WHERE r.standard_curve_id IS NOT NULL)",
                "replicate_curves",
            ),
        ] {
            cell_query.expr_as(expr, name);
        }
        let cell_query = cell_query
            .from_as(readings::Entity, r.clone())
            .join_as(
                JoinType::LeftJoin,
                samples::Entity,
                s_.clone(),
                Expr::col((s_.clone(), samples::Column::Id))
                    .equals((r.clone(), readings::Column::SampleId)),
            )
            .and_where(Expr::cust_with_values(
                "r.collection_event_id = ANY($1)",
                [event_ids.clone()],
            ))
            .and_where(Expr::col((r.clone(), readings::Column::ParameterId)).is_not_null())
            .add_group_by([
                Expr::col((r.clone(), readings::Column::CollectionEventId)),
                Expr::col((r.clone(), readings::Column::ParameterId)),
            ])
            .take();
        let (cell_sql, cell_values) = cell_query.build(PostgresQueryBuilder);
        let cell_rows = state
            .db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                cell_sql,
                cell_values,
            ))
            .await?;
        let finding_rows = state
            .db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                // A hold is keyed on the slot (an event-audit finding) or on the stream that
                // raised it (a statistics disagreement, a source modification, a brake). Both
                // land at a visit's instant and both belong in its grid, so the stream's own
                // pairing resolves the slot rather than the hold being skipped for lacking one.
                // Oldest first, matching the detail endpoint, so the two grids cannot disagree
                // about which finding a cell carries.
                format!(
                    "SELECT COALESCE(h.parameter_id, sp.parameter_id) AS parameter_id, \
                        h.group_time, h.kind, h.created_at \
                 FROM replicate_audit_holds h \
                 LEFT JOIN data_streams ds ON ds.id = h.stream_id \
                 LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
                 JOIN collection_events ce \
                   ON ce.site_id = COALESCE(h.site_id, sp.site_id) \
                  AND ce.collected_at = h.group_time \
                 WHERE h.status = '{pending}' AND ce.id = ANY($1) \
                   AND COALESCE(h.parameter_id, sp.parameter_id) IS NOT NULL \
                 ORDER BY h.created_at",
                    pending = HoldStatus::Pending.as_str()
                ),
                [event_ids.into()],
            ))
            .await?;
        // Oldest wins, and the rest are counted: a cell carrying two open findings says so
        // rather than picking one silently.
        let mut findings: std::collections::HashMap<(DateTime<Utc>, Uuid), (String, i64)> =
            std::collections::HashMap::new();
        for f in &finding_rows {
            let f = VisitFindingRow::from_query_result(f, "")?;
            findings
                .entry((f.group_time.with_timezone(&Utc), f.parameter_id))
                .and_modify(|(_, n)| *n += 1)
                .or_insert((f.kind, 1));
        }
        let cell_rows = cell_rows
            .iter()
            .map(|c| CellRow::from_query_result(c, ""))
            .collect::<Result<Vec<_>, _>>()?;
        let curve_names = curve_names(&state.db, &cell_rows).await?;
        let mut by_event: std::collections::HashMap<Uuid, Vec<VisitCell>> =
            std::collections::HashMap::new();
        for c in cell_rows {
            let replicates = replicates_of(&c);
            let curves = cell_curves(
                c.replicate_curves.as_deref().unwrap_or_default(),
                &curve_names,
            );
            by_event.entry(c.event_id).or_default().push(VisitCell {
                parameter_id: c.parameter_id,
                value: c.value,
                flagged: c.all_flagged.unwrap_or(false),
                withdrawn: c.all_withdrawn.unwrap_or(false),
                n_total: c.n_total,
                n_flagged: c.n_flagged,
                n_withdrawn: c.n_withdrawn,
                n_unverified: c.n_unverified,
                n: c.sample_n,
                stdev: c.stdev,
                median: c.median,
                min: c.min_value,
                max: c.max_value,
                finding: None,
                finding_count: None,
                replicates,
                has_provenance: c.has_provenance.unwrap_or(false),
                tool: c.tool,
                tool_run_id: c.tool_run_id,
                curves,
            });
        }
        for visit in &mut visits {
            let mut cells = by_event.remove(&visit.id).unwrap_or_default();
            for cell in &mut cells {
                if let Some((kind, n)) = findings.get(&(visit.collected_at, cell.parameter_id)) {
                    cell.finding = Some(kind.clone());
                    cell.finding_count = (*n > 1).then_some(*n);
                }
            }
            // A missing-output finding names a parameter with no readings; it still gets a cell.
            for ((at, parameter_id), (kind, n)) in &findings {
                if *at == visit.collected_at
                    && !cells.iter().any(|c| c.parameter_id == *parameter_id)
                {
                    cells.push(VisitCell {
                        parameter_id: *parameter_id,
                        value: None,
                        flagged: false,
                        withdrawn: false,
                        n_total: 0,
                        n_flagged: 0,
                        n_withdrawn: 0,
                        n_unverified: 0,
                        n: None,
                        stdev: None,
                        median: None,
                        min: None,
                        max: None,
                        finding: Some(kind.clone()),
                        finding_count: (*n > 1).then_some(*n),
                        replicates: Vec::new(),
                        has_provenance: false,
                        tool: None,
                        tool_run_id: None,
                        curves: Vec::new(),
                    });
                }
            }
            visit.cells = cells;
        }
    }

    if q.format.as_deref() == Some("csv") {
        let filename = visits_filename(&site.name, &q, &visits);
        let csv = visits_csv(&expected_parameters, &visits);
        return Response::builder()
            .header(header::CONTENT_TYPE, HeaderValue::from_static("text/csv"))
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            )
            .body(axum::body::Body::from(csv))
            .map_err(|e| AppError::Internal(e.to_string()));
    }

    let total = u64::try_from(total).unwrap_or(0);
    Ok(Json(VisitsResponse {
        site_id: site.id,
        total,
        page: paging.map_or(1, Window::page),
        page_size: paging.map_or(total, |w| w.limit),
        expected_parameters,
        visits,
    })
    .into_response())
}

/// List visits across sites with per-visit fill and finding counts. Requires `read_data`; a
/// project-scoped caller sees the visits of its projects' sites.
#[utoipa::path(
    get,
    path = "/api/visits",
    params(VisitListQuery),
    responses(
        (status = 200, description = "Visits", body = Page<VisitListRow>),
        (status = 400, description = "Unknown sort or order"),
    ),
    tag = "collection_events"
)]
pub async fn list_visits(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<VisitListQuery>,
) -> AppResult<Json<Page<VisitListRow>>> {
    let order_by = visit_list_order(q.sort.as_deref(), q.order.as_deref())?;
    let paging = paging(q.page, q.page_size.or(Some(100)));
    let mut binds: Vec<sea_orm::Value> = Vec::new();
    let mut filter = String::from("WHERE true");
    if let Some(projects) = scope.sql_project_array() {
        binds.push(projects);
        filter.push_str(&format!(" AND s.project_id = ANY(${})", binds.len()));
    }
    if let Some(site_id) = q.site_id {
        binds.push(site_id.into());
        filter.push_str(&format!(" AND ce.site_id = ${}", binds.len()));
    }
    filter.push_str(&range_clause(q.start, q.end, &mut binds));

    let total: i64 = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COUNT(*) AS n FROM collection_events ce \
                 JOIN sites s ON s.id = ce.site_id {filter}"
            ),
            binds.clone(),
        ))
        .await?
        .map(|r| r.try_get::<i64>("", "n"))
        .transpose()?
        .unwrap_or(0);

    let limit = limit_clause(paging, &mut binds);
    let counts = visit_count_columns();
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT ce.id, ce.site_id, s.name AS site_name, ce.collected_at, ce.source, \
                        ce.created_by, ce.notes, ce.unverified, ce.withdrawn_at, {counts} \
                 FROM collection_events ce \
                 JOIN sites s ON s.id = ce.site_id \
                 {filter} \
                 ORDER BY {order_by}{limit}"
            ),
            binds,
        ))
        .await?;
    let mut visits: Vec<VisitListRow> = rows
        .iter()
        .map(|r| VisitHeader::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|r| VisitListRow {
            id: r.id,
            site_id: r.site_id,
            site_name: r.site_name,
            collected_at: r.collected_at,
            source: r.source,
            created_by: r.created_by,
            notes: r.notes,
            parameters_filled: r.filled,
            findings_open: r.findings_open,
            unverified: r.unverified,
            withdrawn_at: r.withdrawn_at,
            recompute: String::new(),
        })
        .collect();
    let event_ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    let recompute = service::status_for(&state.db, &event_ids).await?;
    for visit in &mut visits {
        visit.recompute = recompute
            .get(&visit.id)
            .cloned()
            .unwrap_or_else(|| "current".to_string());
    }

    let total = u64::try_from(total).unwrap_or(0);
    Ok(Json(Page::new(visits, total, paging)))
}

/// A visit's header row as the per-site list selects it.
#[derive(Debug, FromQueryResult)]
struct SiteVisitHeader {
    id: Uuid,
    collected_at: DateTime<Utc>,
    source: String,
    created_by: Option<String>,
    notes: Option<String>,
    unverified: bool,
    withdrawn_at: Option<DateTime<Utc>>,
    filled: i64,
    findings_open: i64,
}

/// The same header with the site the cross-site list carries beside it.
#[derive(Debug, FromQueryResult)]
struct VisitHeader {
    id: Uuid,
    site_id: Uuid,
    site_name: String,
    collected_at: DateTime<Utc>,
    source: String,
    created_by: Option<String>,
    notes: Option<String>,
    unverified: bool,
    withdrawn_at: Option<DateTime<Utc>>,
    filled: i64,
    findings_open: i64,
}

/// The three list queries this module makes that fill a shape of their own.
#[derive(FromQueryResult)]
struct ExpectedRow {
    id: Uuid,
    code: String,
    name: String,
    units: Option<String>,
    decimal_places: Option<i16>,
}

#[derive(FromQueryResult)]
struct VisitFindingRow {
    group_time: sea_orm::prelude::DateTimeWithTimeZone,
    parameter_id: Uuid,
    kind: String,
}

#[derive(FromQueryResult)]
struct CellRow {
    event_id: Uuid,
    parameter_id: Uuid,
    value: Option<f64>,
    all_flagged: Option<bool>,
    all_withdrawn: Option<bool>,
    n_total: i64,
    n_flagged: i64,
    n_withdrawn: i64,
    n_unverified: i64,
    sample_n: Option<i32>,
    stdev: Option<f64>,
    median: Option<f64>,
    min_value: Option<f64>,
    max_value: Option<f64>,
    replicate_indexes: Vec<i16>,
    replicate_values: Vec<f64>,
    replicate_flagged: Vec<bool>,
    replicate_withdrawn: Vec<bool>,
    replicate_unverified: Vec<bool>,
    replicate_streams: Vec<Uuid>,
    has_provenance: Option<bool>,
    tool: Option<String>,
    tool_run_id: Option<Uuid>,
    replicate_curves: Option<Vec<Uuid>>,
}

/// The name of every curve the page's cells were corrected through, in one lookup.
async fn curve_names(
    db: &sea_orm::DatabaseConnection,
    rows: &[CellRow],
) -> AppResult<std::collections::HashMap<Uuid, Option<String>>> {
    let ids: std::collections::BTreeSet<Uuid> = rows
        .iter()
        .flat_map(|r| r.replicate_curves.iter().flatten().copied())
        .collect();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(standard_curves::Entity::find()
        .filter(standard_curves::Column::Id.is_in(ids))
        .all(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect())
}

/// A cell's curves: each distinct one its replicates name, in replicate order.
fn cell_curves(
    replicate_curves: &[Uuid],
    names: &std::collections::HashMap<Uuid, Option<String>>,
) -> Vec<VisitCellCurve> {
    let mut curves: Vec<VisitCellCurve> = Vec::new();
    for id in replicate_curves {
        if curves.iter().any(|c| c.id == *id) {
            continue;
        }
        curves.push(VisitCellCurve {
            id: *id,
            name: names.get(id).cloned().flatten(),
        });
    }
    curves
}

/// The five parallel arrays one `ARRAY_AGG` group returns, read back as replicates. They come out
/// of one group over one ordering, so position `i` is the same replicate in each.
fn replicates_of(row: &CellRow) -> Vec<VisitReplicate> {
    row.replicate_indexes
        .iter()
        .zip(&row.replicate_values)
        .enumerate()
        .map(|(i, (replicate_index, value))| VisitReplicate {
            replicate_index: *replicate_index,
            value: *value,
            stream_id: row.replicate_streams.get(i).copied().unwrap_or_default(),
            flagged: row.replicate_flagged.get(i).copied().unwrap_or(false),
            withdrawn: row.replicate_withdrawn.get(i).copied().unwrap_or(false),
            unverified: row.replicate_unverified.get(i).copied().unwrap_or(false),
        })
        .collect()
}

/// One replicate row of a visit's grid, as the detail query selects it. The fold below groups
/// these into cells; decoding them by hand was 28 `try_get` calls nothing checked against the
/// SELECT.
#[derive(Debug, FromQueryResult)]
struct DetailRow {
    parameter_id: Uuid,
    code: String,
    name: String,
    stream_id: Uuid,
    source_system: Option<String>,
    source_key: Option<String>,
    replicate_index: i16,
    raw_value: f64,
    calibrated_value: Option<f64>,
    is_flagged: Option<bool>,
    withdrawn: bool,
    sample_id: Option<Uuid>,
    sample_mean: Option<f64>,
    sample_stdev: Option<f64>,
    sample_n: Option<i32>,
    sample_median: Option<f64>,
    sample_min: Option<f64>,
    sample_max: Option<f64>,
    flag_reason: Option<String>,
    withdrawn_at: Option<DateTime<Utc>>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
    sensor_id: Option<Uuid>,
    sensor_kind: Option<String>,
    sensor_is_lab: Option<bool>,
    unverified: bool,
    has_provenance: Option<bool>,
    provenance_kind: Option<String>,
    tool: Option<String>,
    tool_run_id: Option<Uuid>,
}

/// One open finding on a visit's parameter.
#[derive(Debug, FromQueryResult)]
struct FindingRow {
    id: Uuid,
    kind: String,
    parameter_id: Uuid,
    tool: Option<String>,
    status: String,
}

/// One visit's grid row: every parameter measured at the event with its replicates, sample
/// statistics, tool provenance presence, and any open finding — plus findings for parameters
/// the audit says are missing entirely. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/collection_events/{id}/detail",
    params(("id" = Uuid, Path, description = "Collection event id")),
    responses(
        (status = 200, description = "The visit's cells", body = EventDetailResponse),
        (status = 404, description = "Unknown collection event"),
    ),
    tag = "collection_events"
)]
pub async fn get_event_detail(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(id): Path<Uuid>,
) -> AppResult<Json<EventDetailResponse>> {
    let event = super::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Collection event {id} not found")))?;
    if scope.is_restricted() {
        let project = crate::routes::private::sites::Entity::find_by_id(event.site_id)
            .one(&state.db)
            .await?
            .and_then(|s| s.project_id);
        if !scope.allows_project_opt(project) {
            return Err(AppError::NotFound(format!(
                "Collection event {id} not found"
            )));
        }
    }

    let r = Alias::new("r");
    let p = Alias::new("p");
    let ds = Alias::new("ds");
    let s_ = Alias::new("s");
    let sn = Alias::new("sn");
    let mut detail_query = SeaQuery::select();
    detail_query
        .column((r.clone(), readings::Column::ParameterId))
        .column((p.clone(), parameters::Column::Code))
        .column((p.clone(), parameters::Column::Name))
        .column((r.clone(), readings::Column::StreamId))
        .column((ds.clone(), data_streams::Column::SourceSystem))
        .column((ds.clone(), data_streams::Column::SourceKey))
        .column((r.clone(), readings::Column::ReplicateIndex))
        .column((r.clone(), readings::Column::RawValue))
        .column((r.clone(), readings::Column::CalibratedValue))
        .column((r.clone(), readings::Column::IsFlagged))
        .expr_as(
            Expr::col((r.clone(), readings::Column::WithdrawnAt)).is_not_null(),
            Alias::new("withdrawn"),
        )
        .column((r.clone(), readings::Column::SampleId))
        .expr_as(
            Expr::col((s_.clone(), samples::Column::Mean)),
            Alias::new("sample_mean"),
        )
        .expr_as(
            Expr::col((s_.clone(), samples::Column::Stdev)),
            Alias::new("sample_stdev"),
        )
        .expr_as(
            Expr::col((s_.clone(), samples::Column::N)),
            Alias::new("sample_n"),
        )
        .expr_as(
            Expr::col((s_.clone(), samples::Column::Median)),
            Alias::new("sample_median"),
        )
        .expr_as(
            Expr::col((s_.clone(), samples::Column::MinValue)),
            Alias::new("sample_min"),
        )
        .expr_as(
            Expr::col((s_.clone(), samples::Column::MaxValue)),
            Alias::new("sample_max"),
        )
        .column((r.clone(), readings::Column::FlagReason))
        .column((r.clone(), readings::Column::WithdrawnAt))
        .column((r.clone(), readings::Column::CalibrationId))
        .column((r.clone(), readings::Column::StandardCurveId))
        .column((r.clone(), readings::Column::SensorId))
        .expr_as(
            Expr::col((sn.clone(), sensors::Column::Kind)),
            Alias::new("sensor_kind"),
        )
        .expr_as(
            Expr::col((sn.clone(), sensors::Column::IsLabInstrument)),
            Alias::new("sensor_is_lab"),
        )
        .column((r.clone(), readings::Column::Unverified))
        .expr_as(
            Expr::col((r.clone(), readings::Column::Provenance)).is_not_null(),
            Alias::new("has_provenance"),
        )
        .column((r.clone(), readings::Column::ProvenanceKind))
        .expr_as(Expr::cust("r.provenance ->> 'tool'"), Alias::new("tool"))
        .expr_as(
            Expr::cust("(r.provenance ->> 'run_id')::uuid"),
            Alias::new("tool_run_id"),
        )
        .from_as(readings::Entity, r.clone())
        .join_as(
            JoinType::InnerJoin,
            parameters::Entity,
            p.clone(),
            Expr::col((p.clone(), parameters::Column::Id))
                .equals((r.clone(), readings::Column::ParameterId)),
        )
        .join_as(
            JoinType::LeftJoin,
            data_streams::Entity,
            ds.clone(),
            Expr::col((ds.clone(), data_streams::Column::Id))
                .equals((r.clone(), readings::Column::StreamId)),
        )
        .join_as(
            JoinType::LeftJoin,
            samples::Entity,
            s_.clone(),
            Expr::col((s_.clone(), samples::Column::Id))
                .equals((r.clone(), readings::Column::SampleId)),
        )
        .join_as(
            JoinType::LeftJoin,
            sensors::Entity,
            sn.clone(),
            Expr::col((sn.clone(), sensors::Column::Id))
                .equals((r.clone(), readings::Column::SensorId)),
        )
        .and_where(Expr::col((r.clone(), readings::Column::CollectionEventId)).eq(id))
        .order_by((p.clone(), parameters::Column::Code), Order::Asc)
        .order_by((r.clone(), readings::Column::StreamId), Order::Asc)
        .order_by((r.clone(), readings::Column::ReplicateIndex), Order::Asc);
    let (detail_sql, detail_values) = detail_query.build(PostgresQueryBuilder);
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            detail_sql,
            detail_values,
        ))
        .await?
        .iter()
        .map(|r| DetailRow::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?;

    let findings = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id, kind, parameter_id, tool, status FROM replicate_audit_holds \
                 WHERE stream_id IS NULL AND site_id = $1 AND group_time = $2 \
                   AND status = '{pending}' \
                 ORDER BY created_at",
                pending = HoldStatus::Pending.as_str()
            ),
            [event.site_id.into(), event.collected_at.into()],
        ))
        .await?
        .iter()
        .map(|f| FindingRow::from_query_result(f, ""))
        .collect::<Result<Vec<_>, _>>()?;
    let mut finding_by_param: std::collections::HashMap<Uuid, CellFinding> =
        std::collections::HashMap::new();
    for f in findings {
        finding_by_param
            .entry(f.parameter_id)
            .or_insert(CellFinding {
                id: f.id,
                kind: f.kind,
                tool: f.tool,
                status: f.status,
            });
    }

    // Fold reading rows into per-(parameter, stream) cells.
    let mut cells: Vec<EventCell> = Vec::new();
    for r in rows {
        let replicate = CellReplicate {
            replicate_index: r.replicate_index,
            raw_value: r.raw_value,
            calibrated_value: r.calibrated_value,
            flagged: r.is_flagged.unwrap_or(false),
            flag_reason: r.flag_reason,
            withdrawn: r.withdrawn,
            withdrawn_at: r.withdrawn_at,
            calibration_id: r.calibration_id,
            standard_curve_id: r.standard_curve_id,
            sensor_id: r.sensor_id,
            sensor_kind: r.sensor_id.map(|_| {
                InstrumentKind::of(r.sensor_kind.as_deref(), r.sensor_is_lab)
                    .as_str()
                    .to_string()
            }),
            unverified: r.unverified,
        };
        let same_cell = cells
            .last_mut()
            .filter(|c| c.parameter_id == r.parameter_id && c.stream_id == r.stream_id);
        match same_cell {
            Some(cell) => cell.replicates.push(replicate),
            None => {
                let sample = r.sample_id.map(|sample_id| CellSample {
                    sample_id,
                    mean: r.sample_mean,
                    stdev: r.sample_stdev,
                    median: r.sample_median,
                    min: r.sample_min,
                    max: r.sample_max,
                    n: r.sample_n.unwrap_or(0),
                });
                cells.push(EventCell {
                    parameter_id: r.parameter_id,
                    origin: crate::routes::private::readings::service::classify_source(
                        r.source_system.as_deref().unwrap_or(""),
                    )
                    .to_string(),
                    has_provenance: r.has_provenance.unwrap_or(false),
                    provenance_kind: r.provenance_kind,
                    tool: r.tool,
                    tool_run_id: r.tool_run_id,
                    source_system: r.source_system,
                    source_key: r.source_key,
                    parameter_code: r.code,
                    parameter_name: r.name,
                    stream_id: r.stream_id,
                    served_value: None,
                    sample,
                    replicates: vec![replicate],
                    record: None,
                    finding: finding_by_param.remove(&r.parameter_id),
                    read_by: Vec::new(),
                    written_by: None,
                });
            }
        }
    }
    for cell in &mut cells {
        let live_value = cell
            .replicates
            .iter()
            .find(|r| !r.flagged && !r.withdrawn)
            .map(|r| r.calibrated_value.unwrap_or(r.raw_value));
        cell.served_value = cell.sample.as_ref().and_then(|s| s.mean).or(live_value);
    }
    // Findings for parameters with no readings at the event (missing outputs) still get a cell.
    // One filtered find for all of them: an audit reporting eight missing outputs at one visit was
    // costing eight round trips on the request path.
    let finding_catalog = crate::routes::private::site_parameters::service::catalog_map(
        &state.db,
        finding_by_param.keys().copied(),
    )
    .await?;
    for (parameter_id, finding) in finding_by_param {
        let catalog = finding_catalog.get(&parameter_id);
        cells.push(EventCell {
            parameter_id,
            parameter_code: catalog.map(|c| c.code.clone()).unwrap_or_default(),
            parameter_name: catalog.map(|c| c.name.clone()).unwrap_or_default(),
            stream_id: Uuid::nil(),
            origin: crate::routes::private::readings::service::classify_source("").to_string(),
            has_provenance: false,
            provenance_kind: None,
            tool: None,
            tool_run_id: None,
            source_system: None,
            source_key: None,
            served_value: None,
            sample: None,
            replicates: Vec::new(),
            record: None,
            finding: Some(finding),
            read_by: Vec::new(),
            written_by: None,
        });
    }
    let mut records = crate::routes::private::readings::service::records_for_event(
        &state.db,
        event.id,
        event.collected_at,
    )
    .await?;
    for cell in &mut cells {
        cell.record = records.remove(&cell.stream_id);
    }

    // Which calculation reads each cell, and which writes it: the grid colours by role and names
    // the script in the tooltip, so the consequence of an edit is visible before it is made.
    let touched: Vec<Uuid> = cells.iter().map(|c| c.parameter_id).collect();
    let impacts =
        crate::routes::private::tools::service::calculations_fed_by(&state.db, &touched).await?;
    for cell in &mut cells {
        (cell.read_by, cell.written_by) = service::parameter_roles(&impacts, cell.parameter_id);
    }

    let recompute = service::status_for(&state.db, &[event.id])
        .await?
        .remove(&event.id)
        .unwrap_or_else(|| "current".to_string());
    Ok(Json(EventDetailResponse {
        id: event.id,
        site_id: event.site_id,
        collected_at: event.collected_at,
        source: event.source,
        created_by: event.created_by,
        notes: event.notes,
        unverified: event.unverified,
        withdrawn_at: event.withdrawn_at,
        recompute,
        cells,
    }))
}

#[cfg(test)]
#[path = "tests/views.rs"]
mod tests;
