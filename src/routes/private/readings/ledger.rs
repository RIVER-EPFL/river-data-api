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
use sea_orm::{ConnectionTrait, Statement};
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, kind, actor, at, reason, old, new FROM reading_decisions \
              WHERE stream_id = ANY($1) AND time = $2 ORDER BY at DESC",
            [streams.to_vec().into(), time.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let kind: String = r.try_get("", "kind")?;
            let reason: Option<String> = r.try_get("", "reason")?;
            Ok(LedgerEntry {
                at: r.try_get("", "at")?,
                source: "decision".to_string(),
                severity: Severity::Info.as_str().to_string(),
                actor: r.try_get("", "actor")?,
                what: match reason {
                    Some(why) if !why.trim().is_empty() => format!("{kind}: {why}"),
                    _ => kind,
                },
                old: r.try_get("", "old")?,
                new: r.try_get("", "new")?,
                id: r.try_get("", "id")?,
            })
        })
        .collect()
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, at, submitted, new_rows, changed, unchanged, withdrawn, rejected_total, \
                    braked \
               FROM ingest_receipts \
              WHERE stream_id = ANY($1) AND window_from <= $2 AND window_to >= $2 \
              ORDER BY at DESC",
            [streams.to_vec().into(), time.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let rejected: i32 = r.try_get("", "rejected_total")?;
            let braked: bool = r.try_get("", "braked")?;
            let changed: i32 = r.try_get("", "changed")?;
            let new_rows: i32 = r.try_get("", "new_rows")?;
            Ok(LedgerEntry {
                at: r.try_get("", "at")?,
                source: "ingest".to_string(),
                severity: Severity::ingest_receipt(i64::from(rejected), braked)
                    .as_str()
                    .to_string(),
                actor: None,
                what: if braked {
                    "windowed ingest, braked".to_string()
                } else {
                    format!("windowed ingest: {new_rows} new, {changed} changed")
                },
                old: None,
                new: Some(serde_json::json!({
                    "submitted": r.try_get::<i32>("", "submitted")?,
                    "new": new_rows,
                    "changed": changed,
                    "unchanged": r.try_get::<i32>("", "unchanged")?,
                    "withdrawn": r.try_get::<i32>("", "withdrawn")?,
                    "rejected": rejected,
                    "braked": braked,
                })),
                id: r.try_get("", "id")?,
            })
        })
        .collect()
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT id, kind, status, created_at, tool FROM replicate_audit_holds \
                  WHERE {predicate} ORDER BY created_at DESC"
            ),
            binds,
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let kind: String = r.try_get("", "kind")?;
            let status: String = r.try_get("", "status")?;
            Ok(LedgerEntry {
                at: r.try_get("", "created_at")?,
                source: "hold".to_string(),
                severity: Severity::audit_hold(&status).as_str().to_string(),
                actor: None,
                what: format!("{kind} ({status})"),
                old: None,
                new: r.try_get::<Option<String>>("", "tool")?.map(Into::into),
                id: r.try_get("", "id")?,
            })
        })
        .collect()
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, tool_name, source, created_by, created_at, tool_version \
               FROM tool_runs \
              WHERE id = ANY($1) OR context->>'collection_event_id' = ANY($2) \
              ORDER BY created_at DESC",
            [runs.to_vec().into(), event_keys.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let tool: String = r.try_get("", "tool_name")?;
            let source: String = r.try_get("", "source")?;
            Ok(LedgerEntry {
                at: r.try_get("", "created_at")?,
                source: "tool_run".to_string(),
                severity: Severity::Info.as_str().to_string(),
                actor: r.try_get("", "created_by")?,
                what: format!("{tool} ({source})"),
                old: None,
                new: r.try_get("", "tool_version")?,
                id: r.try_get("", "id")?,
            })
        })
        .collect()
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
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
        .await?;
    rows.iter()
        .map(|r| {
            let trigger: String = r.try_get("", "trigger_type")?;
            let status: String = r.try_get("", "status")?;
            let error: Option<String> = r.try_get("", "error_message")?;
            Ok(LedgerEntry {
                at: r
                    .try_get::<Option<DateTime<Utc>>>("", "completed_at")?
                    .unwrap_or(r.try_get("", "created_at")?),
                source: "job".to_string(),
                severity: Severity::job(&status).as_str().to_string(),
                actor: None,
                what: match &error {
                    Some(message) if status == "failed" => format!("{trigger} failed: {message}"),
                    _ => format!("{trigger} {status}"),
                },
                old: None,
                new: Some(serde_json::json!({
                    "status": status,
                    "readings_updated": r.try_get::<Option<i32>>("", "readings_updated")?,
                    "error_message": error,
                })),
                id: r.try_get("", "id")?,
            })
        })
        .collect()
}

