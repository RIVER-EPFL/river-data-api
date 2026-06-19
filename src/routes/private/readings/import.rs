//! Wide-CSV historical ingestion — the backend for the dashboard's CSV import flow.
//!
//! Accepts the client's delivery format: a `DateTime` column plus one column per parameter
//! (e.g. `DateTime,DOmgL,DOuM,WaterTempdegC`) for a single target site. Each column is resolved to
//! a parameter via, in order: an explicit per-request `mapping`, site_parameter names,
//! site parameter aliases, then the global catalog name. Columns that resolve to a
//! derived-output parameter (e.g. `DOmgL`) are skipped — derived
//! values are recomputed from their sources, never ingested.
//!
//! `dry_run` returns the resolution plan (which columns map where, which are skipped/unmapped, plus
//! warnings, row count and date range) without writing anything, so the UI can show "here's how I'll
//! align this file" before the operator confirms.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::middleware::{ProjectScope, enforce_project_scope_for_sites};
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::services::get_or_create_api_stream;
use crate::routes::private::readings::batch::ConflictMode;
use crate::routes::private::sensors::operations::{ResolvedOwner, resolve_slot_owner_for_times};
use crate::routes::resolve_site_with_project;

#[derive(Debug, Deserialize, ToSchema)]
pub struct ImportCsvRequest {
    /// Target site, by UUID or case-insensitive name.
    pub site: String,
    /// Wide CSV text: a `DateTime` column plus one column per parameter.
    /// Optional when `session_id` references a previously uploaded CSV.
    #[serde(default)]
    pub csv: Option<String>,
    /// Reference to a staged CSV from a prior dry_run. When present without `csv`, the server
    /// retrieves the cached CSV text instead of requiring a re-upload.
    #[serde(default)]
    pub session_id: Option<Uuid>,
    /// Optional explicit column → parameter mapping. The value is a parameter name or UUID;
    /// `null` skips the column. Overrides automatic resolution.
    #[serde(default)]
    pub mapping: Option<HashMap<String, Option<String>>>,
    /// When true, resolve and report the plan (and overlap diff) without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Behaviour on (stream_id, time, replicate_index) collisions. Defaults to `skip`.
    #[serde(default)]
    pub conflict: ConflictMode,
    /// Timezone offset (hours) of the source timestamps relative to UTC. The server subtracts
    /// this offset to convert to UTC, e.g. `2.0` for CEST (UTC+02:00).
    #[serde(default)]
    pub tz_offset_hours: Option<f64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCsvResponse {
    pub site_id: Uuid,
    pub site_name: String,
    pub dry_run: bool,
    /// Staging session ID. Returned on every request; pass it back on subsequent requests
    /// (re-analyze, import) to avoid re-uploading the CSV.
    pub session_id: Option<Uuid>,
    /// Header → resolved catalog parameter name, for columns that will be ingested.
    pub mapped_columns: HashMap<String, String>,
    /// Columns intentionally not ingested: derived outputs (recomputed) or explicitly skipped.
    pub skipped_columns: Vec<String>,
    /// Columns that could not be resolved to a parameter.
    pub unmapped_columns: Vec<String>,
    /// Non-fatal notes (e.g. a mapped parameter is not assigned to the site).
    pub warnings: Vec<String>,
    /// Data rows parsed from the CSV.
    pub row_count: usize,
    /// Readings inserted (0 for `dry_run`).
    pub inserted_total: usize,
    pub earliest: Option<chrono::DateTime<chrono::Utc>>,
    pub latest: Option<chrono::DateTime<chrono::Utc>>,
    /// Background reprocessing job recomputing derived parameters + refreshing aggregates over the
    /// imported range. Poll `GET /api/reprocessing_jobs/{id}` for progress. `null` when nothing was
    /// inserted (idempotent re-import) or on `dry_run`.
    pub derived_job_id: Option<Uuid>,
    /// Distinct timestamps queued for derived recompute by that job.
    pub derived_timestamps: usize,
    /// Readings skipped because they already existed (idempotent re-import). 0 for `dry_run`.
    pub duplicates: usize,
    /// Incoming readings whose (stream, time) already exists with the same stored value.
    pub overlaps_identical: usize,
    /// Incoming readings whose (stream, time) already exists with a different stored value.
    /// In `overwrite` mode these are the rows that would be (or were) replaced.
    pub overlaps_differing: usize,
    /// Existing rows replaced because `conflict = overwrite`. Always 0 in `skip` mode or `dry_run`.
    pub overwritten: usize,
    /// Up to 20 differing overlaps, so the UI can preview what an overwrite would change.
    pub overlap_sample: Vec<OverlapDiff>,
    /// Per-row problems (bad timestamp / non-numeric value): the offending row or cell is skipped
    /// and the rest import, so the operator can fix the source and re-import. Truncated for very
    /// large files (see `error_count`).
    pub errors: Vec<RowError>,
    /// Total number of row problems (may exceed `errors.len()` when the list is truncated).
    pub error_count: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OverlapDiff {
    pub time: chrono::DateTime<chrono::Utc>,
    /// Parameter the differing value belongs to.
    pub parameter_id: Uuid,
    /// Currently stored value.
    pub existing: f64,
    /// Value from the imported CSV.
    pub incoming: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RowError {
    /// 1-based CSV line number (the header is line 1).
    pub row: usize,
    pub message: String,
}

pub(crate) const BATCH_SIZE: usize = 1000;
/// Cap on the returned error list to keep responses bounded; `error_count` reports the true total.
const MAX_ERRORS: usize = 500;

struct ColumnMapping {
    idx: usize,
    header: String,
    parameter_id: Uuid,
    /// public_value = stored_value * factor + offset, so stored = (value - offset) / factor.
    conversion_factor: f64,
    conversion_offset: f64,
}

fn parse_datetime(s: &str, tz_offset: chrono::Duration) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(ndt.and_utc() - tz_offset);
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Some(ndt.and_utc() - tz_offset);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc) - tz_offset);
    }
    None
}

