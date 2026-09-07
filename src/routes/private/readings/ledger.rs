//! One ledger of what happened to a value, read from the records that already hold it.
//!
//! Nine tables each keep part of a value's history and every one of them is already indexed for
//! the lookup, so the ledger is a read rather than a table: a second copy of nine records is a
//! second thing that can disagree with them. Each arm answers in the same row shape, and
//! [`crate::common::severity`] is what makes "show me the failures" a filter instead of a text
//! match.

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::severity::Severity;
use crate::error::{AppError, AppResult};
use crate::routes::private::readings::provenance::{ProvenanceQuery, rows_at, run_id_of};

/// The default and the ceiling on how much history one read returns. A value with a thousand
/// entries is a value with a story to tell in pages, not in one response.
const DEFAULT_LIMIT: u64 = 200;
const MAX_LIMIT: u64 = 1000;

#[derive(Debug, Deserialize, IntoParams)]
pub struct LedgerQuery {
    /// The instant (exact reading timestamp).
    pub time: DateTime<Utc>,
    /// Key form 1: the stream serving the point.
    pub stream_id: Option<Uuid>,
    /// Key form 2: the site half of the slot (with `parameter_id`).
    pub site_id: Option<Uuid>,
    /// Key form 2: the parameter half of the slot (with `site_id`).
    pub parameter_id: Option<Uuid>,
    /// Narrow key form 2 to one cadence ('continuous' matches rows stored as NULL).
    pub measurement_type: Option<String>,
    /// Keep only entries of this severity: `error`, `warning` or `info`.
    pub severity: Option<String>,
    /// How many entries to return, newest first (default 200, max 1000).
    pub limit: Option<u64>,
}

/// One thing that happened, in the one shape every source answers in.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LedgerEntry {
    pub at: DateTime<Utc>,
    /// Which record this came from: `decision`, `ingest`, `hold`, `tool_run`, `job`, `job_log`,
    /// `change` or `alarm`.
    pub source: String,
    /// `error`, `warning` or `info`, derived per source shape at the read.
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// What happened, in the source's own vocabulary.
    pub what: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<serde_json::Value>,
    /// The row this entry is, so a reader can open it where it lives.
    pub id: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LedgerResponse {
    pub time: DateTime<Utc>,
    pub site_id: Option<Uuid>,
    pub parameter_id: Option<Uuid>,
    /// Entries newest first. `truncated` says the window held more than `limit`.
    pub entries: Vec<LedgerEntry>,
    pub truncated: bool,
}

/// Everything that happened to one measured instant, in time order: the decisions taken on it, the
/// ingest passes that carried it, the review holds it raised, the tool run that computed it, the
/// jobs that rewrote it and what they skipped, the slot edits that changed how it is served, and
/// the alarms it raised. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/ledger",
    params(LedgerQuery),
    responses(
        (status = 200, description = "The value's history", body = LedgerResponse),
        (status = 400, description = "Neither key form provided"),
        (status = 404, description = "No reading at that instant"),
    ),
    tag = "readings"
)]
pub async fn get_reading_ledger(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<LedgerQuery>,
) -> AppResult<Json<LedgerResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let wanted = match q.severity.as_deref() {
        None => None,
        Some(s @ ("error" | "warning" | "info")) => Some(s.to_string()),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "severity is error, warning or info, got '{other}'"
            )));
        }
    };
    let key = ProvenanceQuery {
        time: q.time,
        stream_id: q.stream_id,
        site_id: q.site_id,
        parameter_id: q.parameter_id,
        measurement_type: q.measurement_type.clone(),
    };
    let rows = rows_at(&state.db, &key, &scope).await?;

    let streams: Vec<Uuid> = dedup(rows.iter().map(|r| r.stream_id));
    let site_id = rows.iter().find_map(|r| r.site_id).or(q.site_id);
    let parameter_id = rows.iter().find_map(|r| r.parameter_id).or(q.parameter_id);
    let events: Vec<Uuid> = dedup(rows.iter().filter_map(|r| r.collection_event_id));
    let runs: Vec<Uuid> = dedup(rows.iter().filter_map(|r| run_id_of(r.provenance.as_ref())));

    let mut entries = Vec::new();
    entries.extend(decisions(&state.db, &streams, q.time).await?);
    entries.extend(ingest_passes(&state.db, &streams, q.time).await?);
    entries.extend(holds(&state.db, &streams, site_id, parameter_id).await?);
    entries.extend(tool_runs(&state.db, &runs, &events).await?);
    let jobs = job_entries(&state.db, site_id, parameter_id, &events).await?;
    let job_ids: Vec<Uuid> = jobs.iter().map(|e| e.id).collect();
    entries.extend(jobs);
    entries.extend(job_logs(&state.db, &job_ids).await?);
    entries.extend(slot_changes(&state.db, site_id, parameter_id).await?);
    entries.extend(alarms(&state.db, site_id, parameter_id, q.time).await?);

    if let Some(sev) = &wanted {
        entries.retain(|e| &e.severity == sev);
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.at));
    let truncated = entries.len() as u64 > limit;
    entries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));

    Ok(Json(LedgerResponse {
        time: q.time,
        site_id,
        parameter_id,
        entries,
        truncated,
    }))
}