/// What those jobs said while they ran, minus the routine. A cascade step that was skipped is
/// recorded here and nowhere else, which is the half of the story the job row cannot tell.
async fn job_logs<C: ConnectionTrait>(conn: &C, jobs: &[Uuid]) -> AppResult<Vec<LedgerEntry>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT job_id, level, message, ts FROM reprocessing_job_logs \
              WHERE job_id = ANY($1) AND level <> 'info' ORDER BY ts DESC",
            [jobs.to_vec().into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let level: String = r.try_get("", "level")?;
            Ok(LedgerEntry {
                at: r.try_get("", "ts")?,
                source: "job_log".to_string(),
                severity: Severity::job_log(&level).as_str().to_string(),
                actor: None,
                what: r.try_get("", "message")?,
                old: None,
                new: None,
                // A timeline entry is keyed by (job_id, seq) and has no id of its own; the job is
                // where a reader opens it.
                id: r.try_get("", "job_id")?,
            })
        })
        .collect()
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT c.id, c.subject, c.change, c.old_value, c.new_value, c.changed_by, \
                    c.changed_at \
               FROM change_audit c \
              WHERE c.subject = 'parameter:' || $2::text \
                 OR c.subject IN (SELECT 'site_parameter:' || sp.id::text FROM site_parameters sp \
                                   WHERE sp.site_id = $1 AND sp.parameter_id = $2) \
              ORDER BY c.changed_at DESC",
            [site_id.into(), parameter_id.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(LedgerEntry {
                at: r.try_get("", "changed_at")?,
                source: "change".to_string(),
                severity: Severity::Info.as_str().to_string(),
                actor: r.try_get("", "changed_by")?,
                what: r.try_get("", "change")?,
                old: r.try_get("", "old_value")?,
                new: r.try_get("", "new_value")?,
                id: r.try_get("", "id")?,
            })
        })
        .collect()
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
    let rows = conn
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, severity, max_severity, started_at, last_seen_at, resolved_at, \
                    acknowledged_by, measurement_type \
               FROM alarm_events \
              WHERE site_id = $1 AND parameter_id = $2 \
                AND started_at <= $3 AND COALESCE(resolved_at, last_seen_at) >= $3",
            [site_id.into(), parameter_id.into(), time.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let max: i16 = r.try_get("", "max_severity")?;
            let resolved: Option<DateTime<Utc>> = r.try_get("", "resolved_at")?;
            let cadence: String = r.try_get("", "measurement_type")?;
            Ok(LedgerEntry {
                at: r.try_get("", "started_at")?,
                source: "alarm".to_string(),
                severity: Severity::alarm(max, resolved.is_some()).as_str().to_string(),
                actor: r.try_get("", "acknowledged_by")?,
                what: match resolved {
                    Some(_) => format!("{cadence} alarm, resolved"),
                    None => format!("{cadence} alarm, open"),
                },
                old: None,
                new: Some(serde_json::json!({
                    "severity": r.try_get::<i16>("", "severity")?,
                    "max_severity": max,
                    "resolved_at": resolved,
                })),
                id: r.try_get("", "id")?,
            })
        })
        .collect()
}
