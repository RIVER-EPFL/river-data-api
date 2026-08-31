//! Wide-CSV historical ingestion, the backend for the dashboard's CSV import flow.
//!
//! Accepts the client's delivery format: a `DateTime` column plus one column per parameter
//! (e.g. `DateTime,DOmgL,DOuM,WaterTempdegC`) for a single target site. Each column is resolved to
//! a parameter via, in order: an explicit per-request `mapping`, site_parameter names,
//! site parameter aliases, then the global catalog name. Columns that resolve to a
//! derived-output parameter (e.g. `DOmgL`) are skipped, derived
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
use crate::routes::private::data_streams::service::get_or_create_api_stream;
use crate::routes::private::readings::batch::{ConflictMode, admission};
use crate::routes::private::sensors::operations::{ResolvedOwner, resolve_slot_owner_for_times};
use crate::routes::resolve_site_with_project;

/// What the file's numbers are, deciding whether the import may claim a correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CsvValueState {
    /// Uncorrected instrument output: the deployment's calibration covering each row's time is
    /// stamped and applied, exactly as if the rows had arrived through `/ingest`.
    Raw,
    /// Already-processed numbers (a result sheet, a portal export). Stored as served: no
    /// calibration id is stamped and nothing recomputes them, because a stored calibration id
    /// claims `raw_value` is the uncorrected input, which these rows are not. The default:
    /// an uncorrected import left uncorrected is visible and repairable, a corrected import
    /// corrected again is silent corruption.
    #[default]
    Corrected,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
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
    /// measurement_type stamped on every imported reading ('continuous' | 'spot' | 'derived').
    /// Use 'spot' for lab/campaign result sheets. Omit to resolve per row from the stream's
    /// declaration, then the owning sensor's data_frequency.
    #[serde(default)]
    pub measurement_type: Option<String>,
    /// Whether the file holds raw instrument output or already-processed values. Defaults to
    /// `corrected`: no calibration is stamped or applied unless the caller declares the rows raw.
    #[serde(default)]
    pub values: CsvValueState,
    /// Import the file as tool entry (S4a): each row's columns are inputs of this tool, the tool
    /// runs over every row, and the outputs are saved through the grab write path with the same
    /// server-built provenance a typed entry gets (`source: csv_import`). Without it, columns are
    /// catalog parameters and values import as-is.
    #[serde(default)]
    pub tool: Option<String>,
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
    /// (parameter, timestamp) groups holding more than one value in a 'spot' file; each is stored
    /// as one replicate set behind a single served point.
    pub replicate_groups: usize,
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
    /// `tool_runs` rows minted by a tool-entry import (one per data row that ran). Always 0 for a
    /// plain import.
    #[serde(default)]
    pub tool_runs_created: usize,
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