/// Import historical readings from a wide CSV for one site. Resolves columns to parameters
/// (explicit mapping > public name > alias > catalog), skips derived outputs, inserts raw values
/// idempotently, then recomputes derived parameters and refreshes aggregates. `dry_run` returns the
/// resolution plan only. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/readings/import_csv",
    request_body = ImportCsvRequest,
    responses(
        (status = 200, description = "Import summary or dry-run plan", body = ImportCsvResponse),
        (status = 400, description = "Unparseable CSV, missing DateTime column, or no resolvable parameter columns"),
        (status = 404, description = "Site not found"),
        (status = 413, description = "Body exceeds 50MB limit"),
    ),
    tag = "ingestion"
)]
pub async fn import_csv(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<ImportCsvRequest>,
) -> AppResult<Json<ImportCsvResponse>> {
    // --- Resolve CSV text: from request body or staging cache ---------------------------------
    let (csv_text, session_id) = if let Some(csv) = req.csv.as_deref() {
        let sid = Uuid::new_v4();
        let arc = Arc::new(csv.to_owned());
        state.import_staging.insert(sid.to_string(), arc.clone()).await;
        (arc, sid)
    } else if let Some(sid) = req.session_id {
        let cached = state.import_staging.get(&sid.to_string()).await.ok_or_else(|| {
            AppError::BadRequest("Staging session expired or not found — re-upload the file".into())
        })?;
        (cached, sid)
    } else {
        return Err(AppError::BadRequest("Provide either csv or session_id".into()));
    };

    let tz_offset = chrono::Duration::milliseconds(
        (req.tz_offset_hours.unwrap_or(0.0) * 3_600_000.0) as i64,
    );

    let (site, _project) = resolve_site_with_project(&state.db, &req.site).await?;
    let site_id = site.id;

    // A project-scoped token may only import into a site within its project.
    enforce_project_scope_for_sites(&state.db, scope, &[site_id]).await?;

    // --- Resolution tables (site_parameter-first) ---------------------------------------------

    // Site parameters for this site: lower(sp.name) -> (parameter_id, sp_name).
    // Also build alias map from the site's parameters only.
    let sp_rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.name AS sp_name, sp.parameter_id, p.code AS param_name, p.aliases \
             FROM site_parameters sp JOIN parameters p ON p.id = sp.parameter_id \
             WHERE sp.site_id = $1",
            [site_id.into()],
        ))
        .await?;

    let mut site_param_map: HashMap<String, (Uuid, String)> = HashMap::new();
    let mut site_alias_map: HashMap<String, (Uuid, String)> = HashMap::new();
    let mut param_names: HashMap<Uuid, String> = HashMap::new();
    let mut site_param_ids: HashSet<Uuid> = HashSet::new();

    for row in &sp_rows {
        let sp_name: String = row.try_get("", "sp_name").unwrap_or_default();
        let Ok(pid) = row.try_get::<Uuid>("", "parameter_id") else { continue };
        let param_name: String = row.try_get("", "param_name").unwrap_or_default();
        let aliases: Vec<String> = row.try_get("", "aliases").unwrap_or_default();

        site_param_map.insert(sp_name.to_lowercase(), (pid, sp_name.clone()));
        site_param_map.insert(param_name.to_lowercase(), (pid, sp_name.clone()));
        param_names.insert(pid, sp_name.clone());
        site_param_ids.insert(pid);

        for alias in &aliases {
            site_alias_map.insert(alias.to_lowercase(), (pid, sp_name.clone()));
        }
    }

    // Derived-output parameters are computed, never ingested.
    let derived_rows = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT output_parameter_id FROM derived_parameter_definitions \
             WHERE output_parameter_id IS NOT NULL"
                .to_owned(),
        ))
        .await?;
    let derived_outputs: HashSet<Uuid> = derived_rows
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "output_parameter_id").ok())
        .collect();

    // Global catalog fallback: lower(name) -> id, lower(alias) -> id.
    let catalog_rows = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, code, aliases FROM parameters".to_owned(),
        ))
        .await?;
    let mut catalog: HashMap<String, Uuid> = HashMap::new();
    for row in &catalog_rows {
        let Ok(pid) = row.try_get::<Uuid>("", "id") else { continue };
        let name: String = row.try_get("", "code").unwrap_or_default();
        let aliases: Vec<String> = row.try_get("", "aliases").unwrap_or_default();
        catalog.insert(name.to_lowercase(), pid);
        for alias in &aliases {
            catalog.insert(alias.to_lowercase(), pid);
        }
        param_names.entry(pid).or_insert(name);
    }

    // Resolve an explicit-mapping target to a parameter id (site_param name, UUID, or catalog name).
    let resolve_target = |target: &str| -> Option<(Uuid, String)> {
        if let Ok(uuid) = Uuid::parse_str(target) {
            return param_names.get(&uuid).map(|n| (uuid, n.clone()));
        }
        let key = target.to_lowercase();
        site_param_map.get(&key).cloned()
            .or_else(|| site_alias_map.get(&key).cloned())
            .or_else(|| catalog.get(&key).map(|&pid| (pid, param_names.get(&pid).cloned().unwrap_or_default())))
    };

    // --- Parse header and classify columns ----------------------------------------------------
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(csv_text.as_bytes());

    let headers = reader
        .headers()
        .map_err(|e| AppError::BadRequest(format!("Failed to read CSV header: {e}")))?
        .clone();

    let datetime_idx = headers
        .iter()
        .position(|h| h.eq_ignore_ascii_case("datetime") || h.eq_ignore_ascii_case("time"))
        .unwrap_or(0);

    let mut mappings: Vec<ColumnMapping> = Vec::new();
    let mut mapped_columns: HashMap<String, String> = HashMap::new();
    let mut skipped_columns: Vec<String> = Vec::new();
    let mut unmapped_columns: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for (idx, header) in headers.iter().enumerate() {
        if idx == datetime_idx {
            continue;
        }

        // Candidate (parameter_id, display_name, factor, offset):
        // Resolution: explicit mapping > site_param name > site aliases > exposure > catalog.
        let candidate: Option<(Uuid, String, f64, f64)> =
            if let Some(entry) = req.mapping.as_ref().and_then(|m| m.get(header)) {
                match entry {
                    None => {
                        skipped_columns.push(header.to_string());
                        continue;
                    }
                    Some(target) => match resolve_target(target) {
                        Some((pid, name)) => Some((pid, name, 1.0, 0.0)),
                        None => {
                            unmapped_columns.push(header.to_string());
                            warnings.push(format!(
                                "Explicit mapping target '{target}' for column '{header}' is not a known parameter"
                            ));
                            continue;
                        }
                    },
                }
            } else {
                let key = header.to_lowercase();
                site_param_map
                    .get(&key)
                    .map(|(pid, name)| (*pid, name.clone(), 1.0, 0.0))
                    .or_else(|| site_alias_map.get(&key).map(|(pid, name)| (*pid, name.clone(), 1.0, 0.0)))
                    .or_else(|| catalog.get(&key).map(|pid| (*pid, param_names.get(pid).cloned().unwrap_or_default(), 1.0, 0.0)))
            };

        match candidate {
            Some((pid, _, _, _)) if derived_outputs.contains(&pid) => {
                skipped_columns.push(header.to_string());
            }
            Some((pid, resolved_name, factor, offset)) => {
                mapped_columns.insert(header.to_string(), resolved_name);
                if !site_param_ids.contains(&pid) {
                    let name = param_names.get(&pid).cloned().unwrap_or_default();
                    warnings.push(format!(
                        "Column '{header}' maps to parameter '{name}', which is not assigned to site '{}' — it will be stored but not exposed until you add the site parameter",
                        site.name
                    ));
                }
                mappings.push(ColumnMapping {
                    idx,
                    header: header.to_string(),
                    parameter_id: pid,
                    conversion_factor: factor,
                    conversion_offset: offset,
                });
            }
            None => unmapped_columns.push(header.to_string()),
        }
    }

    // --- Parse rows ---------------------------------------------------------------------------
    // Bad rows/cells are skipped and recorded in `errors` (non-fatal), so a partially-malformed
    // file still imports its good rows and the operator gets a list to fix and re-import.
    let mut rows: Vec<(Uuid, chrono::DateTime<chrono::Utc>, f64)> = Vec::new();
    let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut latest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut row_count = 0usize;
    let mut errors: Vec<RowError> = Vec::new();
    let mut error_count = 0usize;
    let mut line = 1usize; // header is line 1

    let record_error = |row: usize, message: String, errors: &mut Vec<RowError>, count: &mut usize| {
        *count += 1;
        if errors.len() < MAX_ERRORS {
            errors.push(RowError { row, message });
        }
    };

    for record in reader.records() {
        line += 1;
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                record_error(line, format!("CSV parse error: {e}"), &mut errors, &mut error_count);
                continue;
            }
        };
        let dt_cell = record.get(datetime_idx).unwrap_or("");
        let Some(time) = parse_datetime(dt_cell, tz_offset) else {
            record_error(line, format!("Unparseable DateTime '{dt_cell}'"), &mut errors, &mut error_count);
            continue;
        };
        row_count += 1;
        earliest = Some(earliest.map_or(time, |e| e.min(time)));
        latest = Some(latest.map_or(time, |l| l.max(time)));
        for m in &mappings {
            let cell = record.get(m.idx).unwrap_or("").trim();
            if cell.is_empty()
                || cell.eq_ignore_ascii_case("nan")
                || cell.eq_ignore_ascii_case("na")
                || cell == "-9999"
            {
                continue; // intentional missing value
            }
            let raw = match cell.parse::<f64>() {
                Ok(v) => v,
                Err(_) => {
                    record_error(
                        line,
                        format!("Column '{}': '{}' is not a number", m.header, cell),
                        &mut errors,
                        &mut error_count,
                    );
                    continue;
                }
            };
            let stored = (raw - m.conversion_offset) / m.conversion_factor;
            rows.push((m.parameter_id, time, stored));
        }
    }

    // Overlap diff: bucket incoming rows against what's already stored for this site, so the UI
    // can preview what a re-import or overwrite would touch.
    let overlap = compute_overlaps(&state.db, site_id, &rows, earliest, latest).await?;

    // Dry run: report the plan and overlap diff without writing.
    if req.dry_run {
        return Ok(Json(ImportCsvResponse {
            site_id,
            site_name: site.name,
            dry_run: true,
            session_id: Some(session_id),
            mapped_columns,
            skipped_columns,
            unmapped_columns,
            warnings,
            row_count,
            inserted_total: 0,
            earliest,
            latest,
            derived_job_id: None,
            derived_timestamps: 0,
            duplicates: 0,
            overlaps_identical: overlap.identical,
            overlaps_differing: overlap.differing,
            overwritten: 0,
            overlap_sample: overlap.sample,
            errors,
            error_count,
        }));
    }

    if mappings.is_empty() {
        return Err(AppError::BadRequest(
            "No CSV columns resolved to ingestible parameters for this site's project".to_string(),
        ));
    }

    // --- Prepare insert models (synchronous, fast) ---------------------------------------------
    let mut stream_cache: HashMap<Uuid, Uuid> = HashMap::new();
    for m in &mappings {
        if let std::collections::hash_map::Entry::Vacant(e) = stream_cache.entry(m.parameter_id) {
            let stream_id = get_or_create_api_stream(&state.db, site_id, m.parameter_id).await?;
            e.insert(stream_id);
        }
    }

    // Attribute each row to the sensor whose deployment window covers its time, so imported readings
    // that fall inside an existing deployment land attributed instead of NULL (the historical-orphan
    // source). Rows outside every deployment window resolve to all-None and need a later backdate.
    let mut owner_map: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), ResolvedOwner> = HashMap::new();
    {
        let mut times_by_param: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for (pid, t, _) in &rows {
            times_by_param.entry(*pid).or_default().push(*t);
        }
        for (pid, ts) in &times_by_param {
            let resolved = resolve_slot_owner_for_times(&state.db, site_id, *pid, ts).await?;
            for (t, owner) in resolved {
                owner_map.insert((*pid, t), owner);
            }
        }
    }

    let staged: Vec<StagedRow> = rows
        .iter()
        .map(|(parameter_id, time, value)| {
            let owner = owner_map.get(&(*parameter_id, *time)).cloned().unwrap_or_default();
            StagedRow {
                stream_id: stream_cache[parameter_id],
                site_id,
                parameter_id: *parameter_id,
                time: *time,
                raw_value: *value,
                sensor_id: owner.sensor_id,
                calibration_id: owner.calibration_id,
                deployment_id: owner.deployment_id,
            }
        })
        .collect();

    let mut distinct_ts: Vec<chrono::DateTime<chrono::Utc>> =
        rows.iter().map(|(_, t, _)| *t).collect();
    distinct_ts.sort_unstable();
    distinct_ts.dedup();
    let derived_timestamps = distinct_ts.len();

    let overlapping = overlap.identical + overlap.differing;
    let overlap_differing = overlap.differing;
    let has_work = rows.len() > overlapping || (overlap_differing > 0 && req.conflict == ConflictMode::Overwrite);

    // --- Stage the parsed readings and enqueue the worker job ---------------------------------
    // The readings are externalised to `csv_import_staging` so any replica can run the import (the
    // parsed `Vec` no longer lives only in this handler's memory). The job reads them back by token.
    let derived_job_id = if has_work {
        let import_token = Uuid::new_v4();
        stage_import_rows(&state.db, import_token, &staged).await?;

        let param_streams: Vec<serde_json::Value> = mappings
            .iter()
            .filter_map(|m| {
                stream_cache
                    .get(&m.parameter_id)
                    .map(|&sid| serde_json::json!([m.parameter_id, sid]))
            })
            .collect();

        let params = serde_json::json!({
            "import_token": import_token,
            "site_id": site_id,
            "site_name": site.name,
            "conflict": match req.conflict {
                ConflictMode::Skip => "skip",
                ConflictMode::Overwrite => "overwrite",
            },
            "since": earliest.map(|t| t.to_rfc3339()),
            "latest": latest.map(|t| t.to_rfc3339()),
            "overlapping": overlapping,
            "overlap_differing": overlap_differing,
            "param_streams": param_streams,
        });

        crate::routes::private::reprocessing_jobs::worker::enqueue(
            &state.db,
            "csv_import",
            None,
            None,
            &params,
            None,
        )
        .await?
    } else {
        None
    };

    let new_rows = rows.len().saturating_sub(overlapping);
    let (inserted_total, duplicates, overwritten) = match req.conflict {
        ConflictMode::Skip => (new_rows, overlapping, 0),
        ConflictMode::Overwrite => (new_rows, overlap.identical, overlap.differing),
    };

    Ok(Json(ImportCsvResponse {
        site_id,
        site_name: site.name,
        dry_run: false,
        session_id: Some(session_id),
        mapped_columns,
        skipped_columns,
        unmapped_columns,
        warnings,
        row_count,
        inserted_total,
        earliest,
        latest,
        derived_job_id,
        derived_timestamps,
        duplicates,
        overlaps_identical: overlap.identical,
        overlaps_differing: overlap.differing,
        overwritten,
        overlap_sample: overlap.sample,
        errors,
        error_count,
    }))
}