fn dedup(ids: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = ids.collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Every curation decision on the instant. A decision is a person's act, so it is never a failure.
async fn decisions<C: ConnectionTrait>(
    conn: &C,
    streams: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<Vec<LedgerEntry>> {
    if streams.is_empty() {
        return Ok(Vec::new());
    }
    let rows = DecisionRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, kind, actor, at, reason, old, new FROM reading_decisions \
          WHERE stream_id = ANY($1) AND time = $2 ORDER BY at DESC",
        [streams.to_vec().into(), time.into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.at,
            source: "decision".to_string(),
            severity: Severity::Info.as_str().to_string(),
            actor: r.actor,
            what: match r.reason {
                Some(why) if !why.trim().is_empty() => format!("{}: {why}", r.kind),
                _ => r.kind,
            },
            old: r.old,
            new: r.new,
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct DecisionRow {
    id: Uuid,
    kind: String,
    actor: Option<String>,
    at: DateTime<Utc>,
    reason: Option<String>,
    old: Option<serde_json::Value>,
    new: Option<serde_json::Value>,
}

/// Every windowed ingest pass whose claimed window covers the instant, not only the latest: the
/// question the ledger answers is what carried this value, over all the passes that did.
async fn ingest_passes<C: ConnectionTrait>(
    conn: &C,
    streams: &[Uuid],
    time: DateTime<Utc>,
) -> AppResult<Vec<LedgerEntry>> {
    if streams.is_empty() {
        return Ok(Vec::new());
    }
    let rows = ReceiptRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, at, submitted, new_rows, changed, unchanged, withdrawn, rejected_total, \
                braked \
           FROM ingest_receipts \
          WHERE stream_id = ANY($1) AND window_from <= $2 AND window_to >= $2 \
          ORDER BY at DESC",
        [streams.to_vec().into(), time.into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.at,
            source: "ingest".to_string(),
            severity: Severity::ingest_receipt(i64::from(r.rejected_total), r.braked)
                .as_str()
                .to_string(),
            actor: None,
            what: if r.braked {
                "windowed ingest, braked".to_string()
            } else {
                format!("windowed ingest: {} new, {} changed", r.new_rows, r.changed)
            },
            old: None,
            new: Some(serde_json::json!({
                "submitted": r.submitted,
                "new": r.new_rows,
                "changed": r.changed,
                "unchanged": r.unchanged,
                "withdrawn": r.withdrawn,
                "rejected": r.rejected_total,
                "braked": r.braked,
            })),
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct ReceiptRow {
    id: Uuid,
    at: DateTime<Utc>,
    submitted: i32,
    new_rows: i32,
    changed: i32,
    unchanged: i32,
    withdrawn: i32,
    rejected_total: i32,
    braked: bool,
}

/// The review queue, by both of its key shapes: a statistics hold is keyed by stream, an event
/// finding by the slot it was found at.
async fn holds<C: ConnectionTrait>(
    conn: &C,
    streams: &[Uuid],
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        if streams.is_empty() {
            return Ok(Vec::new());
        }
        return hold_rows(
            conn,
            "stream_id = ANY($1)",
            vec![streams.to_vec().into()],
        )
        .await;
    };
    hold_rows(
        conn,
        "stream_id = ANY($1) OR (site_id = $2 AND parameter_id = $3)",
        vec![streams.to_vec().into(), site_id.into(), parameter_id.into()],
    )
    .await
}

async fn hold_rows<C: ConnectionTrait>(
    conn: &C,
    predicate: &str,
    binds: Vec<sea_orm::Value>,
) -> AppResult<Vec<LedgerEntry>> {
    let rows = HoldRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT id, kind, status, created_at, tool FROM replicate_audit_holds \
              WHERE {predicate} ORDER BY created_at DESC"
        ),
        binds,
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.created_at,
            source: "hold".to_string(),
            severity: Severity::audit_hold(&r.status).as_str().to_string(),
            actor: None,
            what: format!("{} ({})", r.kind, r.status),
            old: None,
            new: r.tool.map(Into::into),
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct HoldRow {
    id: Uuid,
    kind: String,
    status: String,
    created_at: DateTime<Utc>,
    tool: Option<String>,
}

/// The runs that produced the value, and the runs made at the visit it belongs to: a chain step
/// that wrote a neighbouring cell is part of this value's story when it read this one.
async fn tool_runs<C: ConnectionTrait>(
    conn: &C,
    runs: &[Uuid],
    events: &[Uuid],
) -> AppResult<Vec<LedgerEntry>> {
    if runs.is_empty() && events.is_empty() {
        return Ok(Vec::new());
    }
    let event_keys: Vec<String> = events.iter().map(ToString::to_string).collect();
    let rows = RunRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, tool_name, source, created_by, created_at, tool_version \
           FROM tool_runs \
          WHERE id = ANY($1) OR context->>'collection_event_id' = ANY($2) \
          ORDER BY created_at DESC",
        [runs.to_vec().into(), event_keys.into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.created_at,
            source: "tool_run".to_string(),
            severity: Severity::Info.as_str().to_string(),
            actor: Some(r.created_by),
            what: format!("{} ({})", r.tool_name, r.source),
            old: None,
            new: Some(r.tool_version),
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct RunRow {
    id: Uuid,
    tool_name: String,
    source: String,
    created_by: String,
    created_at: DateTime<Utc>,
    tool_version: serde_json::Value,
}

/// The tracked jobs that touched this slot or the visit it belongs to. A job names its subject in
/// `params`, which is what makes "what rewrote this row" reachable from the row.
async fn job_entries<C: ConnectionTrait>(
    conn: &C,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    events: &[Uuid],
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        return Ok(Vec::new());
    };
    let event_keys: Vec<String> = events.iter().map(ToString::to_string).collect();
    let rows = JobRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, trigger_type, status, error_message, created_at, completed_at, \
                readings_updated \
           FROM reprocessing_jobs \
          WHERE (params->>'site_id' = $1 AND params->>'parameter_id' = $2) \
             OR params->>'collection_event_id' = ANY($3) \
          ORDER BY created_at DESC",
        [
            site_id.to_string().into(),
            parameter_id.to_string().into(),
            event_keys.into(),
        ],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.completed_at.unwrap_or(r.created_at),
            source: "job".to_string(),
            severity: Severity::job(&r.status).as_str().to_string(),
            actor: None,
            what: match &r.error_message {
                Some(message) if r.status == "failed" => {
                    format!("{} failed: {message}", r.trigger_type)
                }
                _ => format!("{} {}", r.trigger_type, r.status),
            },
            old: None,
            new: Some(serde_json::json!({
                "status": r.status,
                "readings_updated": r.readings_updated,
                "error_message": r.error_message,
            })),
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct JobRow {
    id: Uuid,
    trigger_type: String,
    status: String,
    error_message: Option<String>,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    readings_updated: Option<i32>,
}

/// What those jobs said while they ran, minus the routine. A cascade step that was skipped is
/// recorded here and nowhere else, which is the half of the story the job row cannot tell.
async fn job_logs<C: ConnectionTrait>(conn: &C, jobs: &[Uuid]) -> AppResult<Vec<LedgerEntry>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let rows = JobLogRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT job_id, level, message, ts FROM reprocessing_job_logs \
          WHERE job_id = ANY($1) AND level <> 'info' ORDER BY ts DESC",
        [jobs.to_vec().into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.ts,
            source: "job_log".to_string(),
            severity: Severity::job_log(&r.level).as_str().to_string(),
            actor: None,
            what: r.message,
            old: None,
            new: None,
            // A timeline entry is keyed by (job_id, seq) and has no id of its own; the job is
            // where a reader opens it.
            id: r.job_id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct JobLogRow {
    job_id: Uuid,
    level: String,
    message: String,
    ts: DateTime<Utc>,
}

/// The edits to the slot and to the catalog parameter behind it: not what the value is, but what
/// changed about how it is served.
async fn slot_changes<C: ConnectionTrait>(
    conn: &C,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        return Ok(Vec::new());
    };
    let rows = ChangeRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT c.id, c.change, c.old_value, c.new_value, c.changed_by, c.changed_at \
           FROM change_audit c \
          WHERE c.subject = 'parameter:' || $2::text \
             OR c.subject IN (SELECT 'site_parameter:' || sp.id::text FROM site_parameters sp \
                               WHERE sp.site_id = $1 AND sp.parameter_id = $2) \
          ORDER BY c.changed_at DESC",
        [site_id.into(), parameter_id.into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.changed_at,
            source: "change".to_string(),
            severity: Severity::Info.as_str().to_string(),
            actor: r.changed_by,
            what: r.change,
            old: r.old_value,
            new: r.new_value,
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct ChangeRow {
    id: Uuid,
    change: String,
    old_value: Option<serde_json::Value>,
    new_value: Option<serde_json::Value>,
    changed_by: Option<String>,
    changed_at: DateTime<Utc>,
}

/// The alarm episodes this value fell inside, which is the half nothing else answers: what
/// happened because of it.
async fn alarms<C: ConnectionTrait>(
    conn: &C,
    site_id: Option<Uuid>,
    parameter_id: Option<Uuid>,
    time: DateTime<Utc>,
) -> AppResult<Vec<LedgerEntry>> {
    let (Some(site_id), Some(parameter_id)) = (site_id, parameter_id) else {
        return Ok(Vec::new());
    };
    let rows = AlarmRow::find_by_statement(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT id, severity, max_severity, started_at, resolved_at, acknowledged_by, \
                measurement_type \
           FROM alarm_events \
          WHERE site_id = $1 AND parameter_id = $2 \
            AND started_at <= $3 AND COALESCE(resolved_at, last_seen_at) >= $3",
        [site_id.into(), parameter_id.into(), time.into()],
    ))
    .all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| LedgerEntry {
            at: r.started_at,
            source: "alarm".to_string(),
            severity: Severity::alarm(r.max_severity, r.resolved_at.is_some())
                .as_str()
                .to_string(),
            actor: r.acknowledged_by,
            what: match r.resolved_at {
                Some(_) => format!("{} alarm, resolved", r.measurement_type),
                None => format!("{} alarm, open", r.measurement_type),
            },
            old: None,
            new: Some(serde_json::json!({
                "severity": r.severity,
                "max_severity": r.max_severity,
                "resolved_at": r.resolved_at,
            })),
            id: r.id,
        })
        .collect())
}

#[derive(FromQueryResult)]
struct AlarmRow {
    id: Uuid,
    severity: i16,
    max_severity: i16,
    started_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    acknowledged_by: Option<String>,
    measurement_type: String,
}