/// Resolve a CSV timestamp cell to an instant. `tz_offset` is the zone the operator declared for
/// the file and applies to the naive forms only: an RFC 3339 timestamp already carries its offset
/// and is a resolved instant, so applying the declared zone to it would shift it a second time.
fn parse_datetime(s: &str, tz_offset: chrono::Duration) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(ndt.and_utc() - tz_offset);
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Some(ndt.and_utc() - tz_offset);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc));
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
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<ImportCsvRequest>,
) -> AppResult<Json<ImportCsvResponse>> {
    crate::routes::private::readings::measurement::validate_measurement_type(
        req.measurement_type.as_deref(),
    )?;

    // --- Resolve CSV text: from request body or staging cache ---------------------------------
    let (csv_text, session_id) = if let Some(csv) = req.csv.as_deref() {
        let sid = Uuid::new_v4();
        let arc = Arc::new(csv.to_owned());
        state
            .import_staging
            .insert(sid.to_string(), arc.clone())
            .await;
        (arc, sid)
    } else if let Some(sid) = req.session_id {
        let cached = state
            .import_staging
            .get(&sid.to_string())
            .await
            .ok_or_else(|| {
                AppError::BadRequest(
                    "Staging session expired or not found, re-upload the file".into(),
                )
            })?;
        (cached, sid)
    } else {
        return Err(AppError::BadRequest(
            "Provide either csv or session_id".into(),
        ));
    };

    let tz_offset =
        chrono::Duration::milliseconds((req.tz_offset_hours.unwrap_or(0.0) * 3_600_000.0) as i64);

    let (site, _project) = resolve_site_with_project(&state.db, &req.site).await?;
    let site_id = site.id;

    // A project-scoped token may only import into a site within its project.
    enforce_project_scope_for_sites(&state.db, &scope, &[site_id]).await?;

    // Tool entry: the file's columns are tool inputs, not catalog parameters. Same write path as
    // typing the rows into the tool (D15).
    if let Some(tool_name) = req.tool.as_deref() {
        let response = import_tool_csv(
            &state, &auth, &scope, &req, tool_name, &csv_text, session_id, &site, tz_offset,
        )
        .await?;
        return Ok(Json(response));
    }

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
        let Ok(pid) = row.try_get::<Uuid>("", "parameter_id") else {
            continue;
        };
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
        let Ok(pid) = row.try_get::<Uuid>("", "id") else {
            continue;
        };
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
        site_param_map
            .get(&key)
            .cloned()
            .or_else(|| site_alias_map.get(&key).cloned())
            .or_else(|| {
                catalog
                    .get(&key)
                    .map(|&pid| (pid, param_names.get(&pid).cloned().unwrap_or_default()))
            })
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
        let candidate: Option<(Uuid, String, f64, f64)> = if let Some(entry) =
            req.mapping.as_ref().and_then(|m| m.get(header))
        {
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
                .or_else(|| {
                    site_alias_map
                        .get(&key)
                        .map(|(pid, name)| (*pid, name.clone(), 1.0, 0.0))
                })
                .or_else(|| {
                    catalog.get(&key).map(|pid| {
                        (
                            *pid,
                            param_names.get(pid).cloned().unwrap_or_default(),
                            1.0,
                            0.0,
                        )
                    })
                })
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
                        "Column '{header}' maps to parameter '{name}', which is not assigned to site '{}'; it will be stored but not exposed until you add the site parameter",
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
    let mut rows: Vec<(Uuid, chrono::DateTime<chrono::Utc>, f64, usize)> = Vec::new();
    let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut latest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut row_count = 0usize;
    let mut errors: Vec<RowError> = Vec::new();
    let mut error_count = 0usize;
    let mut line = 1usize; // header is line 1

    let record_error =
        |row: usize, message: String, errors: &mut Vec<RowError>, count: &mut usize| {
            *count += 1;
            if errors.len() < MAX_ERRORS {
                errors.push(RowError { row, message });
            }
        };

    // A file that declares a non-spot cadence holds one reading per (parameter, timestamp): a
    // repeat is a source defect, and absorbing it as replicate 1 hides the row from the default
    // read and from every rollup while fabricating a grab sample around it. A file declared 'spot'
    // is a replicate plate, where the repeat is the point.
    let one_reading_per_timestamp = match req.measurement_type.as_deref() {
        Some(declared @ ("continuous" | "derived")) => Some(declared),
        _ => None,
    };
    let mut seen_slots: HashSet<(Uuid, chrono::DateTime<chrono::Utc>)> = HashSet::new();

    for record in reader.records() {
        line += 1;
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                record_error(
                    line,
                    format!("CSV parse error: {e}"),
                    &mut errors,
                    &mut error_count,
                );
                continue;
            }
        };
        let dt_cell = record.get(datetime_idx).unwrap_or("");
        let Some(time) = parse_datetime(dt_cell, tz_offset) else {
            record_error(
                line,
                format!("Unparseable DateTime '{dt_cell}'"),
                &mut errors,
                &mut error_count,
            );
            continue;
        };
        // The same bound /ingest and /readings/batch enforce, reported per row so the rest of the
        // file still imports.
        if let Some(reason) = admission::time_rejection(time) {
            record_error(line, reason, &mut errors, &mut error_count);
            continue;
        }
        row_count += 1;
        earliest = Some(earliest.map_or(time, |e| e.min(time)));
        latest = Some(latest.map_or(time, |l| l.max(time)));
        for m in &mappings {
            let stored = match admission::classify_cell(record.get(m.idx).unwrap_or("")) {
                admission::Cell::Missing => continue,
                admission::Cell::Invalid(reason) => {
                    record_error(
                        line,
                        format!("Column '{}': {reason}", m.header),
                        &mut errors,
                        &mut error_count,
                    );
                    continue;
                }
                admission::Cell::Value(raw) => (raw - m.conversion_offset) / m.conversion_factor,
            };
            if let Some(declared) = one_reading_per_timestamp
                && !seen_slots.insert((m.parameter_id, time))
            {
                record_error(
                    line,
                    format!(
                        "Column '{}': timestamp {} is repeated, and a '{declared}' series holds \
                         one reading per timestamp",
                        m.header,
                        time.to_rfc3339()
                    ),
                    &mut errors,
                    &mut error_count,
                );
                continue;
            }
            rows.push((m.parameter_id, time, stored, line));
        }
    }

    // Overlap diff: bucket incoming rows against what's already stored for this site, so the UI
    // can preview what a re-import or overwrite would touch.
    let mut overlap = compute_overlaps(&state.db, site_id, &rows, earliest, latest).await?;

    // A slot instant owned by a replicate-family stream refuses CSV rows by name: a reading's
    // replicate_index is the source's column position, so an import minting indexes from 0 onto
    // the family would fabricate replicates, and routing beside it would double-serve the
    // instant. The family's members sync from the source; corrections happen there.
    {
        let mut owning_ids: Vec<Uuid> = overlap.owning_stream.values().copied().collect();
        owning_ids.sort_unstable();
        owning_ids.dedup();
        let mut family_keys: HashMap<Uuid, String> = HashMap::new();
        if !owning_ids.is_empty() {
            for row in state
                .db
                .query_all(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "SELECT id, source_key FROM data_streams \
                     WHERE id = ANY($1) AND metadata -> 'replicates' IS NOT NULL",
                    [owning_ids.into()],
                ))
                .await?
            {
                family_keys.insert(row.try_get("", "id")?, row.try_get("", "source_key")?);
            }
        }
        if !family_keys.is_empty() {
            let header_of: HashMap<Uuid, String> = mappings
                .iter()
                .map(|m| (m.parameter_id, m.header.clone()))
                .collect();
            let before = rows.len();
            rows.retain(|(pid, time, _, line)| {
                let family = overlap
                    .owning_stream
                    .get(&(*pid, *time))
                    .and_then(|sid| family_keys.get(sid));
                let Some(key) = family else {
                    return true;
                };
                record_error(
                    *line,
                    format!(
                        "Column '{}': {} is served by replicate family stream '{key}'; its \
                         replicates sync from the source and cannot be written by CSV import",
                        header_of.get(pid).map_or("?", String::as_str),
                        time.to_rfc3339()
                    ),
                    &mut errors,
                    &mut error_count,
                );
                false
            });
            if rows.len() != before {
                overlap = compute_overlaps(&state.db, site_id, &rows, earliest, latest).await?;
            }
        }
    }
    let overlap = overlap;

    let replicate_groups = if req.measurement_type.as_deref() == Some("spot") {
        let mut group_sizes: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), usize> = HashMap::new();
        for (pid, t, _, _) in &rows {
            *group_sizes.entry((*pid, *t)).or_default() += 1;
        }
        group_sizes.values().filter(|n| **n > 1).count()
    } else {
        0
    };

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
            replicate_groups,
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
            tool_runs_created: 0,
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
    let mut owner_map: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), ResolvedOwner> =
        HashMap::new();
    {
        let mut times_by_param: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for (pid, t, _, _) in &rows {
            times_by_param.entry(*pid).or_default().push(*t);
        }
        for (pid, ts) in &times_by_param {
            let resolved = resolve_slot_owner_for_times(&state.db, site_id, *pid, ts).await?;
            for (t, owner) in resolved {
                owner_map.insert((*pid, t), owner);
            }
        }
    }

    // Write onto the stream that already holds the slot, whatever it is, so an overwrite replaces
    // the stored reading instead of adding a second one beside it on the importer's own stream.
    // Both rows would otherwise satisfy the rollup predicate and double-count the slot's bucket.
    // A slot nothing has written to yet lands on the importer's "api" stream.
    //
    // A stream that resolves for more than one of this file's parameters is not usable as a
    // target: replicates are numbered per (stream, time), so two parameters sharing a stream at
    // one timestamp would be numbered as each other's replicates.
    let mut params_per_stream: HashMap<Uuid, HashSet<Uuid>> = HashMap::new();
    for ((parameter_id, _), stream_id) in &overlap.owning_stream {
        params_per_stream
            .entry(*stream_id)
            .or_default()
            .insert(*parameter_id);
    }
    let write_target = |parameter_id: &Uuid, time: &chrono::DateTime<chrono::Utc>| -> Uuid {
        overlap
            .owning_stream
            .get(&(*parameter_id, *time))
            .filter(|stream_id| {
                params_per_stream
                    .get(*stream_id)
                    .is_some_and(|params| params.len() == 1)
            })
            .copied()
            .unwrap_or(stream_cache[parameter_id])
    };

    let staged: Vec<StagedRow> = rows
        .iter()
        .map(|(parameter_id, time, value, _)| {
            let owner = owner_map
                .get(&(*parameter_id, *time))
                .cloned()
                .unwrap_or_default();
            StagedRow {
                stream_id: write_target(parameter_id, time),
                site_id,
                parameter_id: *parameter_id,
                time: *time,
                raw_value: *value,
                // Sensor and deployment are physical facts about the slot at that time and stay
                // stamped either way; the calibration is a claim the row's value is uncorrected
                // input, which only a declared-raw import may make.
                sensor_id: owner.sensor_id,
                calibration_id: match req.values {
                    CsvValueState::Raw => owner.calibration_id,
                    CsvValueState::Corrected => None,
                },
                deployment_id: owner.deployment_id,
            }
        })
        .collect();

    let mut distinct_ts: Vec<chrono::DateTime<chrono::Utc>> =
        rows.iter().map(|(_, t, _, _)| *t).collect();
    distinct_ts.sort_unstable();
    distinct_ts.dedup();
    let derived_timestamps = distinct_ts.len();

    let overlapping = overlap.identical + overlap.differing;
    let overlap_differing = overlap.differing;
    let has_work = rows.len() > overlapping
        || (overlap_differing > 0 && req.conflict == ConflictMode::Overwrite);

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
            "measurement_type": req.measurement_type.as_deref(),
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
        replicate_groups,
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
        tool_runs_created: 0,
    }))
}

