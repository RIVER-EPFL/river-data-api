//! Wide-CSV historical ingestion — the backend for the dashboard's CSV import flow.
//!
//! Accepts the client's delivery format: a `DateTime` column plus one column per parameter
//! (e.g. `DateTime,DOmgL,DOuM,WaterTempdegC`) for a single target site. Each column is resolved to
//! a parameter via, in order: an explicit per-request `mapping`, the project's
//! `public_exposed_parameters` (public name), the global `parameters.aliases`, then the catalog
//! name. Columns that resolve to a derived-output parameter (e.g. `DOmgL`) are skipped — derived
//! values are recomputed from their sources, never ingested.
//!
//! `dry_run` returns the resolution plan (which columns map where, which are skipped/unmapped, plus
//! warnings, row count and date range) without writing anything, so the UI can show "here's how I'll
//! align this file" before the operator confirms.

use axum::{Json, extract::State};
use sea_orm::{ConnectionTrait, EntityTrait, Set, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::common::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::data_streams::services::get_or_create_api_stream;
use crate::routes::private::readings;
use crate::routes::resolve_site_with_project;

#[derive(Debug, Deserialize, ToSchema)]
pub struct ImportCsvRequest {
    /// Target site, by UUID or case-insensitive name.
    pub site: String,
    /// Wide CSV text: a `DateTime` column plus one column per parameter.
    pub csv: String,
    /// Optional explicit column → parameter mapping. The value is a parameter name or UUID;
    /// `null` skips the column. Overrides automatic resolution.
    #[serde(default)]
    pub mapping: Option<HashMap<String, Option<String>>>,
    /// When true, resolve and report the plan without writing anything.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportCsvResponse {
    pub site_id: Uuid,
    pub site_name: String,
    pub dry_run: bool,
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
    /// Per-row problems (bad timestamp / non-numeric value): the offending row or cell is skipped
    /// and the rest import, so the operator can fix the source and re-import. Truncated for very
    /// large files (see `error_count`).
    pub errors: Vec<RowError>,
    /// Total number of row problems (may exceed `errors.len()` when the list is truncated).
    pub error_count: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RowError {
    /// 1-based CSV line number (the header is line 1).
    pub row: usize,
    pub message: String,
}

const BATCH_SIZE: usize = 1000;
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

fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(ndt.and_utc());
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Some(ndt.and_utc());
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
        (status = 413, description = "Body exceeds 10MB limit"),
    ),
    tag = "ingestion"
)]
pub async fn import_csv(
    State(state): State<AppState>,
    Json(req): Json<ImportCsvRequest>,
) -> AppResult<Json<ImportCsvResponse>> {
    let (site, project) = resolve_site_with_project(&state.db, &req.site).await?;
    let project = project.ok_or_else(|| {
        AppError::BadRequest(format!("Site '{}' is not linked to a project", site.name))
    })?;
    let site_id = site.id;

    // --- Resolution tables (site_parameter-first) ---------------------------------------------

    // Site parameters for this site: lower(sp.name) -> (parameter_id, sp_name).
    // Also build alias map from the site's parameters only.
    let sp_rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.name AS sp_name, sp.parameter_id, p.name AS param_name, p.aliases \
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

    // Public exposure for the project: lower(public_name) -> (id, sp_name, factor, offset).
    let exposure_rows = state
        .db
        .query_all(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT pep.public_name, pep.parameter_id, \
             COALESCE(pep.conversion_factor, 1.0) AS factor, \
             COALESCE(pep.conversion_offset, 0.0) AS offset, \
             sp.name AS sp_name \
             FROM public_exposed_parameters pep \
             LEFT JOIN site_parameters sp ON sp.parameter_id = pep.parameter_id AND sp.site_id = $2 \
             WHERE pep.project_id = $1 AND pep.public_name IS NOT NULL",
            [project.id.into(), site_id.into()],
        ))
        .await?;
    let mut exposure: HashMap<String, (Uuid, String, f64, f64)> = HashMap::new();
    for row in &exposure_rows {
        let name: String = row.try_get("", "public_name").unwrap_or_default();
        let Ok(pid) = row.try_get::<Uuid>("", "parameter_id") else { continue };
        let sp_name: String = row.try_get("", "sp_name").unwrap_or(name.clone());
        let factor: f64 = row.try_get("", "factor").unwrap_or(1.0);
        let offset: f64 = row.try_get("", "offset").unwrap_or(0.0);
        exposure.insert(name.to_lowercase(), (pid, sp_name, factor, offset));
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

    // Global catalog fallback: lower(name) -> id (for columns that don't match site params).
    let catalog_rows = state
        .db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, name FROM parameters".to_owned(),
        ))
        .await?;
    let mut catalog: HashMap<String, Uuid> = HashMap::new();
    for row in &catalog_rows {
        let Ok(pid) = row.try_get::<Uuid>("", "id") else { continue };
        let name: String = row.try_get("", "name").unwrap_or_default();
        catalog.insert(name.to_lowercase(), pid);
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
        .from_reader(req.csv.as_bytes());

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
                    .or_else(|| exposure.get(&key).map(|(pid, name, f, o)| (*pid, name.clone(), *f, *o)))
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
        let Some(time) = parse_datetime(dt_cell) else {
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

    // Dry run: report the plan without writing.
    if req.dry_run {
        return Ok(Json(ImportCsvResponse {
            site_id,
            site_name: site.name,
            dry_run: true,
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
            errors,
            error_count,
        }));
    }

    if mappings.is_empty() {
        return Err(AppError::BadRequest(
            "No CSV columns resolved to ingestible parameters for this site's project".to_string(),
        ));
    }

    // --- Insert raw readings ------------------------------------------------------------------
    let mut stream_cache: HashMap<Uuid, Uuid> = HashMap::new();
    for m in &mappings {
        if let std::collections::hash_map::Entry::Vacant(e) = stream_cache.entry(m.parameter_id) {
            let stream_id = get_or_create_api_stream(&state.db, site_id, m.parameter_id).await?;
            e.insert(stream_id);
        }
    }

    let models: Vec<readings::ActiveModel> = rows
        .iter()
        .map(|(parameter_id, time, value)| readings::ActiveModel {
            stream_id: Set(stream_cache[parameter_id]),
            site_id: Set(Some(site_id)),
            parameter_id: Set(Some(*parameter_id)),
            time: Set((*time).into()),
            replicate_index: Set(0),
            raw_value: Set(*value),
            calibrated_value: Set(Some(*value)),
            sensor_id: Set(None),
            calibration_id: Set(None),
            deployment_id: Set(None),
            logged: Set(Some(false)),
            measurement_type: Set(Some("continuous".to_string())),
            is_flagged: Set(Some(false)),
            flag_reason: Set(None),
            sample_id: Set(None),
        })
        .collect();

    let mut inserted_total = 0usize;
    for chunk in models.chunks(BATCH_SIZE) {
        match readings::Entity::insert_many(chunk.to_vec())
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    readings::Column::StreamId,
                    readings::Column::Time,
                    readings::Column::ReplicateIndex,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(&state.db)
            .await
        {
            Ok(affected) => inserted_total += affected as usize,
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("None of the records") {
                    tracing::warn!(error = %e, "Failed to insert imported readings chunk");
                    return Err(AppError::Database(e));
                }
            }
        }
    }

    tracing::info!(site = %site.name, inserted_total, params = mappings.len(), "CSV import inserted readings");

    // Recompute derived parameters + refresh aggregates in the background as a tracked
    // reprocessing job, so the request returns immediately after the (fast) raw insert. Skip
    // entirely when nothing new was inserted (idempotent re-import).
    let mut distinct_ts: Vec<chrono::DateTime<chrono::Utc>> =
        rows.iter().map(|(_, t, _)| *t).collect();
    distinct_ts.sort_unstable();
    distinct_ts.dedup();
    let derived_timestamps = distinct_ts.len();

    let derived_job_id = if inserted_total > 0 {
        let job_id = Uuid::new_v4();
        let total = i32::try_from(derived_timestamps).unwrap_or(i32::MAX);
        state
            .db
            .execute(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO reprocessing_jobs (id, sensor_id, trigger_type, trigger_id, status, total, progress) \
                 VALUES ($1, NULL, 'csv_import', NULL, 'pending', $2, 0)",
                [job_id.into(), total.into()],
            ))
            .await?;

        let db = state.db.clone();
        let app = state.clone();
        let since = earliest;
        tokio::spawn(async move {
            let _ = db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'running' WHERE id = $1",
                    [job_id.into()],
                ))
                .await;

            let mut filled = 0i32;
            for (i, time) in distinct_ts.iter().enumerate() {
                if crate::routes::private::sensor_calibrations::services::recalculate_derived_at_timestamp(
                    &db, site_id, *time,
                )
                .await
                .is_ok()
                {
                    filled += 1;
                }
                if (i + 1) % 500 == 0 {
                    let _ = db
                        .execute(Statement::from_sql_and_values(
                            sea_orm::DatabaseBackend::Postgres,
                            "UPDATE reprocessing_jobs SET progress = $1 WHERE id = $2",
                            [i32::try_from(i + 1).unwrap_or(i32::MAX).into(), job_id.into()],
                        ))
                        .await;
                }
            }

            if let Some(s) = since {
                crate::common::sync_state::refresh_continuous_aggregates(&db, Some(s)).await;
            }
            crate::common::cache::invalidate_prefix(&app, &format!("readings:{site_id}")).await;
            crate::common::cache::invalidate_prefix(&app, &format!("aggregates:{site_id}")).await;

            let _ = db
                .execute(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "UPDATE reprocessing_jobs SET status = 'completed', progress = total, \
                     readings_updated = $1, completed_at = NOW() WHERE id = $2",
                    [filled.into(), job_id.into()],
                ))
                .await;
        });
        Some(job_id)
    } else {
        None
    };

    // Readings we tried to insert but that already existed (skipped via ON CONFLICT).
    let duplicates = rows.len().saturating_sub(inserted_total);

    Ok(Json(ImportCsvResponse {
        site_id,
        site_name: site.name,
        dry_run: false,
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
        errors,
        error_count,
    }))
}
