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
use sea_orm::sea_query::{Alias, Expr, JoinType, PostgresQueryBuilder, Query as SeaQuery};
use sea_orm::{ConnectionTrait, EntityTrait, ExprTrait, FromQueryResult, Order, Statement};
use uuid::Uuid;

use super::models::{
    CellFinding, CellReplicate, CellSample, EnqueuedJobResponse, Entity, EventAuditRequest,
    EventCell, EventDetailResponse, EventRecomputeRequest, ExpectedParameter, PreviewEventRequest,
    PreviewUnstagedRequest, SiteVisitCount, StageEventRequest, StageEventsRequest, StageVisitRow,
    StagedEvent, VisitListQuery, VisitListRow, VisitRow, VisitSitesQuery, VisitsQuery,
    VisitsResponse,
};
use super::service::{self, paging, visit_list_order};
use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::common::paging::{Page, Window};
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::parameters::models as parameters;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::samples::models as samples;
use crate::routes::private::sensors::models::{self as sensors, InstrumentKind};
use crate::routes::private::sync::hold_model as holds;
use crate::routes::private::sync::models::HoldStatus;
use crate::routes::private::tools::models::EventPreview;
use crate::routes::private::tools::service::parse_ids;
use crate::routes::resolve_site;

/// Recompute a collection event's tool outputs on demand: the chain executor runs every active
/// tool whose inputs resolve at this event, in dependency order, and saves the outputs through
/// the grab write path with fresh server-built provenance. Tracked job. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/collection_events/{id}/recompute",
    params(("id" = Uuid, Path, description = "Collection event id")),
    responses(
        (status = 200, description = "The tracked recompute job", body = EnqueuedJobResponse),
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
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "event_recompute",
        None,
        Some(event.site_id),
        &serde_json::json!({
            "collection_event_id": id,
            "site_id": event.site_id,
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
    let filter = service::visit_filter(Some(site.id), None, q.start, q.end);
    let total = service::count_visits(&state.db, filter.clone()).await?;
    let expected_parameters = service::expected_parameters(&state.db, site.id).await?;
    let mut headers =
        service::visit_headers_page(&state.db, filter, &service::newest_first(), paging).await?;
    service::attach_recompute_status(&state.db, &mut headers).await?;
    let mut visits: Vec<VisitRow> = headers.into_iter().map(VisitRow::from).collect();
    service::fill_visit_cells(&state.db, &mut visits).await?;

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

/// The sites with a visit holding a live value of every parameter named, each with how many such
/// visits it has: where a calculation reading those parameters can run. Requires `read_data`; a
/// project-scoped caller sees its projects' sites.
#[utoipa::path(
    get,
    path = "/api/visits/sites",
    params(VisitSitesQuery),
    responses(
        (status = 200, description = "Sites and their visit counts", body = Vec<SiteVisitCount>),
        (status = 400, description = "A parameter id is not a UUID"),
    ),
    tag = "collection_events"
)]
pub async fn list_visit_sites(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<VisitSitesQuery>,
) -> AppResult<Json<Vec<SiteVisitCount>>> {
    let holding = parse_ids(Some(&q.holding))?;
    if holding.is_empty() {
        return Err(AppError::BadRequest(
            "holding names no parameter".to_string(),
        ));
    }
    Ok(Json(
        service::sites_holding(&state.db, &holding, scope.project_ids()).await?,
    ))
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
    let order = visit_list_order(q.sort.as_deref(), q.order.as_deref())?;
    let paging = paging(q.page, q.page_size.or(Some(100)));
    let holding = parse_ids(q.holding.as_deref())?;
    let filter = service::visit_filter(q.site_id, scope.project_ids(), q.start, q.end)
        .add_option(service::holding(&holding));
    let total = service::count_visits(&state.db, filter.clone()).await?;
    let mut headers = service::visit_headers_page(&state.db, filter, &order, paging).await?;
    service::attach_recompute_status(&state.db, &mut headers).await?;
    let visits = headers.into_iter().map(VisitListRow::from).collect();
    Ok(Json(Page::new(visits, total, paging)))
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

/// The open findings at one visit, oldest first. A hold is keyed on the slot (an event-audit
/// finding) or on the stream that raised it (a statistics disagreement, a source modification, a
/// brake); the stream's pairing places the second on its slot, as the visits listing does.
async fn open_findings_at(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    collected_at: DateTime<Utc>,
) -> Result<Vec<FindingRow>, sea_orm::DbErr> {
    let query = holds::with_stream_slot()
        .column((holds::h(), holds::Column::Id))
        .column((holds::h(), holds::Column::Kind))
        .expr_as(holds::slot_parameter(), Alias::new("parameter_id"))
        .column((holds::h(), holds::Column::Tool))
        .column((holds::h(), holds::Column::Status))
        .and_where(holds::slot_site().eq(site_id))
        .and_where(Expr::col((holds::h(), holds::Column::GroupTime)).eq(collected_at))
        .and_where(Expr::col((holds::h(), holds::Column::Status)).eq(HoldStatus::Pending.as_str()))
        .and_where(holds::slot_parameter().is_not_null())
        .order_by((holds::h(), holds::Column::CreatedAt), Order::Asc)
        .to_owned();
    db.query_all(&query)
        .await?
        .iter()
        .map(|f| FindingRow::from_query_result(f, ""))
        .collect()
}

/// One visit's grid row: every parameter measured at the event with its replicates, sample
/// statistics, tool provenance presence, and any open finding, plus findings for parameters
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

    let findings = open_findings_at(&state.db, event.site_id, event.collected_at).await?;
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
                    computed_curves: Vec::new(),
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
            computed_curves: Vec::new(),
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
    service::attach_computed_curves(&state.db, &mut cells).await?;

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