/// One parsed CSV reading staged for the worker job, holding only the per-row fields.
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
/// parameter count per statement stays bounded. `seq` records file order so the worker job can
/// number replicate groups deterministically. The worker job reads them back by token.
async fn stage_import_rows(
    db: &sea_orm::DatabaseConnection,
    import_token: Uuid,
    rows: &[StagedRow],
) -> AppResult<()> {
    let mut seq: i64 = 0;
    for chunk in rows.chunks(BATCH_SIZE) {
        let mut sql = String::from(
            "INSERT INTO csv_import_staging \
             (import_token, stream_id, site_id, parameter_id, time, raw_value, \
              sensor_id, calibration_id, deployment_id, seq) VALUES ",
        );
        let mut values: Vec<sea_orm::Value> = Vec::with_capacity(chunk.len() * 10);
        for (i, r) in chunk.iter().enumerate() {
            let base = i * 10;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "(${},${},${},${},${},${},${},${},${},${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
                base + 10,
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
            values.push(seq.into());
            seq += 1;
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
    /// Stream already holding readings for a (parameter, time), ie. the row an incoming value for
    /// that slot must be written onto. Absent when the slot is empty.
    owning_stream: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Uuid>,
}

/// Cap on differing-overlap rows returned for UI preview.
const OVERLAP_SAMPLE_CAP: usize = 20;
/// Tolerance for treating an incoming value as identical to the stored one.
const OVERLAP_EPSILON: f64 = 1e-9;

/// Bucket incoming rows against existing readings into identical and differing overlaps, and
/// report which stream already owns each occupied slot. The Nth incoming row for a
/// (parameter, time) key is compared against the Nth existing replicate.
async fn compute_overlaps(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    rows: &[(Uuid, chrono::DateTime<chrono::Utc>, f64, usize)],
    earliest: Option<chrono::DateTime<chrono::Utc>>,
    latest: Option<chrono::DateTime<chrono::Utc>>,
) -> AppResult<OverlapReport> {
    let mut identical = 0usize;
    let mut differing = 0usize;
    let mut sample = Vec::new();
    let mut owning_stream = HashMap::new();

    let (Some(t_min), Some(t_max)) = (earliest, latest) else {
        return Ok(OverlapReport {
            identical,
            differing,
            sample,
            owning_stream,
        });
    };
    if rows.is_empty() {
        return Ok(OverlapReport {
            identical,
            differing,
            sample,
            owning_stream,
        });
    }

    let param_ids: Vec<Uuid> = rows
        .iter()
        .map(|(pid, _, _, _)| *pid)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    let existing_rows = db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT parameter_id, time, stream_id, COALESCE(calibrated_value, raw_value) AS val \
             FROM readings \
             WHERE site_id = $1 \
             AND parameter_id = ANY($2) \
             AND time >= $3 AND time <= $4 \
             ORDER BY parameter_id, time, replicate_index",
            [
                site_id.into(),
                param_ids.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(t_min).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(t_max).into(),
            ],
        ))
        .await?;

    let mut existing: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), Vec<f64>> =
        HashMap::with_capacity(existing_rows.len());
    for row in &existing_rows {
        let Ok(pid) = row.try_get::<Uuid>("", "parameter_id") else {
            continue;
        };
        let Ok(t) = row.try_get::<sea_orm::prelude::DateTimeWithTimeZone>("", "time") else {
            continue;
        };
        let Ok(val) = row.try_get::<f64>("", "val") else {
            continue;
        };
        let key = (pid, t.with_timezone(&chrono::Utc));
        if let Ok(stream_id) = row.try_get::<Uuid>("", "stream_id") {
            owning_stream.entry(key).or_insert(stream_id);
        }
        existing.entry(key).or_default().push(val);
    }

    let mut occurrence: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), usize> = HashMap::new();
    for (pid, time, incoming, _) in rows {
        let key = (*pid, *time);
        let idx = occurrence.entry(key).or_insert(0);
        let slot = *idx;
        *idx += 1;
        let Some(stored) = existing.get(&key).and_then(|vs| vs.get(slot)) else {
            continue;
        };
        if (stored - incoming).abs() <= OVERLAP_EPSILON {
            identical += 1;
        } else {
            differing += 1;
            if sample.len() < OVERLAP_SAMPLE_CAP {
                sample.push(OverlapDiff {
                    time: *time,
                    parameter_id: *pid,
                    existing: *stored,
                    incoming: *incoming,
                });
            }
        }
    }

    Ok(OverlapReport {
        identical,
        differing,
        sample,
        owning_stream,
    })
}