/// One parsed CSV reading staged for the worker job. Carries only the variable per-row fields; the
/// `csv_import` job re-applies the readings constants (replicate_index=0, logged=false,
/// measurement_type='continuous', is_flagged=false) when it reads these back.
struct StagedRow {
    stream_id: Uuid,
    site_id: Uuid,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
    raw_value: f64,
    sensor_id: Option<Uuid>,
    calibration_id: Option<Uuid>,
    deployment_id: Option<Uuid>,
}

/// Bulk-insert the parsed rows into `csv_import_staging` under `import_token`, chunked so the
/// parameter count per statement stays bounded. The worker job reads them back by token.
async fn stage_import_rows(
    db: &sea_orm::DatabaseConnection,
    import_token: Uuid,
    rows: &[StagedRow],
) -> AppResult<()> {
    for chunk in rows.chunks(BATCH_SIZE) {
        let mut sql = String::from(
            "INSERT INTO csv_import_staging \
             (import_token, stream_id, site_id, parameter_id, time, raw_value, \
              sensor_id, calibration_id, deployment_id) VALUES ",
        );
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(chunk.len() * 9);
        for (i, r) in chunk.iter().enumerate() {
            let base = i * 9;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "(${},${},${},${},${},${},${},${},${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
            ));
            values.push(import_token.into());
            values.push(r.stream_id.into());
            values.push(r.site_id.into());
            values.push(r.parameter_id.into());
            values.push(sea_orm::prelude::DateTimeWithTimeZone::from(r.time).into());
            values.push(r.raw_value.into());
            values.push(r.sensor_id.into());
            values.push(r.calibration_id.into());
            values.push(r.deployment_id.into());
        }
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            values,
        ))
        .await?;
    }
    Ok(())
}

