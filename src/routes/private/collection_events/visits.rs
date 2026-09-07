//! The visits table: the portal's wide `data` view reborn. One row per collection event at a
//! site, and per event the grid of parameter cells a field date filled in.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, EntityTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::paging::Window;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::resolve_site;

const MAX_PAGE_SIZE: u64 = 200;

#[derive(Debug, Deserialize, IntoParams)]
pub struct VisitsQuery {
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// 1-based page. Absent, with `page_size` absent, lists every visit.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, max 200. Absent, with `page` absent, lists every visit.
    #[serde(default)]
    pub page_size: Option<u64>,
    /// `json` (default) or `csv`: the grid as displayed, one column per expected parameter
    /// headed by its code.
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitRow {
    pub id: Uuid,
    pub collected_at: DateTime<Utc>,
    /// 'manual' | 'portal_sync'.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Parameters with at least one non-withdrawn reading at this visit.
    pub parameters_filled: i64,
    /// Open event-audit findings at this visit.
    pub findings_open: i64,
    /// The visit's recompute state: `current` | `queued` | `running` | `failed` | `stale`.
    pub recompute: String,
    /// One cell per parameter measured at the visit (the wide portal row).
    pub cells: Vec<VisitCell>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitCell {
    pub parameter_id: Uuid,
    /// The served value: sample mean, else the lowest live replicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// Every replicate in the group is flagged.
    pub flagged: bool,
    /// Every replicate in the group is withdrawn.
    pub withdrawn: bool,
    /// Replicates stored, flagged and withdrawn. A partly curated group serves a mean the
    /// exclusions moved, so the counts are what says a value stepped because replicates were
    /// removed rather than because the measurement changed.
    pub n_total: i64,
    pub n_flagged: i64,
    pub n_withdrawn: i64,
    /// The group's statistics, so a triplicate and a single measurement do not render identically.
    /// `n` counts what the mean stands on, which is `n_total` less the exclusions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Which divisor produced `stdev`, and what chose it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sd_estimator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sd_estimator_source: Option<String>,
    /// Kind of the oldest open finding on this cell, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
    /// How many open findings the cell carries, when more than one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding_count: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExpectedParameter {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
    /// The unit the column's numbers are in, from the site's slot when it declares one and the
    /// catalog default otherwise. A grid of bare numbers cannot be read without it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// `site_parameters.decimal_places` for the slot, null when it declares none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimal_places: Option<i16>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitsResponse {
    pub site_id: Uuid,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
    /// The grid's column set, ordered by code: every parameter this site has sampled, plus its
    /// active configured slots, so a parameter whose readings never formed a `samples` row still
    /// has a column to render into.
    pub expected_parameters: Vec<ExpectedParameter>,
    pub visits: Vec<VisitRow>,
}

/// The per-visit counts, computed the same way on the site grid and the cross-site list. A
/// hold is keyed on the slot (an event-audit finding) or on the stream that raised it, so the
/// stream's pairing resolves the site.
const VISIT_COUNT_COLUMNS: &str = "\
    (SELECT COUNT(DISTINCT r.parameter_id) FROM readings r \
      WHERE r.collection_event_id = ce.id AND r.withdrawn_at IS NULL \
        AND r.is_flagged IS NOT TRUE \
        AND r.parameter_id IS NOT NULL) AS filled, \
    (SELECT COUNT(*) FROM replicate_audit_holds h \
      LEFT JOIN data_streams ds ON ds.id = h.stream_id \
      LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
      WHERE h.group_time = ce.collected_at AND h.status = 'pending' \
        AND COALESCE(h.site_id, sp.site_id) = ce.site_id) AS findings_open";

/// Paging is opt-in: a caller naming neither `page` nor `page_size` gets every row.
fn paging(page: Option<u64>, page_size: Option<u64>) -> Option<Window> {
    if page.is_none() && page_size.is_none() {
        return None;
    }
    Some(Window::from_page(
        page,
        page_size,
        MAX_PAGE_SIZE,
        MAX_PAGE_SIZE,
    ))
}

fn range_clause(
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    binds: &mut Vec<sea_orm::Value>,
) -> String {
    let mut range = String::new();
    if let Some(start) = start {
        binds.push(start.into());
        range.push_str(&format!(" AND ce.collected_at >= ${}", binds.len()));
    }
    if let Some(end) = end {
        binds.push(end.into());
        range.push_str(&format!(" AND ce.collected_at <= ${}", binds.len()));
    }
    range
}

fn limit_clause(paging: Option<Window>, binds: &mut Vec<sea_orm::Value>) -> String {
    let Some(window) = paging else {
        return String::new();
    };
    binds.push((window.limit as i64).into());
    let limit_ref = binds.len();
    binds.push((window.offset as i64).into());
    format!(" LIMIT ${limit_ref} OFFSET ${}", binds.len())
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

    let total: i64 = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) AS n FROM collection_events ce WHERE ce.site_id = $1{range}"),
            binds.clone(),
        ))
        .await?
        .map(|r| r.try_get("", "n").unwrap_or(0))
        .unwrap_or(0);

    // The column set is every parameter this site can hold a spot value for: the ones its visits
    // carry, plus the spot-capable slots configured on it. The first arm reads the readings through
    // `collection_events` rather than by an unbounded DISTINCT over the hypertable, which pays a
    // planning cost proportional to the chunk count on every page load; `samples` cannot speak for
    // it, since a measurement taken once forms no row there. Taking the union means a parameter
    // with no reading yet still gets a column, so its value has somewhere to render and the fill
    // ratio cannot exceed its own denominator.
    let expected_rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT ON (p.code) p.id, p.code, p.name, \
                    COALESCE(sp.display_units, p.default_units) AS units, \
                    sp.decimal_places \
             FROM parameters p \
             LEFT JOIN site_parameters sp ON sp.parameter_id = p.id AND sp.site_id = $1 \
             WHERE p.id IN (SELECT r.parameter_id FROM readings r \
                              JOIN collection_events ce ON ce.id = r.collection_event_id \
                             WHERE ce.site_id = $1) \
                OR p.id IN (SELECT sp2.parameter_id FROM site_parameters sp2 \
                             WHERE sp2.site_id = $1 AND COALESCE(sp2.is_active, true) = true) \
             ORDER BY p.code, sp.id",
            [site.id.into()],
        ))
        .await?;
    let mut expected_parameters = Vec::with_capacity(expected_rows.len());
    for r in &expected_rows {
        expected_parameters.push(ExpectedParameter {
            parameter_id: r.try_get("", "id")?,
            code: r.try_get("", "code")?,
            name: r.try_get("", "name")?,
            units: r.try_get("", "units")?,
            decimal_places: r.try_get("", "decimal_places")?,
        });
    }

    let mut page_binds = binds;
    let limit = limit_clause(paging, &mut page_binds);
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT ce.id, ce.collected_at, ce.source, ce.created_by, ce.notes, \
                        {VISIT_COUNT_COLUMNS} \
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
            recompute: String::new(),
            cells: Vec::new(),
        })
        .collect();
    let event_ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    let recompute = super::recompute::status_for(&state.db, &event_ids).await?;
    for visit in &mut visits {
        visit.recompute = recompute
            .get(&visit.id)
            .cloned()
            .unwrap_or_else(|| "current".to_string());
    }

    // One pass over the page's events fills the grid cells: served value per (event, parameter)
    // plus the all-flagged/all-withdrawn state, then the open finding kinds.
    if !event_ids.is_empty() {
        let cell_rows = state
            .db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT r.collection_event_id AS event_id, r.parameter_id,
                        COALESCE(MAX(s.mean),
                                 (ARRAY_AGG(COALESCE(r.calibrated_value, r.raw_value)
                                            ORDER BY r.replicate_index)
                                  FILTER (WHERE r.is_flagged IS NOT TRUE
                                            AND r.withdrawn_at IS NULL))[1]) AS value,
                        BOOL_AND(r.is_flagged IS TRUE) AS all_flagged,
                        BOOL_AND(r.withdrawn_at IS NOT NULL) AS all_withdrawn,
                        COUNT(*)::bigint AS n_total,
                        COUNT(*) FILTER (WHERE r.is_flagged IS TRUE)::bigint AS n_flagged,
                        COUNT(*) FILTER (WHERE r.withdrawn_at IS NOT NULL)::bigint AS n_withdrawn,
                        MAX(s.n) AS sample_n,
                        MAX(s.stdev) AS stdev,
                        MAX(s.median) AS median,
                        MAX(s.min_value) AS min_value,
                        MAX(s.max_value) AS max_value,
                        MAX(s.sd_estimator) AS sd_estimator,
                        MAX(s.sd_estimator_source) AS sd_estimator_source
                 FROM readings r
                 LEFT JOIN samples s ON s.id = r.sample_id
                 WHERE r.collection_event_id = ANY($1) AND r.parameter_id IS NOT NULL
                 GROUP BY 1, 2",
                [event_ids.clone().into()],
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
                "SELECT COALESCE(h.parameter_id, sp.parameter_id) AS parameter_id, \
                        h.group_time, h.kind, h.created_at \
                 FROM replicate_audit_holds h \
                 LEFT JOIN data_streams ds ON ds.id = h.stream_id \
                 LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id \
                 JOIN collection_events ce \
                   ON ce.site_id = COALESCE(h.site_id, sp.site_id) \
                  AND ce.collected_at = h.group_time \
                 WHERE h.status = 'pending' AND ce.id = ANY($1) \
                   AND COALESCE(h.parameter_id, sp.parameter_id) IS NOT NULL \
                 ORDER BY h.created_at",
                [event_ids.into()],
            ))
            .await?;
        // Oldest wins, and the rest are counted: a cell carrying two open findings says so
        // rather than picking one silently.
        let mut findings: std::collections::HashMap<(DateTime<Utc>, Uuid), (String, i64)> =
            std::collections::HashMap::new();
        for f in &finding_rows {
            let at = f
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "group_time")?
                .with_timezone(&Utc);
            let parameter_id: Uuid = f.try_get("", "parameter_id")?;
            let kind: String = f.try_get("", "kind")?;
            findings
                .entry((at, parameter_id))
                .and_modify(|(_, n)| *n += 1)
                .or_insert((kind, 1));
        }
        let mut by_event: std::collections::HashMap<Uuid, Vec<VisitCell>> =
            std::collections::HashMap::new();
        for c in &cell_rows {
            let event_id: Uuid = c.try_get("", "event_id")?;
            by_event.entry(event_id).or_default().push(VisitCell {
                parameter_id: c.try_get("", "parameter_id")?,
                value: c.try_get("", "value")?,
                flagged: c
                    .try_get::<Option<bool>>("", "all_flagged")?
                    .unwrap_or(false),
                withdrawn: c
                    .try_get::<Option<bool>>("", "all_withdrawn")?
                    .unwrap_or(false),
                n_total: c.try_get("", "n_total")?,
                n_flagged: c.try_get("", "n_flagged")?,
                n_withdrawn: c.try_get("", "n_withdrawn")?,
                n: c.try_get("", "sample_n")?,
                stdev: c.try_get("", "stdev")?,
                median: c.try_get("", "median")?,
                min: c.try_get("", "min_value")?,
                max: c.try_get("", "max_value")?,
                sd_estimator: c.try_get("", "sd_estimator")?,
                sd_estimator_source: c.try_get("", "sd_estimator_source")?,
                finding: None,
                finding_count: None,
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
                        n: None,
                        stdev: None,
                        median: None,
                        min: None,
                        max: None,
                        sd_estimator: None,
                        sd_estimator_source: None,
                        finding: Some(kind.clone()),
                        finding_count: (*n > 1).then_some(*n),
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

#[derive(Debug, Deserialize, IntoParams)]
pub struct VisitListQuery {
    /// Confine to one site.
    #[serde(default)]
    pub site_id: Option<Uuid>,
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// 1-based page, default 1.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, default 100, max 200.
    #[serde(default)]
    pub page_size: Option<u64>,
    /// `collected_at` (default), `parameters_filled`, `findings_open` or `site_name`.
    #[serde(default)]
    pub sort: Option<String>,
    /// `asc` or `desc` (default).
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitListRow {
    pub id: Uuid,
    pub site_id: Uuid,
    pub site_name: String,
    pub collected_at: DateTime<Utc>,
    /// 'manual' | 'portal_sync'.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Parameters with at least one non-withdrawn reading at this visit.
    pub parameters_filled: i64,
    /// Open findings at this visit.
    pub findings_open: i64,
    /// The visit's recompute state: `current` | `queued` | `running` | `failed` | `stale`.
    pub recompute: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitListResponse {
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
    pub visits: Vec<VisitListRow>,
}

/// The `ORDER BY` a sort name resolves to; the secondary key keeps ties stable.
fn visit_list_order(sort: Option<&str>, order: Option<&str>) -> AppResult<String> {
    let direction = match order.unwrap_or("desc") {
        "asc" => "ASC",
        "desc" => "DESC",
        other => {
            return Err(AppError::BadRequest(format!(
                "order must be asc or desc, not {other}"
            )));
        }
    };
    let column = match sort.unwrap_or("collected_at") {
        "collected_at" => "ce.collected_at",
        "parameters_filled" => "filled",
        "findings_open" => "findings_open",
        "site_name" => "s.name",
        other => {
            return Err(AppError::BadRequest(format!(
                "sort must be collected_at, parameters_filled, findings_open or site_name, \
                 not {other}"
            )));
        }
    };
    Ok(format!("{column} {direction}, ce.collected_at DESC, ce.id"))
}

/// List visits across sites with per-visit fill and finding counts. Requires `read_data`; a
/// project-scoped caller sees the visits of its projects' sites.
#[utoipa::path(
    get,
    path = "/api/visits",
    params(VisitListQuery),
    responses(
        (status = 200, description = "Visits", body = VisitListResponse),
        (status = 400, description = "Unknown sort or order"),
    ),
    tag = "collection_events"
)]
pub async fn list_visits(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<VisitListQuery>,
) -> AppResult<Json<VisitListResponse>> {
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
        .map(|r| r.try_get("", "n").unwrap_or(0))
        .unwrap_or(0);

    let limit = limit_clause(paging, &mut binds);
    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT ce.id, ce.site_id, s.name AS site_name, ce.collected_at, ce.source, \
                        ce.created_by, ce.notes, {VISIT_COUNT_COLUMNS} \
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
            recompute: String::new(),
        })
        .collect();
    let event_ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    let recompute = super::recompute::status_for(&state.db, &event_ids).await?;
    for visit in &mut visits {
        visit.recompute = recompute
            .get(&visit.id)
            .cloned()
            .unwrap_or_else(|| "current".to_string());
    }

    let total = u64::try_from(total).unwrap_or(0);
    Ok(Json(VisitListResponse {
        total,
        page: paging.map_or(1, Window::page),
        page_size: paging.map_or(total, |w| w.limit),
        visits,
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventDetailResponse {
    pub id: Uuid,
    pub site_id: Uuid,
    pub collected_at: DateTime<Utc>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The visit's recompute state: `current` | `queued` | `running` | `failed` | `stale`.
    pub recompute: String,
    pub cells: Vec<EventCell>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventCell {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    pub parameter_name: String,
    pub stream_id: Uuid,
    /// Which feed these replicates came in on. Two streams can serve one slot at one instant, and
    /// then the grid shows two rows under one parameter name with nothing distinguishing them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    /// How these readings reached the store: `manual`, `csv`, `api` or `sync`. A measurement
    /// without a tool-run blob is not therefore hand-entered; it may be an import or a batch, and
    /// the two are different answers to "did a person type this".
    pub origin: String,
    /// A server-built tool-run blob is stored on the measurement.
    pub has_provenance: bool,
    /// Where the value came from, as the row records it: `tool_run` | `chain` | `csv_import` |
    /// `manual` | `batch` | `sync` | `derived` | `migration`. Narrower than `origin`, which reads
    /// the stream alone and cannot tell a hand entry from a tool save on the same channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_kind: Option<String>,
    /// The blob's tool name, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The value serving arm reports: sample mean, else the lowest unflagged live replicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<CellSample>,
    pub replicates: Vec<CellReplicate>,
    /// The instant's assembled record for this stream, the same shape `/readings/provenance`
    /// serves, so the point record opened from the grid needs no second fetch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<crate::routes::private::readings::provenance::ProvenanceRecord>,
    /// The oldest open event-audit finding for this cell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<CellFinding>,
    /// The calculations that read this parameter, by tool name. A person typing into a field needs
    /// to see which script it feeds while typing it, not after the save.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_by: Vec<String>,
    /// The calculation that writes this parameter, when one does. Its value is a computed output,
    /// not a measurement, and editing it is a different act from editing an input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_by: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellSample {
    pub sample_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    /// The sd under the divisor the slot declares. `sd_estimator` names which that is; the other
    /// travels beside it so a reviewer can read both without declaring anything first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev_sample: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev_population: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    pub n: i32,
    /// 'sample' | 'population', and what chose it ('default' is the fallback having applied).
    pub sd_estimator: String,
    pub sd_estimator_source: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellReplicate {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibrated_value: Option<f64>,
    pub flagged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag_reason: Option<String>,
    pub withdrawn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawn_at: Option<DateTime<Utc>>,
    /// The base calibration this replicate was corrected with, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibration_id: Option<Uuid>,
    /// The standard curve applied on top of the base calibration, null when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub standard_curve_id: Option<Uuid>,
    /// The instrument the replicate names. The grid offers it back as the row's declaration, so
    /// re-entering a value does not silently re-attribute it to whatever the slot declares now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_id: Option<Uuid>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellFinding {
    pub id: Uuid,
    /// `missing_output`, `stale_output` or `skipped_output`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub status: String,
}

/// A visit's header row as the per-site list selects it.
#[derive(Debug, FromQueryResult)]
struct SiteVisitHeader {
    id: Uuid,
    collected_at: DateTime<Utc>,
    source: String,
    created_by: Option<String>,
    notes: Option<String>,
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
    filled: i64,
    findings_open: i64,
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
    stdev_sample: Option<f64>,
    stdev_population: Option<f64>,
    sample_median: Option<f64>,
    sample_min: Option<f64>,
    sample_max: Option<f64>,
    sd_estimator: Option<String>,
    sd_estimator_source: Option<String>,
    flag_reason: Option<String>,
    withdrawn_at: Option<DateTime<Utc>>,
    calibration_id: Option<Uuid>,
    standard_curve_id: Option<Uuid>,
    sensor_id: Option<Uuid>,
    has_provenance: Option<bool>,
    provenance_kind: Option<String>,
    tool: Option<String>,
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

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT r.parameter_id, p.code, p.name, r.stream_id, \
                    ds.source_system, ds.source_key, r.replicate_index, \
                    r.raw_value, r.calibrated_value, r.is_flagged, \
                    (r.withdrawn_at IS NOT NULL) AS withdrawn, r.sample_id, \
                    s.mean AS sample_mean, s.stdev AS sample_stdev, s.n AS sample_n, \
                    s.stdev_sample, s.stdev_population, s.median AS sample_median, \
                    s.min_value AS sample_min, s.max_value AS sample_max, \
                    s.sd_estimator, s.sd_estimator_source, \
                    r.flag_reason, r.withdrawn_at, r.calibration_id, r.standard_curve_id, \
                    r.sensor_id, \
                    (r.provenance IS NOT NULL) AS has_provenance, \
                    r.provenance_kind, r.provenance ->> 'tool' AS tool \
             FROM readings r \
             JOIN parameters p ON p.id = r.parameter_id \
             LEFT JOIN data_streams ds ON ds.id = r.stream_id \
             LEFT JOIN samples s ON s.id = r.sample_id \
             WHERE r.collection_event_id = $1 \
             ORDER BY p.code, r.stream_id, r.replicate_index",
            [id.into()],
        ))
        .await?
        .iter()
        .map(|r| DetailRow::from_query_result(r, ""))
        .collect::<Result<Vec<_>, _>>()?;

    let findings = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, kind, parameter_id, tool, status FROM replicate_audit_holds \
             WHERE stream_id IS NULL AND site_id = $1 AND group_time = $2 \
               AND status = 'pending' \
             ORDER BY created_at",
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
                    stdev_sample: r.stdev_sample,
                    stdev_population: r.stdev_population,
                    median: r.sample_median,
                    min: r.sample_min,
                    max: r.sample_max,
                    n: r.sample_n.unwrap_or(0),
                    sd_estimator: r.sd_estimator.unwrap_or_default(),
                    sd_estimator_source: r.sd_estimator_source.unwrap_or_default(),
                });
                cells.push(EventCell {
                    parameter_id: r.parameter_id,
                    origin: crate::routes::private::readings::provenance::classify_source(
                        r.source_system.as_deref().unwrap_or(""),
                    )
                    .to_string(),
                    has_provenance: r.has_provenance.unwrap_or(false),
                    provenance_kind: r.provenance_kind,
                    tool: r.tool,
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
    let finding_catalog = crate::routes::private::sites::parameters::descriptor::catalog_map(
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
            origin: crate::routes::private::readings::provenance::classify_source("").to_string(),
            has_provenance: false,
            provenance_kind: None,
            tool: None,
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
    let mut records = crate::routes::private::readings::provenance::records_for_event(
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
        crate::routes::private::tools::closure::calculations_fed_by(&state.db, &touched).await?;
    for cell in &mut cells {
        cell.read_by = impacts
            .iter()
            .filter(|i| i.reads.iter().any(|r| r.parameter_id == cell.parameter_id))
            .map(|i| i.tool.clone())
            .collect();
        cell.written_by = impacts
            .iter()
            .find(|i| {
                i.outputs
                    .iter()
                    .any(|o| o.parameter_id == cell.parameter_id)
            })
            .map(|i| i.tool.clone());
    }

    let recompute = super::recompute::status_for(&state.db, &[event.id])
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
        recompute,
        cells,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paging_is_opt_in() {
        assert!(paging(None, None).is_none());
        assert_eq!(paging(Some(3), None).unwrap().limit, MAX_PAGE_SIZE);
        assert_eq!(paging(None, Some(10)).unwrap().limit, 10);
        assert_eq!(paging(Some(0), Some(500)).unwrap().page(), 1);
        assert_eq!(paging(Some(0), Some(500)).unwrap().limit, MAX_PAGE_SIZE);
    }

    #[test]
    fn test_visit_list_order_defaults_and_refuses_unknown() {
        assert_eq!(
            visit_list_order(None, None).unwrap(),
            "ce.collected_at DESC, ce.collected_at DESC, ce.id"
        );
        assert_eq!(
            visit_list_order(Some("findings_open"), Some("asc")).unwrap(),
            "findings_open ASC, ce.collected_at DESC, ce.id"
        );
        assert!(visit_list_order(Some("notes"), None).is_err());
        assert!(visit_list_order(None, Some("random")).is_err());
    }

    #[test]
    fn test_csv_field_quotes_only_what_needs_it() {
        assert_eq!(csv_field("DOC_avg_ppb"), "DOC_avg_ppb");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn test_visits_filename_is_site_and_range() {
        let q = VisitsQuery {
            start: Some("2021-01-01T00:00:00Z".parse().unwrap()),
            end: Some("2021-12-31T00:00:00Z".parse().unwrap()),
            page: None,
            page_size: None,
            format: None,
        };
        assert_eq!(
            visits_filename("Les Dailles", &q, &[]),
            "Les_Dailles_visits_2021-01-01_2021-12-31.csv"
        );
    }
}