/// Ceiling on tool-entry rows per request: each row is one runner execution plus one grab save,
/// and a campaign result sheet is tens of rows, not thousands.
const TOOL_IMPORT_ROW_CAP: usize = 500;

/// A header of the form `{name}_rep_{k}` or `{name}_{k}` (k from 1) naming position k-1 of a
/// `replicates` param.
fn replicate_column(
    params: &[crate::routes::private::tools::engine::ManifestParam],
    header: &str,
) -> Option<(String, Option<usize>)> {
    let lower = header.to_ascii_lowercase();
    params
        .iter()
        .filter(|p| p.kind == "replicates")
        .find_map(|p| {
            let rest = lower.strip_prefix(&format!("{}_", p.name.to_ascii_lowercase()))?;
            let rest = rest.strip_prefix("rep_").unwrap_or(rest);
            let k: usize = rest.parse().ok().filter(|k| *k >= 1)?;
            Some((p.name.clone(), Some(k - 1)))
        })
}

/// The tool-entry import: one tool run per data row, outputs saved through the grab write path.
///
/// Columns map to the tool's manifest params (case-insensitive, exact otherwise); the `DateTime`
/// column is the row's collection instant and travels to the engine as `collected_at`, so
/// station and event inputs resolve exactly as they would for a typed entry. Each row's run is
/// stored with `source = 'csv_import'` and its save builds the ordinary server-side blob.
#[allow(clippy::too_many_arguments)]
async fn import_tool_csv(
    state: &AppState,
    auth: &crate::common::middleware::AuthContext,
    scope: &crate::common::authz::AccessScope,
    req: &ImportCsvRequest,
    tool_name: &str,
    csv_text: &str,
    session_id: Uuid,
    site: &crate::routes::private::sites::Model,
    tz_offset: chrono::Duration,
) -> AppResult<ImportCsvResponse> {
    use crate::routes::private::readings::grab_samples::{
        GrabSampleReading, GrabSampleRequest, GrabWriteMode, insert_grab_samples,
    };
    use crate::routes::private::tools::{engine, execute_and_store_run};

    let tool = engine::find_active_tool(&state.db, tool_name).await?;

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

    let mut mapped_columns: HashMap<String, String> = HashMap::new();
    let mut unmapped_columns: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    // (column index, param name, position within a replicates param)
    let mut column_params: Vec<(usize, String, Option<usize>)> = Vec::new();
    for (idx, header) in headers.iter().enumerate() {
        if idx == datetime_idx {
            continue;
        }
        let hit = tool
            .manifest
            .params
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(header))
            .map(|p| (p.name.clone(), None))
            .or_else(|| replicate_column(&tool.manifest.params, header));
        match hit {
            Some((name, position)) => {
                mapped_columns.insert(header.to_string(), name.clone());
                column_params.push((idx, name, position));
            }
            None => {
                unmapped_columns.push(header.to_string());
                warnings.push(format!(
                    "Column '{header}' is not an input of tool '{}' and is ignored",
                    tool.name
                ));
            }
        }
    }
    if column_params.is_empty() {
        return Err(AppError::BadRequest(format!(
            "No CSV columns match inputs of tool '{}'",
            tool.name
        )));
    }

    let mut errors: Vec<RowError> = Vec::new();
    let mut error_count = 0usize;
    let record_error =
        |row: usize, message: String, errors: &mut Vec<RowError>, count: &mut usize| {
            *count += 1;
            if errors.len() < MAX_ERRORS {
                errors.push(RowError { row, message });
            }
        };

    // Parse every row up front so the plan (and dry_run) reports the whole file before anything
    // runs.
    let mut rows: Vec<(usize, chrono::DateTime<chrono::Utc>, serde_json::Map<String, serde_json::Value>)> =
        Vec::new();
    let mut earliest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut latest: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut line = 1usize;
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
            record_error(
                line,
                format!("Unparseable DateTime '{dt_cell}'"),
                &mut errors,
                &mut error_count,
            );
            continue;
        };
        if let Some(reason) = admission::time_rejection(time) {
            record_error(line, reason, &mut errors, &mut error_count);
            continue;
        }
        let mut body = serde_json::Map::new();
        let mut bad_cell = false;
        for (idx, param, position) in &column_params {
            match admission::classify_cell(record.get(*idx).unwrap_or("")) {
                admission::Cell::Missing => {}
                admission::Cell::Invalid(reason) => {
                    record_error(
                        line,
                        format!("Column '{param}': {reason}"),
                        &mut errors,
                        &mut error_count,
                    );
                    bad_cell = true;
                }
                admission::Cell::Value(v) => match position {
                    None => {
                        body.insert(param.clone(), serde_json::json!(v));
                    }
                    // A replicate column lands at its own position; a blank column before it
                    // stays a null so the replicate keeps its index.
                    Some(pos) => {
                        let list = body
                            .entry(param.clone())
                            .or_insert_with(|| serde_json::json!([]));
                        let list = list.as_array_mut().expect("replicate columns build a list");
                        while list.len() <= *pos {
                            list.push(serde_json::Value::Null);
                        }
                        list[*pos] = serde_json::json!(v);
                    }
                },
            }
        }
        if bad_cell || body.is_empty() {
            continue;
        }
        earliest = Some(earliest.map_or(time, |e| e.min(time)));
        latest = Some(latest.map_or(time, |l| l.max(time)));
        rows.push((line, time, body));
    }
    if rows.len() > TOOL_IMPORT_ROW_CAP {
        return Err(AppError::BadRequest(format!(
            "Tool-entry import takes at most {TOOL_IMPORT_ROW_CAP} rows per file; this file has {}",
            rows.len()
        )));
    }

    let catalog =
        engine::load_parameter_catalog(&state.db, std::iter::once(&tool.manifest)).await?;
    let saved_outputs: Vec<(String, Uuid)> = tool
        .manifest
        .outputs
        .iter()
        .filter_map(|o| catalog.resolve(o).map(|p| (o.key.clone(), p.id)))
        .collect();
    // The replicates a row enters are readings of their own parameter, stored raw.
    let saved_inputs: Vec<(String, Uuid)> = tool
        .manifest
        .params
        .iter()
        .filter(|p| p.kind == "replicates")
        .filter_map(|p| {
            catalog
                .resolve_code(p.parameter_code.as_deref()?)
                .map(|row| (p.name.clone(), row.id))
        })
        .collect();
    if saved_outputs.is_empty() && saved_inputs.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Nothing of tool '{}' resolves to a catalog parameter; nothing could be saved",
            tool.name
        )));
    }

    let row_count = rows.len();
    let mut inserted_total = 0usize;
    let mut tool_runs_created = 0usize;
    if !req.dry_run {
        let actor = crate::routes::private::tools::scripts::actor_label(auth);
        for (line, time, mut body) in rows {
            body.insert("site_id".into(), serde_json::json!(site.id));
            body.insert(
                "collected_at".into(),
                serde_json::json!(time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            );
            let body = serde_json::Value::Object(body);
            let body_bytes =
                serde_json::to_vec(&body).map_err(|e| AppError::Internal(e.to_string()))?;
            let result =
                match execute_and_store_run(state, &tool, &body_bytes, &actor, "csv_import").await {
                    Ok(result) => result,
                    Err(e) => {
                        record_error(line, e.to_string(), &mut errors, &mut error_count);
                        continue;
                    }
                };
            tool_runs_created += 1;

            let mut readings: Vec<GrabSampleReading> = saved_outputs
                .iter()
                .filter_map(|(key, parameter_id)| {
                    result
                        .results
                        .get(key)
                        .and_then(serde_json::Value::as_f64)
                        .map(|value| GrabSampleReading {
                            parameter_id: *parameter_id,
                            sensor_id: None,
                            value,
                            time,
                            replicate_index: None,
                            output: Some(key.clone()),
                            input: None,
                            standard_curve_id: None,
                        })
                })
                .collect();
            for (name, parameter_id) in &saved_inputs {
                let Some(values) = body.get(name).and_then(serde_json::Value::as_array) else {
                    continue;
                };
                for (position, cell) in values.iter().enumerate() {
                    let (Some(value), Ok(replicate_index)) =
                        (cell.as_f64(), i16::try_from(position))
                    else {
                        continue;
                    };
                    readings.push(GrabSampleReading {
                        parameter_id: *parameter_id,
                        sensor_id: None,
                        value,
                        time,
                        replicate_index: Some(replicate_index),
                        output: None,
                        input: Some(name.clone()),
                        standard_curve_id: None,
                    });
                }
            }
            if readings.is_empty() {
                record_error(
                    line,
                    "the run produced no savable output for this row".to_string(),
                    &mut errors,
                    &mut error_count,
                );
                continue;
            }
            let request = GrabSampleRequest {
                site_id: site.id,
                created_by: Some(actor.clone()),
                label: None,
                notes: None,
                mode: (req.conflict == ConflictMode::Overwrite).then_some(GrabWriteMode::Replace),
                dry_run: false,
                tool_run_id: Some(result.run_id),
                check_id: None,
                // The tool's manifest is read by the save path itself; nothing here overrides
                // the slot's declaration.
                sd_estimator: None,
                readings,
            };
            match insert_grab_samples(
                axum::extract::State(state.clone()),
                axum::Extension(auth.clone()),
                crate::common::middleware::ProjectScope(scope.clone()),
                axum::Json(request),
            )
            .await
            {
                Ok(axum::Json(resp)) => inserted_total += resp.inserted,
                Err(e) => record_error(line, e.to_string(), &mut errors, &mut error_count),
            }
        }
    }

    Ok(ImportCsvResponse {
        site_id: site.id,
        site_name: site.name.clone(),
        dry_run: req.dry_run,
        session_id: Some(session_id),
        mapped_columns,
        skipped_columns: Vec::new(),
        unmapped_columns,
        warnings,
        row_count,
        replicate_groups: 0,
        inserted_total,
        earliest,
        latest,
        derived_job_id: None,
        derived_timestamps: 0,
        duplicates: 0,
        overlaps_identical: 0,
        overlaps_differing: 0,
        overwritten: 0,
        overlap_sample: Vec::new(),
        errors,
        error_count,
        tool_runs_created,
    })
}