struct OverlapReport {
    identical: usize,
    differing: usize,
    sample: Vec<OverlapDiff>,
}

/// Cap on differing-overlap rows returned for UI preview.
const OVERLAP_SAMPLE_CAP: usize = 20;
/// Tolerance for treating an incoming value as identical to the stored one.
const OVERLAP_EPSILON: f64 = 1e-9;

/// Bucket incoming `(parameter_id, time, stored_value)` rows against existing readings for the
/// site into identical vs differing overlaps. Fetches all existing readings in the time range for
/// the relevant parameters in a single query, then compares in memory.
async fn compute_overlaps(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    rows: &[(Uuid, chrono::DateTime<chrono::Utc>, f64)],
    earliest: Option<chrono::DateTime<chrono::Utc>>,
    latest: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<OverlapReport> {
    let mut identical = 0usize;
    let mut differing = 0usize;
    let mut sample = Vec::new();

    let (Some(t_min), Some(t_max)) = (earliest, latest) else {
        return Ok(OverlapReport { identical, differing, sample });
    };
    if rows.is_empty() {
        return Ok(OverlapReport { identical, differing, sample });
    }

    let param_ids: Vec<Uuid> = rows.iter().map(|(pid, _, _)| *pid).collect::<HashSet<_>>().into_iter().collect();

    let existing_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT parameter_id, time, COALESCE(calibrated_value, raw_value) AS val \
             FROM readings \
             WHERE site_id = $1 AND replicate_index = 0 \
             AND parameter_id = ANY($2) \
             AND time >= $3 AND time <= $4",
            [
                site_id.into(),
                param_ids.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(t_min).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(t_max).into(),
            ],
        ))
        .await?;

    let mut existing: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), f64> =
        HashMap::with_capacity(existing_rows.len());
    for row in &existing_rows {
        let Ok(pid) = row.try_get::<Uuid>("", "parameter_id") else { continue };
        let Ok(t) = row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "time") else { continue };
        let Ok(val) = row.try_get::<f64>("", "val") else { continue };
        existing.insert((pid, t.with_timezone(&chrono::Utc)), val);
    }

    for (pid, time, incoming) in rows {
        if let Some(&stored) = existing.get(&(*pid, *time)) {
            if (stored - incoming).abs() <= OVERLAP_EPSILON {
                identical += 1;
            } else {
                differing += 1;
                if sample.len() < OVERLAP_SAMPLE_CAP {
                    sample.push(OverlapDiff {
                        time: *time,
                        parameter_id: *pid,
                        existing: stored,
                        incoming: *incoming,
                    });
                }
            }
        }
    }

    Ok(OverlapReport { identical, differing, sample })
}
