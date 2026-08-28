//! The visits table: the portal's wide `data` view reborn. One row per collection event at a
//! site, and per event the grid of parameter cells a field date filled in.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, EntityTrait, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::error::{AppError, AppResult};
use crate::routes::resolve_site;

const DEFAULT_PAGE_SIZE: u64 = 50;
const MAX_PAGE_SIZE: u64 = 200;

#[derive(Debug, Deserialize, IntoParams)]
pub struct VisitsQuery {
    #[serde(default)]
    pub start: Option<DateTime<Utc>>,
    #[serde(default)]
    pub end: Option<DateTime<Utc>>,
    /// 1-based page, default 1.
    #[serde(default)]
    pub page: Option<u64>,
    /// Rows per page, default 50, max 200.
    #[serde(default)]
    pub page_size: Option<u64>,
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
    /// Kind of the open event-audit finding on this cell, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExpectedParameter {
    pub parameter_id: Uuid,
    pub code: String,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VisitsResponse {
    pub site_id: Uuid,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
    /// Parameters that have ever held a spot reading at this site: the grid's column set,
    /// ordered by code.
    pub expected_parameters: Vec<ExpectedParameter>,
    pub visits: Vec<VisitRow>,
}

/// List a site's visits (collection events), newest first, with per-visit fill and finding
/// counts. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/sites/{id}/visits",
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
) -> AppResult<Json<VisitsResponse>> {
    let site = resolve_site(&state.db, &site_id).await?;
    if scope.is_restricted() && !scope.allows_project_opt(site.project_id) {
        return Err(AppError::NotFound("Site not found".to_string()));
    }

    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let mut range = String::new();
    let mut binds: Vec<sea_orm::Value> = vec![site.id.into()];
    if let Some(start) = q.start {
        binds.push(start.into());
        range.push_str(&format!(" AND ce.collected_at >= ${}", binds.len()));
    }
    if let Some(end) = q.end {
        binds.push(end.into());
        range.push_str(&format!(" AND ce.collected_at <= ${}", binds.len()));
    }

    let total: i64 = state
        .db
        .query_one(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT COUNT(*) AS n FROM collection_events ce WHERE ce.site_id = $1{range}"),
            binds.clone(),
        ))
        .await?
        .map(|r| r.try_get("", "n").unwrap_or(0))
        .unwrap_or(0);

    let expected_rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT p.id, p.code, p.name FROM readings r \
             JOIN parameters p ON p.id = r.parameter_id \
             WHERE r.site_id = $1 AND r.measurement_type = 'spot' \
             ORDER BY p.code",
            [site.id.into()],
        ))
        .await?;
    let mut expected_parameters = Vec::with_capacity(expected_rows.len());
    for r in &expected_rows {
        expected_parameters.push(ExpectedParameter {
            parameter_id: r.try_get("", "id")?,
            code: r.try_get("", "code")?,
            name: r.try_get("", "name")?,
        });
    }

    let mut page_binds = binds;
    page_binds.push((page_size as i64).into());
    let limit_ref = page_binds.len();
    page_binds.push((((page - 1) * page_size) as i64).into());
    let offset_ref = page_binds.len();
    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT ce.id, ce.collected_at, ce.source, ce.created_by, ce.notes, \
                        (SELECT COUNT(DISTINCT r.parameter_id) FROM readings r \
                          WHERE r.collection_event_id = ce.id AND r.withdrawn_at IS NULL \
                            AND r.parameter_id IS NOT NULL) AS filled, \
                        (SELECT COUNT(*) FROM replicate_audit_holds h \
                          WHERE h.stream_id IS NULL AND h.site_id = ce.site_id \
                            AND h.group_time = ce.collected_at AND h.status = 'pending') \
                          AS findings_open \
                 FROM collection_events ce \
                 WHERE ce.site_id = $1{range} \
                 ORDER BY ce.collected_at DESC \
                 LIMIT ${limit_ref} OFFSET ${offset_ref}"
            ),
            page_binds,
        ))
        .await?;

    let mut visits = Vec::with_capacity(rows.len());
    for r in &rows {
        visits.push(VisitRow {
            id: r.try_get("", "id")?,
            collected_at: r
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "collected_at")?
                .with_timezone(&Utc),
            source: r.try_get("", "source")?,
            created_by: r.try_get("", "created_by")?,
            notes: r.try_get("", "notes")?,
            parameters_filled: r.try_get("", "filled")?,
            findings_open: r.try_get("", "findings_open")?,
            cells: Vec::new(),
        });
    }

    // One pass over the page's events fills the grid cells: served value per (event, parameter)
    // plus the all-flagged/all-withdrawn state, then the open finding kinds.
    let event_ids: Vec<Uuid> = visits.iter().map(|v| v.id).collect();
    if !event_ids.is_empty() {
        let cell_rows = state
            .db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT r.collection_event_id AS event_id, r.parameter_id,
                        COALESCE(MAX(s.mean),
                                 (ARRAY_AGG(COALESCE(r.calibrated_value, r.raw_value)
                                            ORDER BY r.replicate_index)
                                  FILTER (WHERE r.is_flagged IS NOT TRUE
                                            AND r.withdrawn_at IS NULL))[1]) AS value,
                        BOOL_AND(r.is_flagged IS TRUE) AS all_flagged,
                        BOOL_AND(r.withdrawn_at IS NOT NULL) AS all_withdrawn
                 FROM readings r
                 LEFT JOIN samples s ON s.id = r.sample_id
                 WHERE r.collection_event_id = ANY($1) AND r.parameter_id IS NOT NULL
                 GROUP BY 1, 2",
                [event_ids.clone().into()],
            ))
            .await?;
        let finding_rows = state
            .db
            .query_all(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT h.parameter_id, h.group_time, h.kind FROM replicate_audit_holds h \
                 JOIN collection_events ce \
                   ON ce.site_id = h.site_id AND ce.collected_at = h.group_time \
                 WHERE h.stream_id IS NULL AND h.status = 'pending' AND ce.id = ANY($1)",
                [event_ids.into()],
            ))
            .await?;
        let mut findings: std::collections::HashMap<(DateTime<Utc>, Uuid), String> =
            std::collections::HashMap::new();
        for f in &finding_rows {
            let at = f
                .try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "group_time")?
                .with_timezone(&Utc);
            let parameter_id: Uuid = f.try_get("", "parameter_id")?;
            findings
                .entry((at, parameter_id))
                .or_insert(f.try_get("", "kind")?);
        }
        let mut by_event: std::collections::HashMap<Uuid, Vec<VisitCell>> =
            std::collections::HashMap::new();
        for c in &cell_rows {
            let event_id: Uuid = c.try_get("", "event_id")?;
            by_event.entry(event_id).or_default().push(VisitCell {
                parameter_id: c.try_get("", "parameter_id")?,
                value: c.try_get("", "value")?,
                flagged: c.try_get::<Option<bool>>("", "all_flagged")?.unwrap_or(false),
                withdrawn: c
                    .try_get::<Option<bool>>("", "all_withdrawn")?
                    .unwrap_or(false),
                finding: None,
            });
        }
        for visit in &mut visits {
            let mut cells = by_event.remove(&visit.id).unwrap_or_default();
            for cell in &mut cells {
                cell.finding = findings
                    .get(&(visit.collected_at, cell.parameter_id))
                    .cloned();
            }
            // A missing-output finding names a parameter with no readings; it still gets a cell.
            for ((at, parameter_id), kind) in &findings {
                if *at == visit.collected_at
                    && !cells.iter().any(|c| c.parameter_id == *parameter_id)
                {
                    cells.push(VisitCell {
                        parameter_id: *parameter_id,
                        value: None,
                        flagged: false,
                        withdrawn: false,
                        finding: Some(kind.clone()),
                    });
                }
            }
            visit.cells = cells;
        }
    }

    Ok(Json(VisitsResponse {
        site_id: site.id,
        total: u64::try_from(total).unwrap_or(0),
        page,
        page_size,
        expected_parameters,
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
    pub cells: Vec<EventCell>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EventCell {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    pub parameter_name: String,
    pub stream_id: Uuid,
    /// The value serving arm reports: sample mean, else the lowest unflagged live replicate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<CellSample>,
    pub replicates: Vec<CellReplicate>,
    /// The oldest open event-audit finding for this cell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<CellFinding>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellSample {
    pub sample_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdev: Option<f64>,
    pub n: i32,
    /// A server-built tool-run blob is stored on the sample.
    pub has_provenance: bool,
    /// The blob's tool name, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellReplicate {
    pub replicate_index: i16,
    pub raw_value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calibrated_value: Option<f64>,
    pub flagged: bool,
    pub withdrawn: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CellFinding {
    pub id: Uuid,
    /// `missing_output` or `stale_output`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub status: String,
}

/// One visit's grid row: every parameter measured at the event with its replicates, sample
/// statistics, tool provenance presence, and any open finding — plus findings for parameters
/// the audit says are missing entirely. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/collection_events/{id}/detail",
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
            return Err(AppError::NotFound(format!("Collection event {id} not found")));
        }
    }

    let rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT r.parameter_id, p.code, p.name, r.stream_id, r.replicate_index, \
                    r.raw_value, r.calibrated_value, r.is_flagged, \
                    (r.withdrawn_at IS NOT NULL) AS withdrawn, r.sample_id, \
                    s.mean AS sample_mean, s.stdev AS sample_stdev, s.n AS sample_n, \
                    (s.provenance IS NOT NULL) AS has_provenance, \
                    s.provenance ->> 'tool' AS tool \
             FROM readings r \
             JOIN parameters p ON p.id = r.parameter_id \
             LEFT JOIN samples s ON s.id = r.sample_id \
             WHERE r.collection_event_id = $1 \
             ORDER BY p.code, r.stream_id, r.replicate_index",
            [id.into()],
        ))
        .await?;

    let findings = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, kind, parameter_id, tool, status FROM replicate_audit_holds \
             WHERE stream_id IS NULL AND site_id = $1 AND group_time = $2 \
               AND status = 'pending' \
             ORDER BY created_at",
            [event.site_id.into(), event.collected_at.into()],
        ))
        .await?;
    let mut finding_by_param: std::collections::HashMap<Uuid, CellFinding> =
        std::collections::HashMap::new();
    for f in &findings {
        let parameter_id: Uuid = f.try_get("", "parameter_id")?;
        finding_by_param
            .entry(parameter_id)
            .or_insert(CellFinding {
                id: f.try_get("", "id")?,
                kind: f.try_get("", "kind")?,
                tool: f.try_get("", "tool")?,
                status: f.try_get("", "status")?,
            });
    }

    // Fold reading rows into per-(parameter, stream) cells.
    let mut cells: Vec<EventCell> = Vec::new();
    for r in &rows {
        let parameter_id: Uuid = r.try_get("", "parameter_id")?;
        let stream_id: Uuid = r.try_get("", "stream_id")?;
        let replicate = CellReplicate {
            replicate_index: r.try_get("", "replicate_index")?,
            raw_value: r.try_get("", "raw_value")?,
            calibrated_value: r.try_get("", "calibrated_value")?,
            flagged: r.try_get::<Option<bool>>("", "is_flagged")?.unwrap_or(false),
            withdrawn: r.try_get("", "withdrawn")?,
        };
        let same_cell = cells
            .last_mut()
            .filter(|c| c.parameter_id == parameter_id && c.stream_id == stream_id);
        match same_cell {
            Some(cell) => cell.replicates.push(replicate),
            None => {
                let sample = match r.try_get::<Option<Uuid>>("", "sample_id")? {
                    Some(sample_id) => Some(CellSample {
                        sample_id,
                        mean: r.try_get("", "sample_mean")?,
                        stdev: r.try_get("", "sample_stdev")?,
                        n: r.try_get::<Option<i32>>("", "sample_n")?.unwrap_or(0),
                        has_provenance: r
                            .try_get::<Option<bool>>("", "has_provenance")?
                            .unwrap_or(false),
                        tool: r.try_get("", "tool")?,
                    }),
                    None => None,
                };
                cells.push(EventCell {
                    parameter_id,
                    parameter_code: r.try_get("", "code")?,
                    parameter_name: r.try_get("", "name")?,
                    stream_id,
                    served_value: None,
                    sample,
                    replicates: vec![replicate],
                    finding: finding_by_param.remove(&parameter_id),
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
    for (parameter_id, finding) in finding_by_param {
        let (code, name) = crate::routes::private::parameters::Entity::find_by_id(parameter_id)
            .one(&state.db)
            .await?
            .map(|p| (p.code, p.name))
            .unwrap_or_else(|| (String::new(), String::new()));
        cells.push(EventCell {
            parameter_id,
            parameter_code: code,
            parameter_name: name,
            stream_id: Uuid::nil(),
            served_value: None,
            sample: None,
            replicates: Vec::new(),
            finding: Some(finding),
        });
    }

    Ok(Json(EventDetailResponse {
        id: event.id,
        site_id: event.site_id,
        collected_at: event.collected_at,
        source: event.source,
        created_by: event.created_by,
        notes: event.notes,
        cells,
    }))
}
