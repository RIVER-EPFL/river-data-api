use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::Query;
use axum::extract::State;
use chrono::Utc;
use sea_orm::ActiveModelTrait;
use sea_orm::ColumnTrait;
use sea_orm::Condition;
use sea_orm::EntityTrait;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::QueryTrait;
use sea_orm::Set;
use sea_orm::TransactionTrait;
use sea_orm::sea_query::Expr;
use tower_http::limit::RequestBodyLimitLayer;
use uuid::Uuid;

use super::models::*;
use super::service::*;
use crate::common::AppState;
use crate::common::actor::label;
use crate::common::authz::AccessScope;
use crate::common::middleware::AuthContext;
use crate::common::middleware::IsSyncService;
use crate::common::middleware::ProjectScope;
use crate::common::middleware::enforce_project_scope_for_sites;
use crate::common::middleware::require_admin;
use crate::common::middleware::require_read_data;
use crate::common::middleware::require_write_data;
use crate::common::middleware::scope_site_ids;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::collection_events::flows;
use crate::routes::private::data_streams;
use crate::routes::private::data_streams::service::get_or_create_api_stream;
use crate::routes::private::parameters;
use crate::routes::private::readings;
use crate::routes::private::readings::decision_model;
use crate::routes::private::readings::models::ConflictMode;
use crate::routes::private::readings::models::Kind;
use crate::routes::private::readings::models::Origin;
use crate::routes::private::readings::models::Owner;
use crate::routes::private::readings::models::ProvenanceQuery;
use crate::routes::private::readings::service::CurveClaim;
use crate::routes::private::readings::service::Replace;
use crate::routes::private::readings::service::admit_standard_curves;
use crate::routes::private::readings::service::readings_on_conflict;
use crate::routes::private::readings::service::readings_upsert;
use crate::routes::private::readings::service::rows_at;
use crate::routes::private::readings::service::run_id_of;
use crate::routes::private::readings::status_events;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::resolver;
use crate::routes::private::sensor_calibrations::service::Curve;
use crate::routes::private::sensor_calibrations::service::apply_curves;
use crate::routes::private::sensors::models::ResolvedOwner;
use crate::routes::private::sensors::service::resolve_slot_owner_for_times;
use crate::routes::private::sensors::service::resolve_windows_for_times;
use crate::routes::private::site_parameters;
use crate::routes::private::sites;
use crate::routes::private::standard_curves;
use crate::routes::private::sync::models::GroupAudit;
use crate::routes::private::sync::models::HoldStatus;
use crate::routes::private::sync::service as audit;
use crate::routes::resolve_site_with_project;
use crate::routes::service::ACTION_BODY_LIMIT;
use crate::routes::service::DATA_BODY_LIMIT;
use crate::routes::service::IMPORT_BODY_LIMIT;

/// Preview a replicate group's statistics after flagging or restoring replicates.
/// Nothing is written. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/readings/sample_preview",
    request_body = SamplePreviewRequest,
    responses(
        (status = 200, body = SamplePreviewResponse),
        (status = 400, description = "Neither key form, or an index the group does not hold"),
        (status = 404, description = "No spot reading at that instant, or no such hold on it"),
    ),
    tag = "readings"
)]
pub async fn sample_preview(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(q): Json<SamplePreviewRequest>,
) -> AppResult<Json<SamplePreviewResponse>> {
    let time = sea_orm::prelude::DateTimeWithTimeZone::from(q.time);
    let find = preview_rows().filter(readings::Column::Time.eq(time));
    let rows = match (q.stream_id, q.site_id, q.parameter_id) {
        (Some(stream_id), _, _) => find.filter(readings::Column::StreamId.eq(stream_id)),
        (None, Some(site_id), Some(parameter_id)) => find
            .filter(readings::Column::SiteId.eq(site_id))
            .filter(readings::Column::ParameterId.eq(parameter_id))
            .filter(readings::Column::MeasurementType.eq("spot")),
        _ => {
            return Err(AppError::BadRequest(
                "Provide either stream_id or both site_id and parameter_id".to_string(),
            ));
        }
    }
    .order_by_asc(readings::Column::ReplicateIndex)
    .into_model::<PreviewRow>()
    .all(&state.db)
    .await?;
    if rows.is_empty() {
        return Err(AppError::NotFound("No reading at that instant".to_string()));
    }
    let mut replicates = Vec::with_capacity(rows.len());
    let mut slot: Option<(Uuid, Uuid)> = None;
    for row in &rows {
        if let (Some(s), Some(p)) = (row.site_id, row.parameter_id) {
            slot.get_or_insert((s, p));
        }
        replicates.push(Replicate {
            index: row.replicate_index,
            value: row.value,
            flagged: row.flagged,
            withdrawn: row.withdrawn,
            unverified: row.unverified,
        });
    }
    if let Some((site_id, _)) = slot {
        enforce_project_scope_for_sites(&state.db, &scope, &[site_id]).await?;
    } else if scope.is_restricted() {
        return Err(AppError::NotFound("No reading at that instant".to_string()));
    }
    let known: Vec<i16> = replicates.iter().map(|r| r.index).collect();
    let unknown: Vec<String> = q
        .exclude_replicate_indexes
        .iter()
        .chain(q.include_replicate_indexes.iter())
        .filter(|i| !known.contains(i))
        .map(i16::to_string)
        .collect();
    if !unknown.is_empty() {
        return Err(AppError::BadRequest(format!(
            "no reading at replicate index {} in this group",
            unknown.join(", ")
        )));
    }

    let change = Change {
        exclude: &q.exclude_replicate_indexes,
        include: &q.include_replicate_indexes,
    };
    let (current, proposed, delta, rows) = preview_statistics(&replicates, &change);

    let hold = match q.hold_id {
        None => None,
        Some(hold_id) => {
            let expected = crate::routes::private::readings::service::hold_expectation(
                &state.db, hold_id, time,
            )
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!("no replicate audit hold {hold_id} on this instant"))
            })?;
            let expected = GroupAudit {
                time: q.time,
                expected_mean: expected.get("mean").and_then(serde_json::Value::as_f64),
                expected_sd: expected.get("sd").and_then(serde_json::Value::as_f64),
                expected_n: expected.get("n").and_then(serde_json::Value::as_i64),
            };
            Some(hold_match(hold_id, &expected, &current, &proposed))
        }
    };

    Ok(Json(SamplePreviewResponse {
        current,
        proposed,
        delta,
        replicates: rows,
        hold,
    }))
}

/// Screen entered values against the site's seasonal distribution (same site, entry month ±2
/// across all years, unflagged spot replicates pooled; min/Q10/Q90/max). Stores the check and
/// returns its id: pass it as `check_id` on `/grab_samples` and the save is validated against
/// exactly these values, so an edit after checking requires a fresh check. Requires `read_data`.
///
/// Raw against raw: the window pools `raw_value` and the screened value is the number as entered,
/// so a correction applied to the history cannot move the distribution out from under it. The
/// caveat this does not solve: a CSV imported with `values: "corrected"` stores a processed number
/// in `raw_value`, so such rows pool on a different basis than a typed entry.
#[utoipa::path(
    post,
    path = "/api/readings/seasonal_check",
    request_body = SeasonalCheckRequest,
    responses(
        (status = 200, description = "Per-value classification with the distribution payload and the method", body = SeasonalCheckResponse),
        (status = 403, description = "Site outside the caller's projects"),
        (status = 404, description = "Site not found"),
    ),
    tag = "ingestion"
)]
pub async fn seasonal_check(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<SeasonalCheckRequest>,
) -> AppResult<Json<SeasonalCheckResponse>> {
    crate::common::scope::require_sites_in_scope(&state.db, &scope, &[req.site_id]).await?;
    if req.values.is_empty() {
        return Err(AppError::BadRequest("No values to check".to_string()));
    }
    let site_exists = crate::routes::private::sites::Entity::find_by_id(req.site_id)
        .one(&state.db)
        .await?
        .is_some();
    if !site_exists {
        return Err(AppError::NotFound(format!(
            "Site {} not found",
            req.site_id
        )));
    }

    let mut findings = Vec::with_capacity(req.values.len());
    for v in &req.values {
        let stats = seasonal_stats(&state.db, req.site_id, v.parameter_id, req.time).await?;
        let distribution = if stats.n > 0 {
            seasonal_distribution(&state.db, req.site_id, v.parameter_id, req.time).await?
        } else {
            Vec::new()
        };
        let class = stats.classify(v.value);
        findings.push(SeasonalFinding {
            parameter_id: v.parameter_id,
            value: v.value,
            class,
            warning: class.is_warning(),
            n: stats.n,
            min: stats.min,
            q10: stats.q10,
            q90: stats.q90,
            max: stats.max,
            distribution,
        });
    }

    let check_id = store_check(
        &state.db,
        req.site_id,
        req.time,
        &req.values,
        crate::common::actor::label(&auth),
    )
    .await?;

    let warnings = findings.iter().filter(|f| f.warning).count();
    Ok(Json(SeasonalCheckResponse {
        check_id,
        findings,
        warnings,
        method: method(),
    }))
}

/// Detach an output slot at a visit from its calculation (Q40's admin override): the rows
/// become manual entries and the chain no longer writes the slot there, until an input at the
/// visit changes or the slot is returned. Requires Administrator.
#[utoipa::path(
    post,
    path = "/api/readings/detach",
    request_body = OutputSlotRequest,
    responses(
        (status = 200, description = "Detached", body = OwnershipResponse),
        (status = 404, description = "No readings at the slot instant"),
        (status = 409, description = "Already manual"),
    ),
    tag = "readings"
)]
pub async fn detach_output(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<OutputSlotRequest>,
) -> AppResult<Json<OwnershipResponse>> {
    let actor = crate::common::actor::label(&auth);
    let rows = output_rows_at(&state.db, req.site_id, req.parameter_id, req.time).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound(
            "No spot readings at that site, parameter and instant".to_string(),
        ));
    }
    if output_owner(&state.db, req.site_id, req.parameter_id, req.time).await? == Owner::Manual {
        return Err(AppError::Conflict(
            "The slot is already detached".to_string(),
        ));
    }
    let mut streams: Vec<Uuid> = rows.iter().map(|(s, _)| *s).collect();
    streams.dedup();
    let mut decided = 0u64;
    let recorded = crate::common::bulk_write::guarded(&state.db, async |txn| {
        for stream_id in &streams {
            record(
                txn,
                &Decision {
                    key: DecisionKey {
                        stream_id: *stream_id,
                        time: req.time,
                        replicate_index: None,
                    },
                    kind: Kind::Detach,
                    new: serde_json::json!({ "owner": "manual" }),
                    actor: actor.clone(),
                    reason: req.reason.clone(),
                    origin: Origin::Manual,
                    set_id: None,
                },
            )
            .await?;
            decided += 1;
        }
        Ok(decided)
    })
    .await?;
    Ok(Json(OwnershipResponse {
        owner: Owner::Manual,
        rows_decided: recorded,
    }))
}

/// Return a detached output slot to its calculation: the value the last correction since the
/// detach replaced is restored and the tool owns the slot again. Requires Administrator.
#[utoipa::path(
    post,
    path = "/api/readings/return",
    request_body = OutputSlotRequest,
    responses(
        (status = 200, description = "Returned", body = OwnershipResponse),
        (status = 404, description = "No readings at the slot instant"),
        (status = 409, description = "Not detached"),
    ),
    tag = "readings"
)]
pub async fn return_output(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<OutputSlotRequest>,
) -> AppResult<Json<OwnershipResponse>> {
    let actor = crate::common::actor::label(&auth);
    let rows = output_rows_at(&state.db, req.site_id, req.parameter_id, req.time).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound(
            "No spot readings at that site, parameter and instant".to_string(),
        ));
    }
    if output_owner(&state.db, req.site_id, req.parameter_id, req.time).await? == Owner::Tool {
        return Err(AppError::Conflict("The slot is not detached".to_string()));
    }
    let mut streams: Vec<Uuid> = rows.iter().map(|(s, _)| *s).collect();
    streams.dedup();
    let decided = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let mut decided = 0u64;
        for stream_id in &streams {
            // The value the first correction after the detach replaced is the tool's last value.
            let at = sea_orm::prelude::DateTimeWithTimeZone::from(req.time);
            let detached_at = decision_model::Entity::find()
                .select_only()
                .expr(decision_model::Column::At.max())
                .filter(decision_model::Column::StreamId.eq(*stream_id))
                .filter(decision_model::Column::Time.eq(at))
                .filter(decision_model::Column::Kind.eq(Kind::Detach.as_str()))
                .filter(decision_model::Column::RolledBackBy.is_null())
                .into_query();
            let restore = decision_model::Entity::find()
                .select_only()
                .column(decision_model::Column::ReplicateIndex)
                .column(decision_model::Column::Old)
                .filter(decision_model::Column::StreamId.eq(*stream_id))
                .filter(decision_model::Column::Time.eq(at))
                .filter(decision_model::Column::Kind.eq(Kind::ValueCorrection.as_str()))
                .filter(decision_model::Column::RolledBackBy.is_null())
                .filter(sea_orm::ExprTrait::gt(
                    Expr::col(decision_model::Column::At),
                    detached_at,
                ))
                .distinct_on([decision_model::Column::ReplicateIndex])
                .order_by_asc(decision_model::Column::ReplicateIndex)
                .order_by_asc(decision_model::Column::At)
                .order_by_asc(decision_model::Column::Id)
                .into_tuple::<(Option<i16>, serde_json::Value)>()
                .all(txn)
                .await?;
            let rows: Vec<(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)> = restore
                .into_iter()
                .filter_map(|(replicate_index, old)| {
                    let raw = old.get("raw_value")?.as_f64()?;
                    Some((
                        req.time,
                        replicate_index?,
                        serde_json::json!({ "raw_value": raw }),
                    ))
                })
                .collect();
            record_keyed(
                txn,
                Kind::ValueCorrection,
                *stream_id,
                &rows,
                &actor,
                Some("returned to the calculation's value"),
                Origin::Rollback,
                Keyed::Changed,
                None,
                None,
            )
            .await?;
            record(
                txn,
                &Decision {
                    key: DecisionKey {
                        stream_id: *stream_id,
                        time: req.time,
                        replicate_index: None,
                    },
                    kind: Kind::Return,
                    new: serde_json::json!({ "owner": "tool" }),
                    actor: actor.clone(),
                    reason: req.reason.clone(),
                    origin: Origin::Manual,
                    set_id: None,
                },
            )
            .await?;
            decided += 1;
        }
        Ok(decided)
    })
    .await?;
    Ok(Json(OwnershipResponse {
        owner: Owner::Tool,
        rows_decided: decided,
    }))
}

/// The decision history of one reading or replicate group, newest first. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/decisions",
    params(DecisionsQuery),
    responses((status = 200, description = "Decisions, newest first", body = [DecisionRow])),
    tag = "readings"
)]
pub async fn list_decisions(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<DecisionsQuery>,
) -> AppResult<Json<Vec<DecisionRow>>> {
    require_reading_in_scope(&state.db, &scope, q.stream_id, q.time).await?;
    let key = DecisionKey {
        stream_id: q.stream_id,
        time: q.time,
        replicate_index: q.replicate_index,
    };
    Ok(Json(history(&state.db, &key).await?))
}

/// A derived value's own arithmetic: the formula the computation recorded, run again over the
/// values it consumed, beside the number the reading holds. Reads only; writes nothing and touches
/// no live value. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/replay",
    params(DecisionsQuery),
    responses(
        (status = 200, description = "The recorded formula over the recorded values", body = ReplayResponse),
        (status = 404, description = "No reading, or no computation captured at that key"),
        (status = 409, description = "The captured set cannot be evaluated"),
    ),
    tag = "readings"
)]
pub async fn replay_derived(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<DecisionsQuery>,
) -> AppResult<Json<ReplayResponse>> {
    require_reading_in_scope(&state.db, &scope, q.stream_id, q.time).await?;
    let key = DecisionKey {
        stream_id: q.stream_id,
        time: q.time,
        replicate_index: q.replicate_index.or(Some(0)),
    };
    Ok(Json(
        crate::routes::private::readings::service::replay_at(&state.db, &key).await?,
    ))
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

/// What each row a selection covers may have done to it, and by which route. Requires `read_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/inspect",
    request_body = InspectRequest,
    responses(
        (status = 200, description = "The rows and their routes", body = InspectResponse),
        (status = 400, description = "A selection naming nothing"),
    ),
    tag = "readings"
)]
pub async fn inspect(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<InspectRequest>,
) -> AppResult<Json<InspectResponse>> {
    let sites = scope_site_ids(&state.db, &scope).await?;
    Ok(Json(InspectResponse {
        rows: inspect_rows(&state.db, &req.selection, sites.as_deref()).await?,
    }))
}

/// Apply the decision, read back every number it moved, and roll it back. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/preview",
    request_body = EditRequest,
    responses(
        (status = 200, description = "What the edit would do", body = PreviewResponse),
        (status = 400, description = "An edit the rows' provenance does not permit"),
    ),
    tag = "readings"
)]
pub async fn preview(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<EditRequest>,
) -> AppResult<Json<PreviewResponse>> {
    let actor = label(&auth);
    let kind = req.decision.parsed()?;
    let (_, option) = req.decision.assertion_over(kind, &req.selection)?;
    authorise(&auth, option)?;
    let rows_where = req.selection.condition()?;
    let sites = selected_sites(&state.db, rows_where.clone()).await?;
    require_edit_in_scope(&state.db, &scope, &sites).await?;
    refuse_unrouted(&state.db, &req.selection, option).await?;
    admit_edit_curve(&state.db, rows_where.clone(), kind, req.decision.target_id).await?;
    let parameters = touched_parameters(&state.db, rows_where.clone()).await?;

    // The transaction is the preview: the decision is applied, the numbers are read back from the
    // rows the trigger just rewrote, and the whole thing is undone. Nothing recomputes the
    // arithmetic a second time, so the preview cannot disagree with the write.
    let (rows, samples) = crate::common::bulk_write::guarded_rollback(&state.db, async |txn| {
        let before_rows = row_states(txn, rows_where.clone()).await?;
        let before_samples = sample_states(txn, rows_where.clone()).await?;
        apply(txn, &req.selection, &req.decision, &actor, auth.origin()).await?;
        let after_rows = row_states(txn, rows_where.clone()).await?;
        let after_samples = sample_states(txn, rows_where.clone()).await?;
        let rows: Vec<MovedRow> = before_rows
            .into_iter()
            .zip(after_rows)
            .map(
                |((stream_id, time, index, before), (_, _, _, after))| MovedRow {
                    stream_id,
                    time,
                    replicate_index: index,
                    before,
                    after,
                },
            )
            .collect();
        let samples: Vec<MovedSample> = before_samples
            .into_iter()
            .zip(after_samples)
            .map(|((sample_id, before), (_, after))| MovedSample {
                sample_id,
                before,
                after,
            })
            .collect();
        Ok((rows, samples))
    })
    .await?;

    let calculations =
        crate::routes::private::tools::service::calculations_fed_by(&state.db, &parameters).await?;

    Ok(Json(PreviewResponse {
        preview_id: preview_id(&req.selection, &req.decision)?,
        rows,
        samples,
        calculations,
        not_previewed: vec![
            "continuous aggregates, which the commit refreshes over the range it moved".to_string(),
            "alarm episodes, which the commit re-evaluates for the slots it touched".to_string(),
            "the calculations listed, which run as their own job after the commit".to_string(),
        ],
    }))
}

/// Commit an edit, held to the selection and decision the preview covered. Requires `write_data`;
/// an attribution edit requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/readings/edits",
    request_body = EditRequest,
    responses(
        (status = 200, description = "The edit was recorded", body = EditResponse),
        (status = 400, description = "An edit the rows' provenance does not permit"),
        (status = 409, description = "The preview covered a different selection or decision"),
    ),
    tag = "readings"
)]
pub async fn commit(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(req): Json<EditRequest>,
) -> AppResult<Json<EditResponse>> {
    let actor = label(&auth);
    let kind = req.decision.parsed()?;
    let (_, option) = req.decision.assertion_over(kind, &req.selection)?;
    authorise(&auth, option)?;
    let sites = selected_sites(&state.db, req.selection.condition()?).await?;
    require_edit_in_scope(&state.db, &scope, &sites).await?;
    admit_edit_curve(
        &state.db,
        req.selection.condition()?,
        kind,
        req.decision.target_id,
    )
    .await?;
    let expected = preview_id(&req.selection, &req.decision)?;
    match req.preview_id {
        Some(id) if id == expected => {}
        Some(_) => {
            return Err(AppError::Conflict(
                "that preview covered a different selection or decision; preview this one first"
                    .to_string(),
            ));
        }
        None => {
            return Err(AppError::BadRequest(
                "an edit is committed against the preview of itself; call preview first"
                    .to_string(),
            ));
        }
    }
    refuse_unrouted(&state.db, &req.selection, option).await?;

    let (set_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        apply(txn, &req.selection, &req.decision, &actor, auth.origin()).await
    })
    .await?;

    propagate(&state, &recorded, &actor).await?;

    let ids = decisions_recorded(&state.db, &req.selection, kind, recorded.rows).await?;
    Ok(Json(EditResponse {
        rows_decided: recorded.rows,
        decision_ids: ids,
        set_id,
    }))
}

/// Invert one edit, restoring exactly the state its decision recorded. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/{id}/rollback",
    params(("id" = Uuid, Path, description = "The decision the edit recorded")),
    responses(
        (status = 200, description = "Inverted", body = RollbackResponse),
        (status = 404, description = "No such decision"),
        (status = 409, description = "Already rolled back, or a decision that projects nothing"),
    ),
    tag = "readings"
)]
pub async fn rollback(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> AppResult<Json<RollbackResponse>> {
    let actor = label(&auth);
    let sites = decided_sites(&state.db, decision_model::Column::Id, id).await?;
    require_edit_in_scope(&state.db, &scope, &sites).await?;
    let (rollback_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        crate::routes::private::readings::service::rollback(
            txn,
            id,
            &actor,
            Some("edit rolled back"),
        )
        .await
    })
    .await?;
    propagate(&state, &recorded, &actor).await?;
    // Inverting a pin changes what the window resolves for that reading, and only the reprocess
    // writes it. The forward path enqueues the same job.
    crate::routes::private::readings::service::enqueue_pin_reprocess_for_decision(&state.db, id)
        .await?;
    Ok(Json(RollbackResponse { rollback_id }))
}

/// Invert every live decision an edit's set recorded, restoring exactly the state each one
/// recorded. This is how a visit retracted whole is put back. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/edits/sets/{set_id}/rollback",
    params(("set_id" = Uuid, Path, description = "The set the edit recorded")),
    responses(
        (status = 200, description = "Inverted", body = RollbackSetResponse),
        (status = 404, description = "No such set"),
        (status = 409, description = "Already rolled back"),
    ),
    tag = "readings"
)]
pub async fn rollback_edit_set(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    ProjectScope(scope): ProjectScope,
    axum::extract::Path(set_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<RollbackSetResponse>> {
    let actor = label(&auth);
    let sites = decided_sites(&state.db, decision_model::Column::SetId, set_id).await?;
    require_edit_in_scope(&state.db, &scope, &sites).await?;
    let (rolled_back, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        crate::routes::private::readings::service::rollback_set(
            txn,
            set_id,
            &actor,
            Some("edit set rolled back"),
        )
        .await
    })
    .await?;
    propagate(&state, &recorded, &actor).await?;
    crate::routes::private::readings::service::enqueue_pin_reprocess_for_set(&state.db, set_id)
        .await?;
    Ok(Json(RollbackSetResponse {
        set_id,
        rolled_back,
    }))
}

/// The stored run in the shape `/tools/{name}/calculate` takes, so a value a tool produced is
/// corrected by reopening it, editing an input and saving again (Q8, M4). Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/tool_runs/{id}/reload",
    params(("id" = Uuid, Path, description = "Tool run id")),
    responses(
        (status = 200, description = "The run's own inputs and context", body = ReloadResponse),
        (status = 404, description = "No such run"),
    ),
    tag = "tools"
)]
pub async fn reload_run(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> AppResult<Json<ReloadResponse>> {
    let run =
        crate::routes::private::tools::service::find_run_in_scope(&state.db, &scope, id).await?;
    let context = run.context.unwrap_or(serde_json::Value::Null);
    let mut body = run.inputs.as_object().cloned().unwrap_or_default();
    // The reserved context fields the calculate body takes, so the reopened run resolves its
    // station and event inputs at the same visit rather than at whatever the browser last saw.
    for field in ["site_id", "collected_at"] {
        if let Some(value) = context.get(field)
            && !value.is_null()
        {
            body.insert(field.to_string(), value.clone());
        }
    }
    Ok(Json(ReloadResponse {
        tool: run.tool_name,
        body: serde_json::Value::Object(body),
        constants: run.constants,
        curves: run.curves.as_array().cloned().unwrap_or_default(),
    }))
}

/// Accept or reject proposed corrections. Requires `manage_sensors`.
#[utoipa::path(
    post,
    path = "/api/sync/change_proposals/decide",
    request_body = DecideRequest,
    responses(
        (status = 200, description = "What the decision did", body = DecideResponse),
        (status = 400, description = "An unrecognised decision"),
    ),
    tag = "sync"
)]
pub async fn decide_proposals(
    axum::extract::State(state): axum::extract::State<crate::common::AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    crate::common::middleware::ProjectScope(scope): crate::common::middleware::ProjectScope,
    axum::Json(req): axum::Json<DecideRequest>,
) -> AppResult<axum::Json<DecideResponse>> {
    let accept = parse_decision(&req.decision)?;
    let actor = crate::common::actor::label(&auth);
    let projects = scope.project_ids();
    let (response, written) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        decide(
            txn,
            &req.ids,
            accept,
            &actor,
            req.reason.as_deref(),
            projects.as_deref(),
        )
        .await
    })
    .await?;
    // An accepted correction rewrites a served value, so it takes the same tail every other
    // curation write takes: the rollups over the span it moved, the cache, and the visits whose
    // calculations read it.
    crate::routes::private::readings::service::run(
        &state,
        &crate::routes::private::readings::service::Written::new(written.rows)
            .over(written.span)
            .touching(written.touched_events.clone()),
        &ACCEPT_TAIL,
        &actor,
    )
    .await?;
    Ok(axum::Json(response))
}

/// The assembled record of one measured instant: where it came from, when it arrived, what
/// instrument and corrections produced the stored value, the visit it belongs to, the tool run
/// that computed it, and any review holds touching it. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/provenance",
    params(ProvenanceQuery),
    responses(
        (status = 200, description = "Provenance record", body = ProvenanceResponse),
        (status = 400, description = "Neither key form provided"),
        (status = 404, description = "No reading at that instant"),
    ),
    tag = "readings"
)]
pub async fn get_reading_provenance(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(q): Query<ProvenanceQuery>,
) -> AppResult<Json<ProvenanceResponse>> {
    let rows = rows_at(&state.db, &q, &scope).await?;
    let records = assemble_records(&state.db, &rows, q.time).await?;

    let site_id = rows.iter().find_map(|r| r.site_id).or(q.site_id);
    let parameter_id = rows.iter().find_map(|r| r.parameter_id).or(q.parameter_id);
    let slot = match (site_id, parameter_id) {
        (Some(site_id), Some(parameter_id)) => {
            slot_identity(&state.db, site_id, parameter_id).await?
        }
        _ => None,
    };

    Ok(Json(ProvenanceResponse {
        time: q.time,
        site_id,
        parameter_id,
        parameter_code: slot.as_ref().map(|s| s.0.clone()),
        parameter_name: slot.as_ref().map(|s| s.1.clone()),
        units: slot.as_ref().and_then(|s| s.2.clone()),
        decimal_places: slot.as_ref().and_then(|s| s.3),
        duplicate_slot: records.len() > 1,
        records,
    }))
}

#[utoipa::path(
    patch,
    path = "/api/readings/flag",
    request_body = FlagReadingsRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "Missing readings or reason"),
    ),
    tag = "ingestion"
)]
pub async fn flag_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<FlagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings specified".to_string()));
    }
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    let updated = apply_flags(
        &state,
        &scope,
        &label(&auth),
        auth.origin(),
        &payload.readings,
        FlagWrite::Set(payload.reason.clone()),
    )
    .await?;

    tracing::info!(updated, reason = %payload.reason, "Flagged readings");
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

/// Unflag a set of previously-flagged readings. Requires `write_data`.
#[utoipa::path(
    patch,
    path = "/api/readings/unflag",
    request_body = UnflagReadingsRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "No readings specified"),
    ),
    tag = "ingestion"
)]
pub async fn unflag_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<UnflagReadingsRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let updated = apply_flags(
        &state,
        &scope,
        &label(&auth),
        auth.origin(),
        &payload.readings,
        FlagWrite::Clear,
    )
    .await?;

    tracing::info!(updated, "Unflagged readings");
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

/// Flag every reading in a (site_id, parameter_id, time range). Requires `write_data`.
/// Refreshes continuous aggregates for the affected window on success. `dry_run` returns the
/// count alone.
#[utoipa::path(
    patch,
    path = "/api/readings/flag_range",
    request_body = FlagRangeRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "Missing reason or end_time < start_time"),
    ),
    tag = "ingestion"
)]
pub async fn flag_range(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<FlagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let range = SlotRange {
        site_id: payload.site_id,
        parameter_id: payload.parameter_id,
        start_time: payload.start_time,
        end_time: payload.end_time,
    };
    if payload.dry_run {
        let updated =
            count_flags_over_range(&state, &scope, range, &FlagWrite::Set(String::new())).await?;
        let calculations = crate::routes::private::tools::service::calculations_fed_by(
            &state.db,
            &[payload.parameter_id],
        )
        .await?;
        return Ok(Json(FlagReadingsResponse {
            updated,
            calculations,
        }));
    }
    if payload.reason.trim().is_empty() {
        return Err(AppError::BadRequest("Reason is required".to_string()));
    }
    let updated = apply_flags_over_range(
        &state,
        &scope,
        &label(&auth),
        auth.origin(),
        range,
        &FlagWrite::Set(payload.reason.clone()),
    )
    .await?;

    tracing::info!(
        updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        reason = %payload.reason,
        "Flagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

/// Unflag every reading in a (site_id, parameter_id, time range). Requires `write_data`.
/// Refreshes continuous aggregates for the affected window on success. `dry_run` returns the
/// count alone.
#[utoipa::path(
    patch,
    path = "/api/readings/unflag_range",
    request_body = UnflagRangeRequest,
    responses(
        (status = 200, description = "Number of readings updated", body = FlagReadingsResponse),
        (status = 400, description = "end_time < start_time"),
    ),
    tag = "ingestion"
)]
pub async fn unflag_range(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<UnflagRangeRequest>,
) -> AppResult<Json<FlagReadingsResponse>> {
    let range = SlotRange {
        site_id: payload.site_id,
        parameter_id: payload.parameter_id,
        start_time: payload.start_time,
        end_time: payload.end_time,
    };
    if payload.dry_run {
        let updated = count_flags_over_range(&state, &scope, range, &FlagWrite::Clear).await?;
        let calculations = crate::routes::private::tools::service::calculations_fed_by(
            &state.db,
            &[payload.parameter_id],
        )
        .await?;
        return Ok(Json(FlagReadingsResponse {
            updated,
            calculations,
        }));
    }
    let updated = apply_flags_over_range(
        &state,
        &scope,
        &label(&auth),
        auth.origin(),
        range,
        &FlagWrite::Clear,
    )
    .await?;

    tracing::info!(
        updated,
        site_id = %payload.site_id,
        parameter_id = %payload.parameter_id,
        "Unflagged readings (range)"
    );
    Ok(Json(FlagReadingsResponse {
        updated,
        calculations: Vec::new(),
    }))
}

/// Batch insert readings keyed by (site_id, parameter_id). Auto-creates "api" streams when
/// a (site, parameter) pair has none. 10MB body limit. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/batch",
    request_body = BatchReadingsRequest,
    responses(
        (status = 200, description = "Inserted count", body = BatchReadingsResponse),
        (status = 400, description = "Timestamp outside the admissible window, or non-finite value"),
        (status = 413, description = "Body exceeds 10MB limit"),
    ),
    tag = "ingestion"
)]
pub async fn insert_batch_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<BatchReadingsRequest>,
) -> AppResult<Json<BatchReadingsResponse>> {
    // A project-scoped token may only write to sites within its project.
    let target_sites: Vec<Uuid> = payload.readings.iter().map(|r| r.site_id).collect();
    enforce_project_scope_for_sites(&state.db, &scope, &target_sites).await?;

    for r in &payload.readings {
        crate::routes::private::readings::service::admission::admit(
            r.time,
            r.raw_value,
            r.measurement_type.as_deref(),
        )?;
        if let Some(calibrated) = r.calibrated_value {
            crate::routes::private::readings::service::admission::admit_value(calibrated)?;
        }
    }

    // Collect unique (site_id, parameter_id) pairs and resolve stream_ids
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    // Whether each pair names a slot that exists. A reading is attributed by the pairing, never by
    // the request alone: a caller naming a parameter the site has not been assigned would otherwise
    // mint attribution no `site_parameters` row backs, which every later resolution reads as wrong.
    let mut slot_exists: HashMap<(Uuid, Uuid), bool> = HashMap::new();

    for r in &payload.readings {
        let key = (r.site_id, r.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, r.site_id, r.parameter_id).await?;
            entry.insert(stream_id);
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = slot_exists.entry(key) {
            let paired = crate::routes::private::data_streams::service::site_parameter_of(
                &state.db,
                r.site_id,
                r.parameter_id,
            )
            .await?
            .is_some();
            entry.insert(paired);
        }
    }

    // Collect unique (site_id, time) pairs for derived auto-compute
    let site_timestamps_for_derived: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = {
        let mut map: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for r in &payload.readings {
            map.entry(r.site_id).or_default().push(r.time);
        }
        for timestamps in map.values_mut() {
            timestamps.sort();
            timestamps.dedup();
        }
        map
    };

    // For rows that don't carry an explicit sensor, resolve it from the deployment window covering
    // the time so batch-inserted data lands attributed. Explicit payload values always win.
    let mut owner_map: HashMap<(Uuid, Uuid, chrono::DateTime<chrono::Utc>), ResolvedOwner> =
        HashMap::new();
    {
        let mut times_by_slot: HashMap<(Uuid, Uuid), Vec<chrono::DateTime<chrono::Utc>>> =
            HashMap::new();
        for r in &payload.readings {
            if r.sensor_id.is_none() {
                times_by_slot
                    .entry((r.site_id, r.parameter_id))
                    .or_default()
                    .push(r.time);
            }
        }
        for ((site, param), ts) in &times_by_slot {
            let resolved = resolve_slot_owner_for_times(&state.db, *site, *param, ts).await?;
            for (t, owner) in resolved {
                owner_map.insert((*site, *param, t), owner);
            }
        }
    }

    // Stream-declared defaults: a retagged "api" stream must classify batch writes the same
    // way it classifies /ingest writes. The channel's instrument travels with them: it is what a
    // row naming neither an instrument of its own nor a deployed one is attributed to, so a batch
    // write cannot store a measurement whose instrument is unknown.
    let mut stream_sensors: HashMap<Uuid, Uuid> = HashMap::new();
    let stream_defaults: HashMap<Uuid, Option<String>> = {
        let stream_ids: Vec<Uuid> = stream_cache.values().copied().collect();
        let mut map = HashMap::with_capacity(stream_ids.len());
        for stream in data_streams::Entity::find()
            .filter(data_streams::Column::Id.is_in(stream_ids))
            .all(&state.db)
            .await?
        {
            if let Some(sensor_id) = stream.sensor_id {
                stream_sensors.insert(stream.id, sensor_id);
            }
            map.insert(stream.id, stream.measurement_type);
        }
        map
    };

    // Sensor-frequency defaults for readings that don't declare a measurement_type: explicit
    // payload sensors plus slot-owner-resolved ones, one query.
    let sensor_types = {
        let mut candidate_sensors: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(owner_map.values().filter_map(|o| o.sensor_id))
            .collect();
        candidate_sensors.sort_unstable();
        candidate_sensors.dedup();
        crate::routes::private::readings::service::measurement_types_for_sensors(
            &state.db,
            &candidate_sensors,
        )
        .await?
    };

    // Per-reading context, resolved before the models are built: which stream the row lands on,
    // which instrument it inherits when it names none, and what it classifies as. The standard
    // curve rules below are stated over these resolved values rather than the submitted ones.
    struct Resolved {
        stream_id: Uuid,
        owner: ResolvedOwner,
        measurement_type: String,
    }
    let resolved: Vec<Resolved> = payload
        .readings
        .iter()
        .map(|r| {
            let stream_id = stream_cache[&(r.site_id, r.parameter_id)];
            let owner = if r.sensor_id.is_none() {
                owner_map
                    .get(&(r.site_id, r.parameter_id, r.time))
                    .cloned()
                    .unwrap_or_default()
            } else {
                ResolvedOwner::default()
            };
            let measurement_type =
                crate::routes::private::readings::service::resolve_measurement_type(
                    r.measurement_type.as_deref(),
                    stream_defaults.get(&stream_id).and_then(|d| d.as_deref()),
                    batch_instrument(r.sensor_id, owner.sensor_id, None),
                    &sensor_types,
                );
            Resolved {
                stream_id,
                owner,
                measurement_type,
            }
        })
        .collect();

    // A caller-supplied standard curve is held to the same rule as a grab entry: the reading must
    // be that instrument's own spot measurement, and the corrected value is computed here from the
    // curve rather than taken from the request.
    let claims: Vec<CurveClaim<'_>> = payload
        .readings
        .iter()
        .zip(&resolved)
        .filter_map(|(r, res)| {
            r.standard_curve_id.map(|id| CurveClaim {
                standard_curve_id: id,
                sensor_id: batch_instrument(r.sensor_id, res.owner.sensor_id, None),
                measurement_type: &res.measurement_type,
            })
        })
        .collect();
    let standard_curve_models = admit_standard_curves(&state.db, &claims).await?;

    // The base calibrations those rows sit on, so the stored value is the one the pair of recorded
    // curves produces: instrument correction first, hand-picked curve on its result. Rows whose
    // caller supplied no value need their base too: stamping the id while leaving the value
    // uncorrected would claim a calibration the number never went through.
    let base_calibrations: HashMap<Uuid, sensor_calibrations::service::Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .zip(&resolved)
            .filter(|(r, _)| r.standard_curve_id.is_some() || r.calibrated_value.is_none())
            .filter_map(|(r, res)| r.calibration_id.or(res.owner.calibration_id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            HashMap::new()
        } else {
            sensor_calibrations::Entity::find()
                .filter(sensor_calibrations::Column::Id.is_in(ids))
                .all(&state.db)
                .await?
                .into_iter()
                .map(|c| {
                    (
                        c.id,
                        sensor_calibrations::service::Curve {
                            id: c.id,
                            slope: c.slope,
                            intercept: c.intercept,
                        },
                    )
                })
                .collect()
        }
    };

    // The cadence a reading is stored under is resolved, not declared, so the replicate index is
    // judged against the resolved value rather than the request's.
    for (r, res) in payload.readings.iter().zip(&resolved) {
        crate::routes::private::readings::service::admission::admit_replicate_index(
            Some(res.measurement_type.as_str()),
            r.replicate_index.unwrap_or(0),
        )?;
    }

    let models: Vec<readings::ActiveModel> = payload
        .readings
        .into_iter()
        .zip(resolved)
        .map(|(r, res)| {
            let Resolved {
                stream_id,
                owner,
                measurement_type,
            } = res;
            let calibration_id = r.calibration_id.or(owner.calibration_id);
            let attributed = slot_exists
                .get(&(r.site_id, r.parameter_id))
                .copied()
                .unwrap_or(false);
            let standard = r.standard_curve_id.map(|id| {
                let c = &standard_curve_models[&id];
                sensor_calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            let base = calibration_id.and_then(|id| base_calibrations.get(&id).copied());
            // A curve claim is computed here; a caller-supplied value is kept as claimed; a row
            // with neither gets its resolved base applied, so the stamped calibration_id is always
            // a curve the stored value went through.
            let calibrated_value = match (standard, r.calibrated_value) {
                (Some(curve), _) => Some(sensor_calibrations::service::apply_curves(
                    r.raw_value,
                    base,
                    Some(curve),
                )),
                (None, Some(value)) => Some(value),
                (None, None) => base.map(|c| c.apply(r.raw_value)),
            };
            readings::ActiveModel {
                standard_curve_id: Set(r.standard_curve_id),
                provenance_kind: Set(Some("batch".to_string())),
                site_id: Set(attributed.then_some(r.site_id)),
                parameter_id: Set(attributed.then_some(r.parameter_id)),
                calibrated_value: Set(calibrated_value),
                sensor_id: Set(batch_instrument(
                    r.sensor_id,
                    owner.sensor_id,
                    stream_sensors.get(&stream_id).copied(),
                )),
                calibration_id: Set(calibration_id),
                deployment_id: Set(r.deployment_id.or(owner.deployment_id)),
                measurement_type: Set(Some(measurement_type)),
                sample_id: Set(r.sample_id),
                ..readings::new(
                    stream_id,
                    r.time.into(),
                    r.replicate_index.unwrap_or(0),
                    r.raw_value,
                )
            }
        })
        .collect();

    let total = models.len();
    let inserted: usize;
    let overwritten: usize;
    let conflict = payload.conflict;

    // One guarded transaction for every chunk: an overwrite rewrites stored rows, which on a
    // hypertable older than the compression policy means decompressing them, and the per-statement
    // cap refuses that outside a transaction that lifts it. Chunking stays, so the statement size
    // is bounded; the transaction is what makes a part-written correction impossible.
    let actor = crate::common::actor::label(&auth);
    // A correction is recorded as what made it. A batch is usually a person or a script acting for
    // one; a sync service reaching the same route is recorded as sync.
    let origin = auth.origin();
    let touched_events;
    (inserted, overwritten, touched_events) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let mut inserted = 0usize;
        let mut overwritten = 0usize;
        for chunk in models.chunks(BATCH_SIZE) {
            // In overwrite mode `rows_affected` counts both inserts and updates, so the count of
            // keys already present (looked up before the write) separates the inserts. Only the
            // rows whose value the write changes count as overwritten, the same definition the
            // CSV import reports; those are exactly the value corrections recorded (ADR 0008).
            let (pre_existing, changed) = if conflict == ConflictMode::Overwrite {
                let corrections =
                    crate::routes::private::readings::service::record_value_corrections(
                        txn,
                        chunk,
                        &actor,
                        origin,
                    )
                    .await?;
                (
                    count_existing(txn, chunk).await?,
                    usize::try_from(corrections.rows).unwrap_or(usize::MAX),
                )
            } else {
                (0, 0)
            };

            match readings::Entity::insert_many(chunk.to_vec())
                .on_conflict(readings_on_conflict(conflict))
                .exec_without_returning(txn)
                .await
            {
                Ok(rows) => {
                    let affected = rows as usize;
                    inserted += affected.saturating_sub(pre_existing);
                    overwritten += changed;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("None of the records") {
                        // All duplicates in this chunk
                    } else {
                        tracing::warn!(error = %e, batch_size = chunk.len(), "Failed to insert reading batch");
                        return Err(crate::error::AppError::Database(e));
                    }
                }
            }
        }
        // A hand-picked curve on a batch row is a claim, recorded once (ADR 0008).
        crate::routes::private::readings::service::record_curve_claims(
            txn,
            &models,
            &actor,
            origin,
        )
        .await?;

        // Attributed spot rows this batch landed belong to collection events (D7). A batch caller
        // is a person or a script acting for one, so the events are manual.
        let spot_times: Vec<chrono::DateTime<chrono::FixedOffset>> = models
            .iter()
            .filter(|m| {
                matches!(&m.measurement_type,
                    sea_orm::ActiveValue::Set(Some(t)) if t == crate::routes::private::readings::service::SPOT)
            })
            .map(|m| *m.time.as_ref())
            .collect();
        let mut touched_events = Vec::new();
        if let (Some(first), Some(last)) = (
            spot_times.iter().min().copied(),
            spot_times.iter().max().copied(),
        ) {
            let mut stream_ids: Vec<Uuid> =
                models.iter().map(|m| *m.stream_id.as_ref()).collect();
            stream_ids.sort_unstable();
            stream_ids.dedup();
            let window = || {
                use crate::routes::private::collection_events::flows::row;
                use crate::routes::private::readings::models::Column;
                use sea_orm::ExprTrait as _;
                sea_orm::Condition::all()
                    .add(row(Column::StreamId).is_in(stream_ids.clone()))
                    .add(row(Column::Time).gte(first))
                    .add(row(Column::Time).lte(last))
            };
            crate::routes::private::collection_events::service::attach_collection_events(
                txn,
                window(),
                crate::routes::private::collection_events::service::EventSource::Manual,
            )
            .await?;
            // A replicate landing beside one already stored makes the instant a group, whichever
            // path wrote either row, so the batch goes through the one materialiser too.
            crate::routes::private::readings::service::materialise_samples(
                txn,
                window(),
            )
            .await?;
            let mut instants = spot_times.clone();
            instants.sort_unstable();
            instants.dedup();
            touched_events =
                crate::routes::private::collection_events::flows::touched_events(
                    txn,
                    {
                        use crate::routes::private::collection_events::flows::row;
                        use crate::routes::private::readings::models::Column;
                        use sea_orm::ExprTrait as _;
                        sea_orm::Condition::all()
                            .add(row(Column::StreamId).is_in(stream_ids))
                            .add(row(Column::Time).is_in(instants))
                    },
                )
                .await?;
        }

        Ok((inserted, overwritten, touched_events))
    })
    .await?;

    // The values have landed; the calculations that read them run without anyone asking
    // (ADR 0007). What they are is reported back alongside the counts.
    let calculations = {
        let mut touched: Vec<Uuid> = touched_events
            .iter()
            .filter(|e| {
                crate::routes::private::collection_events::service::chain_may_run(&e.source)
            })
            .flat_map(|e| e.parameter_ids.iter().copied())
            .collect();
        touched.sort_unstable();
        touched.dedup();
        crate::routes::private::tools::service::calculations_fed_by(&state.db, &touched).await?
    };

    // An overwrite replaces the measurement, not the correction: the write keeps the stored curve
    // references, so the value is recomputed from exactly those curves, the same as the CSV
    // overwrite path. Without this the row carries a value its recorded curves did not produce
    // until the janitor sweep catches it.
    if conflict == ConflictMode::Overwrite && overwritten > 0 {
        let mut stream_ids: Vec<Uuid> = models.iter().map(|m| *m.stream_id.as_ref()).collect();
        stream_ids.sort_unstable();
        stream_ids.dedup();
        let times: Vec<chrono::DateTime<chrono::FixedOffset>> =
            models.iter().map(|m| *m.time.as_ref()).collect();
        if let (Some(first), Some(last)) = (times.iter().min(), times.iter().max()) {
            sensor_calibrations::service::recompose_from_own_curves_guarded(
                &state.db,
                sea_orm::sea_query::Expr::cust("TRUE"),
                "r.stream_id = ANY($1) AND r.time >= $2 AND r.time <= $3",
                vec![stream_ids.into(), (*first).into(), (*last).into()],
            )
            .await?;
        }
    }

    tracing::debug!(
        total,
        inserted,
        overwritten,
        "Batch readings insert complete"
    );

    let earliest = site_timestamps_for_derived
        .values()
        .flatten()
        .min()
        .copied();
    let latest = site_timestamps_for_derived
        .values()
        .flatten()
        .max()
        .copied();

    // Auto-compute derived values for affected sites, tracked as a job. Spawn-guard: keep only
    // sites with an active derived parameter, others would compute nothing.
    if inserted > 0 || overwritten > 0 {
        let mut derived_sites: HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>> = HashMap::new();
        for (site_id, timestamps) in &site_timestamps_for_derived {
            if crate::routes::private::derived_parameters::flows::site_has_active_derived(
                &state.db, *site_id,
            )
            .await
            .unwrap_or(true)
            {
                derived_sites.insert(*site_id, timestamps.clone());
            }
        }
        if !derived_sites.is_empty() {
            let site_timestamps: Vec<serde_json::Value> = derived_sites
                .iter()
                .map(|(site_id, timestamps)| {
                    serde_json::json!({ "site_id": site_id, "timestamps": timestamps })
                })
                .collect();
            crate::routes::private::reprocessing_jobs::service::enqueue(
                &state.db,
                "batch_derived",
                None,
                None,
                &serde_json::json!({ "site_timestamps": site_timestamps }),
                None,
            )
            .await?;
        }
    }

    // An overwrite replaced values the rollups have already materialised; an insert appended past
    // them, which the next scheduled refresh covers. Best-effort: the rows are committed, so a
    // refresh losing a lock to the janitor must not report a write that happened as one that did
    // not. Episodes go to the `alarm_backfill` job because a batch can span a long window.
    let written = crate::routes::private::readings::service::Written::new(
        u64::try_from(inserted + overwritten).unwrap_or(u64::MAX),
    )
    .over(earliest.zip(latest))
    .at(stream_cache
        .iter()
        .map(|((site_id, parameter_id), stream_id)| {
            crate::routes::private::readings::service::Slot::paired(*site_id, *parameter_id)
                .through(*stream_id)
        })
        .collect())
    .touching(touched_events);
    crate::routes::private::readings::service::run(
        &state,
        &written,
        &crate::routes::private::readings::service::Axes {
            cache: crate::routes::private::readings::service::Cache::Sites,
            refresh: if overwritten > 0 {
                crate::routes::private::readings::service::Refresh::Range { fatal: false }
            } else {
                crate::routes::private::readings::service::Refresh::Skip
            },
            announce: true,
            reconcile_alarms: true,
            episodes: crate::routes::private::readings::service::Episodes::Job,
            recompute_derived: false,
            writer: crate::routes::private::collection_events::flows::Writer::Person,
        },
        &crate::common::actor::label(&auth),
    )
    .await?;

    Ok(Json(BatchReadingsResponse {
        inserted,
        overwritten,
        calculations,
    }))
}

/// Stream-based data ingestion. Inserts readings keyed by `stream_id`. If the stream is
/// paired to a `site_parameter`, readings are stamped with `site_id`/`parameter_id`, and each is
/// corrected by whichever of the owning instrument's curves covers its own time; a reading no curve
/// covers is stored uncorrected. Unpaired streams insert with `site_id = NULL` (and won't show up
/// in continuous aggregates until paired). Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/ingest",
    request_body = IngestReadingsRequest,
    responses(
        (status = 200, description = "Inserted count and pairing state. Inadmissible readings (out-of-window timestamp, non-finite value, unknown measurement_type, unknown calibration_id) are skipped and counted in `skipped`, not refused", body = IngestResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_readings(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    IsSyncService(is_sync_service): IsSyncService,
    Json(mut payload): Json<IngestReadingsRequest>,
) -> AppResult<Json<IngestResponse>> {
    if payload.overwrite && !is_sync_service {
        return Err(AppError::Forbidden(
            "overwrite is restricted to sync services".to_string(),
        ));
    }
    if payload.collection && !is_sync_service {
        return Err(AppError::Forbidden(
            "collection is restricted to sync services; grab entry goes through /grab_samples"
                .to_string(),
        ));
    }
    if payload.audit.is_some() && !is_sync_service {
        return Err(AppError::Forbidden(
            "audit is restricted to sync services".to_string(),
        ));
    }
    if payload.window.is_some() && !is_sync_service {
        return Err(AppError::Forbidden(
            "window is restricted to sync services".to_string(),
        ));
    }
    if let Some(window) = &payload.window
        && window.from >= window.to
    {
        return Err(AppError::BadRequest(
            "window.from must be before window.to".to_string(),
        ));
    }

    // An empty payload without a claim is nothing to do. With a window it is a claim the source
    // holds nothing there, which the diff must judge against what is stored.
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(IngestResponse {
            inserted: 0,
            skipped: 0,
            skipped_reasons: Vec::new(),
            changed: 0,
            proposed: 0,
            withdrawn: 0,
            unchanged: 0,
            retained: 0,
            accepted_window: None,
            stream_id: payload.stream_id,
            paired: false,
        }));
    }

    let db = &state.db;

    // Look up stream to get pairing info
    let stream = data_streams::Entity::find_by_id(payload.stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let (site_id, parameter_id) = resolve_stream_slot(db, stream.site_parameter_id).await?;
    let paired = site_id.is_some();

    // Withdrawal is confined to spot rows by a database CHECK; the continuous aggregates exclude
    // spot, which is what keeps a retraction structurally unreachable by a rollup. A window on a
    // non-spot stream is therefore refused rather than half-honoured.
    if payload.window.is_some() && stream.measurement_type.as_deref() != Some("spot") {
        return Err(AppError::BadRequest(
            "A completeness window is only accepted for streams declared measurement_type \
             'spot'; continuous sources stay append-only"
                .to_string(),
        ));
    }

    // A project-scoped token may only ingest into a stream paired to a site within its project.
    // An unpaired stream has no project, so a scoped token is rejected outright.
    enforce_ingest_scope(&state.db, &scope, site_id).await?;

    // Admission is per reading here, not per request: the caller replays from `last_data_time`,
    // which only advances on success, so refusing a batch for one bad row stalls the stream.
    // The cursor is taken from the survivors, so a future timestamp cannot latch it.
    // Tallied by kind because the per-reading messages carry the offending value and cannot group.
    let submitted = payload.readings.len();
    let mut counts: Vec<(
        crate::routes::private::readings::service::admission::RejectionKind,
        usize,
    )> = Vec::new();
    // Keys the funnel refused, remembered so the diff classifies them `retained` rather than
    // withdrawn: a key the request carried and the funnel refused is not absent at source.
    let mut rejected_keys: std::collections::HashSet<(chrono::DateTime<Utc>, i16)> =
        std::collections::HashSet::new();
    let track_rejections = payload.window.is_some();
    let now = Utc::now();
    payload.readings.retain(|r| {
        match crate::routes::private::readings::service::admission::rejection_kind_at(
            now,
            r.time,
            r.raw_value,
            r.measurement_type.as_deref(),
        ) {
            None => true,
            Some(kind) => {
                record_rejection(&mut counts, kind);
                if track_rejections {
                    rejected_keys.insert((r.time, r.replicate_index));
                }
                false
            }
        }
    });

    // Under a completeness claim, two payload rows at one key would classify one submitted row
    // twice. The last occurrence wins (backends emit source-id order, so last is deterministic);
    // the losers are counted so the receipt arithmetic closes.
    if payload.window.is_some() {
        let mut seen: std::collections::HashSet<(chrono::DateTime<Utc>, i16)> =
            std::collections::HashSet::new();
        let mut keep = vec![false; payload.readings.len()];
        for (i, r) in payload.readings.iter().enumerate().rev() {
            keep[i] = seen.insert((r.time, r.replicate_index));
        }
        if keep.iter().any(|k| !k) {
            let mut i = 0;
            payload.readings.retain(|_| {
                let kept = keep[i];
                i += 1;
                if !kept {
                    record_rejection(&mut counts, crate::routes::private::readings::service::admission::RejectionKind::DuplicateKey);
                }
                kept
            });
        }
    }

    // Everything downstream reads `payload.readings`, so a batch that is entirely inadmissible has
    // no work left: return before the window-resolution queries rather than run them over nothing.
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    // Window-aware attribution: resolve calibration/deployment/site per reading TIME from the
    // sensor's windows, agreeing with reprocess_sensor_readings. The stream's frozen sensor_id is the
    // owner; cal/deployment/site come from whichever window covers each timestamp.
    let resolved = if let Some(stream_sensor) = stream.sensor_id {
        let times: Vec<chrono::DateTime<Utc>> = payload.readings.iter().map(|r| r.time).collect();
        resolve_windows_for_times(db, stream_sensor, None, parameter_id, &times).await?
    } else {
        std::collections::HashMap::new()
    };

    // Fallback when the stream carries no frozen sensor: attribute by the (site, parameter)
    // deployment timeline so readings still land owned when a deployment covers their time.
    let slot_owner = match (stream.sensor_id, site_id, parameter_id) {
        (None, Some(s), Some(p)) => {
            let times: Vec<chrono::DateTime<Utc>> =
                payload.readings.iter().map(|r| r.time).collect();
            resolve_slot_owner_for_times(db, s, p, &times).await?
        }
        _ => std::collections::HashMap::new(),
    };

    // The curve covering each reading's own time, ranked by the one resolver the set-based
    // reprocess UPDATEs use. A value stored here and the value a later reprocess recomputes are
    // therefore the same number, so a reading is correct the moment it lands rather than only
    // after the next reprocess.
    let curves = {
        let requests: Vec<(Uuid, Option<Uuid>, chrono::DateTime<Utc>)> = payload
            .readings
            .iter()
            .filter_map(|r| {
                let owner = slot_owner.get(&r.time);
                let sensor = ingest_instrument(
                    r.sensor_id,
                    stream.sensor_id,
                    owner.and_then(|o| o.sensor_id),
                )?;
                Some((sensor, parameter_id, r.time))
            })
            .collect();
        resolver::resolve_many(db, &requests).await?
    };

    // A caller that names its own calibration is taken at its word about which curve applies, but
    // the stored value is still computed from that curve's coefficients rather than trusted or left
    // empty. Reference and value come from one curve on every reading, so nothing can store a
    // correction its `calibration_id` did not produce.
    let declared_curves: HashMap<Uuid, Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.calibration_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            HashMap::new()
        } else {
            sensor_calibrations::Entity::find()
                .filter(sensor_calibrations::Column::Id.is_in(ids))
                .all(db)
                .await?
                .into_iter()
                .map(|c| {
                    (
                        c.id,
                        Curve {
                            id: c.id,
                            slope: c.slope,
                            intercept: c.intercept,
                        },
                    )
                })
                .collect()
        }
    };

    // A deleted calibration never reappears, so refusing the request here would stall the cursor
    // permanently. Second pass because deciding it needs the calibration rows queried above.
    payload.readings.retain(|r| match r.calibration_id {
        Some(id) if !declared_curves.contains_key(&id) => {
            record_rejection(&mut counts, crate::routes::private::readings::service::admission::RejectionKind::UnknownCalibration);
            if track_rejections {
                rejected_keys.insert((r.time, r.replicate_index));
            }
            false
        }
        _ => true,
    });
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    // Sensor-frequency defaults for every sensor a reading could resolve to (explicit, stream, or
    // slot owner), fetched in one query. Applied when neither the reading nor the stream declares
    // a measurement_type.
    let sensor_types = {
        let mut candidate_sensors: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(stream.sensor_id)
            .chain(slot_owner.values().filter_map(|o| o.sensor_id))
            .collect();
        candidate_sensors.sort_unstable();
        candidate_sensors.dedup();
        crate::routes::private::readings::service::measurement_types_for_sensors(
            db,
            &candidate_sensors,
        )
        .await?
    };

    // Replicates belong to a spot instant. The declared cadence is not the stored one, so this is
    // judged against the resolved value; a row that fails it would be stored at an index every
    // continuous reader filters out, ie. served nowhere.
    payload.readings.retain(|r| {
        if r.replicate_index == 0 {
            return true;
        }
        let sensor_id = ingest_instrument(
            r.sensor_id,
            stream.sensor_id,
            slot_owner.get(&r.time).and_then(|o| o.sensor_id),
        );
        let resolved = crate::routes::private::readings::service::resolve_measurement_type(
            r.measurement_type.as_deref(),
            stream.measurement_type.as_deref(),
            sensor_id,
            &sensor_types,
        );
        if resolved == crate::routes::private::readings::service::SPOT {
            return true;
        }
        record_rejection(
            &mut counts,
            crate::routes::private::readings::service::admission::RejectionKind::ReplicateIndexOnNonSpot,
        );
        if track_rejections {
            rejected_keys.insert((r.time, r.replicate_index));
        }
        false
    });

    // Standard curve claims, held to the grab rules (fitted on the reading's instrument, spot
    // measurement). An inadmissible claim is stripped, never the reading: the value is stored
    // uncorrected and the claim lands in the review queue as a `curve_claim_stripped` hold, so
    // a mis-homed curve or a mis-declared stream instrument costs a correction, not data.
    let mut stripped_claims: HashMap<chrono::DateTime<Utc>, Vec<serde_json::Value>> =
        HashMap::new();
    let standard_curves_by_id: HashMap<Uuid, Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.standard_curve_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            HashMap::new()
        } else {
            let rows = standard_curves::Entity::find()
                .filter(standard_curves::Column::Id.is_in(ids))
                .all(db)
                .await?;
            let by_id: HashMap<Uuid, &standard_curves::Model> =
                rows.iter().map(|c| (c.id, c)).collect();
            for r in payload.readings.iter_mut() {
                let Some(id) = r.standard_curve_id else {
                    continue;
                };
                let sensor_id = ingest_instrument(
                    r.sensor_id,
                    stream.sensor_id,
                    slot_owner.get(&r.time).and_then(|o| o.sensor_id),
                );
                let measurement_type =
                    crate::routes::private::readings::service::resolve_measurement_type(
                        r.measurement_type.as_deref(),
                        stream.measurement_type.as_deref(),
                        sensor_id,
                        &sensor_types,
                    );
                let reason = match by_id.get(&id) {
                    None => Some("names no standard curve"),
                    Some(c) if Some(c.sensor_id) != sensor_id => {
                        Some("fitted on a different instrument than the reading's")
                    }
                    Some(_)
                        if measurement_type != crate::routes::private::readings::service::SPOT =>
                    {
                        Some("the reading is not a spot measurement")
                    }
                    Some(_) => None,
                };
                if let Some(reason) = reason {
                    r.standard_curve_id = None;
                    stripped_claims
                        .entry(r.time)
                        .or_default()
                        .push(serde_json::json!({
                            "replicate_index": r.replicate_index,
                            "standard_curve_id": id,
                            "curve_instrument_id": by_id.get(&id).map(|c| c.sensor_id),
                            "reading_instrument_id": sensor_id,
                            "reason": reason,
                        }));
                }
            }
            rows.into_iter()
                .map(|c| {
                    (
                        c.id,
                        Curve {
                            id: c.id,
                            slope: c.slope,
                            intercept: c.intercept,
                        },
                    )
                })
                .collect()
        }
    };
    if payload.readings.is_empty() && payload.window.is_none() {
        return Ok(Json(ingest_outcome(
            payload.stream_id,
            paired,
            submitted,
            0,
            &counts,
        )));
    }

    // Build reading models
    let models: Vec<readings::ActiveModel> = payload
        .readings
        .iter()
        .map(|r| {
            let slot = resolved.get(&r.time);
            let owner = slot_owner.get(&r.time);
            let sensor_id = ingest_instrument(
                r.sensor_id,
                stream.sensor_id,
                owner.and_then(|o| o.sensor_id),
            );
            // A reading on an unpaired stream is staged: nothing has said which instrument
            // measured it, and the channel's registration default is not that answer (B223). What
            // the caller declared is kept, because that is a claim somebody made; what would be
            // derived here waits for the pairing, which stamps site, parameter and instrument
            // together. The cadence still reads the channel's instrument above: which device a
            // feed comes through is a fact about the channel, not a claim about the measurement.
            let stored_sensor_id = if paired { sensor_id } else { r.sensor_id };
            // The one curve this reading is stored against: the caller's if it named one, else
            // whichever window covers its own time. Both `calibration_id` and `calibrated_value`
            // are derived from it, so a later reprocess re-resolving the same windows recomputes
            // what is already stored instead of disagreeing with it. The deployment-derived slot
            // and slot owner resolve calibrations by time alone, blind to the reading's parameter,
            // so they are not consulted for a curve; they still answer for the deployment.
            //
            // No curve at all: `calibrated_value` stays NULL, which is what a reprocess over the
            // same windows would leave too. Consumers read COALESCE(calibrated_value, raw_value),
            // so the raw value is still what is served.
            let curve = r
                .calibration_id
                .and_then(|id| declared_curves.get(&id).copied())
                .or_else(|| {
                    sensor_id.and_then(|s| curves.get(&(s, parameter_id, r.time)).copied())
                });
            // A hand-picked lab curve composes on top of whatever base calibration covers the
            // reading, as on /readings/batch and /grab_samples: instrument correction first, the
            // curve on its result. Reference and value still move together.
            let standard = r
                .standard_curve_id
                .and_then(|id| standard_curves_by_id.get(&id).copied());
            readings::ActiveModel {
                standard_curve_id: Set(standard.map(|c| c.id)),
                provenance_kind: Set(Some("sync".to_string())),
                // Pairing is what attributes a reading to a site, so an unpaired stream stores its
                // readings unattributed even when a deployment of its sensor covers their time.
                // Within a paired stream the deployment decides which site, since a sensor can move
                // between sites while the stream keeps pointing at one slot.
                site_id: Set(site_id.map(|paired| slot.and_then(|s| s.site_id).unwrap_or(paired))),
                parameter_id: Set(parameter_id),
                calibrated_value: Set(match (curve.filter(|_| paired), standard) {
                    (None, None) => None,
                    (base, standard) => Some(apply_curves(r.raw_value, base, standard)),
                }),
                sensor_id: Set(stored_sensor_id),
                calibration_id: Set(if paired {
                    curve.map(|c| c.id)
                } else {
                    r.calibration_id
                }),
                deployment_id: Set(if paired {
                    r.deployment_id
                        .or_else(|| slot.and_then(|s| s.deployment_id))
                        .or_else(|| owner.and_then(|o| o.deployment_id))
                } else {
                    r.deployment_id
                }),
                measurement_type: Set(Some(
                    crate::routes::private::readings::service::resolve_measurement_type(
                        r.measurement_type.as_deref(),
                        stream.measurement_type.as_deref(),
                        sensor_id,
                        &sensor_types,
                    ),
                )),
                ..readings::new(
                    payload.stream_id,
                    r.time.into(),
                    r.replicate_index,
                    r.raw_value,
                )
            }
        })
        .collect();

    let total = models.len();

    // Spot readings on a paired stream can form samples: replicate groups sharing an instant get
    // a `samples` row (mean/stdev/n maintained by the readings triggers), which is what the
    // serving paths and the UI whiskers read. A group needs no index-0 row: `replicate_index` is
    // the source's column position and is never renumbered. Scoped to this batch's time window;
    // identity of a collection event is (site, parameter, instant), so pre-existing rows at the
    // slot join the group. Unpaired streams skip this (site_id is NULL, the pairing backfill
    // materialises).
    let sample_window = if paired {
        let spot_times = models
            .iter()
            .filter_map(|m| match (&m.measurement_type, &m.time) {
                (sea_orm::ActiveValue::Set(Some(t)), sea_orm::ActiveValue::Set(time))
                    if t == crate::routes::private::readings::service::SPOT =>
                {
                    Some(time.with_timezone(&Utc))
                }
                _ => None,
            });
        spot_times.clone().min().zip(spot_times.max())
    } else {
        None
    };

    // Overwrite updates existing rows, the windowed diff corrects and withdraws, and the sample
    // stamping UPDATE reaches back-dated groups; all can touch compressed chunks, so they run
    // inside one transaction with the decompression cap lifted, like every other back-dated
    // write path. The diff, the writes and the receipt commit together or not at all.
    let mut diff_outcome: Option<crate::routes::private::readings::service::DiffOutcome> = None;
    let mut touched_visits: Vec<crate::routes::private::collection_events::flows::TouchedEvent> =
        Vec::new();
    let audited =
        payload.audit.as_deref().is_some_and(|a| !a.is_empty()) || !stripped_claims.is_empty();
    let inserted = if payload.overwrite
        || sample_window.is_some()
        || payload.window.is_some()
        || audited
    {
        let actor = crate::common::actor::label(&auth);
        let (n, diff, touched_events) = crate::common::bulk_write::guarded(db, async |txn| {
                let diff = match &payload.window {
                    Some(window) => {
                        let admitted: Vec<crate::routes::private::readings::service::AdmittedRow> = payload
                            .readings
                            .iter()
                            .map(|r| {
                                (
                                    (r.time, r.replicate_index),
                                    r.raw_value,
                                    r.standard_curve_id,
                                )
                            })
                            .collect();
                        Some(
                            crate::routes::private::readings::service::run_windowed_diff(
                                txn,
                                payload.stream_id,
                                window,
                                &admitted,
                                &rejected_keys,
                                &actor,
                                paired,
                            )
                            .await?,
                        )
                    }
                    None => None,
                };
                // A windowed pass writes only rows the store does not hold, so nothing it writes
                // has a value to replace. An overwrite still replaces, which is what it is for.
                let replace = if payload.overwrite {
                    Replace::ValuesAndAttribution
                } else {
                    Replace::Nothing
                };
                // Under a diff only classified-new rows are written: an unchanged row re-written
                // with identical values is a hypertable write, WAL and an upsert count that reads
                // as effect, all for nothing, and a changed one is a proposal (Q84). An overwrite
                // is exempt: it exists to rewrite attribution, which value equality cannot see.
                let filtered: Vec<readings::ActiveModel>;
                let to_write: &[readings::ActiveModel] = match &diff {
                    Some(d) if !payload.overwrite => {
                        filtered = models
                            .iter()
                            .zip(payload.readings.iter())
                            .filter(|(_, r)| d.write_keys.contains(&(r.time, r.replicate_index)))
                            .map(|(m, _)| m.clone())
                            .collect();
                        &filtered
                    }
                    _ => &models,
                };
                // A correction is a decision of sync origin (ADR 0008), recorded before the
                // upsert so the value it replaces is what the record holds; a hand-picked curve
                // the source names is a claim, recorded once.
                if replace != Replace::Nothing {
                    crate::routes::private::readings::service::record_value_corrections(
                        txn,
                        to_write,
                        &actor,
                        crate::routes::private::readings::models::Origin::Sync,
                    )
                    .await?;
                }
                let n = insert_reading_chunks(txn, to_write, replace).await?;
                crate::routes::private::readings::service::record_curve_claims(
                    txn,
                    to_write,
                    &actor,
                    crate::routes::private::readings::models::Origin::Sync,
                )
                .await?;
                // A pass that wrote, withdrew or reinstated nothing left every group's content
                // as it stood; sample statistics and event attachment have nothing to recompute.
                // Overwrites recompute regardless: they may have moved attribution.
                let diff_touched = payload.overwrite
                    || diff
                        .as_ref()
                        .is_none_or(|d| d.new_rows + d.changed + d.withdrawn + d.reinstated > 0);
                let mut touched_events = Vec::new();
                if diff_touched && let Some((lo, hi)) = sample_window {
                    let window = || {
                        use crate::routes::private::readings::models::Column;
                        use sea_orm::ExprTrait as _;
                        sea_orm::Condition::all()
                            .add(flows::row(Column::StreamId).eq(payload.stream_id))
                            .add(flows::row(Column::Time).gte(
                                sea_orm::prelude::DateTimeWithTimeZone::from(lo),
                            ))
                            .add(flows::row(Column::Time).lte(
                                sea_orm::prelude::DateTimeWithTimeZone::from(hi),
                            ))
                    };
                    crate::routes::private::readings::service::materialise_samples(txn, window())
                        .await?;
                    // Each source row maps onto one collection event (D7). A sync service replaying
                    // a portal row writes a portal_sync event; any other writer is a person.
                    crate::routes::private::collection_events::service::attach_collection_events(
                        txn,
                        window(),
                        if is_sync_service {
                            crate::routes::private::collection_events::service::EventSource::PortalSync
                        } else {
                            crate::routes::private::collection_events::service::EventSource::Manual
                        },
                    )
                    .await?;
                    touched_events =
                        crate::routes::private::collection_events::flows::touched_events(
                            txn,
                            window(),
                        )
                        .await?;
                }
                // The audit judges what this transaction stored, so its hold transitions commit or
                // roll back with the writes. A braked pass withheld the corrections the claim
                // describes, so the stored groups are not what the source asserted; the holds stand.
                if let Some(audits) = payload.audit.as_deref().filter(|a| !a.is_empty())
                    && diff.as_ref().is_none_or(|d| !d.braked)
                {
                    run_replicate_audit(
                        txn,
                        payload.stream_id,
                        audits,
                        paired,
                    )
                    .await?;
                }
                // After the statistics audit on purpose: at one (stream, instant) key the later
                // upsert wins, and a stripped claim explains the disagreement the audit would
                // otherwise report bare.
                if !stripped_claims.is_empty() {
                    let hold_status = audit::status_for(paired);
                    for (time, claims) in &stripped_claims {
                        upsert_curve_claim_hold(txn, payload.stream_id, *time, claims, hold_status)
                            .await?;
                    }
                    tracing::warn!(
                        stream_id = %payload.stream_id,
                        instants = stripped_claims.len(),
                        "Inadmissible standard curve claims stripped; readings stored uncorrected and held for review"
                    );
                }
                if let (Some(window), Some(d)) = (&payload.window, &diff) {
                    let rejected_total: usize = counts.iter().map(|(_, n)| n).sum();
                    let rejected_json = serde_json::Value::Object(
                        counts
                            .iter()
                            .map(|(k, n)| (k.as_str().to_string(), serde_json::json!(n)))
                            .collect(),
                    );
                    crate::routes::private::readings::service::write_receipt(
                        txn,
                        payload.stream_id,
                        window,
                        submitted,
                        d,
                        rejected_total,
                        &rejected_json,
                    )
                    .await?;
                }
                Ok((n, diff, touched_events))
            })
            .await?;
        diff_outcome = diff;
        touched_visits = touched_events;
        n
    } else {
        insert_reading_chunks(db, &models, Replace::Nothing).await?
    };

    let diff_withdrawn = diff_outcome.as_ref().map_or(0, |d| d.withdrawn);
    let diff_reinstated = diff_outcome.as_ref().map_or(0, |d| d.reinstated);
    // What this pass did to served content, the gate every post-write side effect reads: a
    // withdrawal with `inserted == 0` still rewrote history. A classified-changed key moved
    // nothing here: it is a proposal until somebody accepts it, and the accept path does this
    // work for the rows it writes.
    let moved = u64::try_from(inserted + diff_withdrawn + diff_reinstated).unwrap_or(u64::MAX);
    let effect = moved > 0;

    let span = payload
        .readings
        .iter()
        .map(|r| r.time)
        .min()
        .zip(payload.readings.iter().map(|r| r.time).max());

    // A correction rewrites history that bounded queries may have cached and replaces values the
    // rollups have already materialised. The upsert leaves a hand-picked curve standing and this
    // correction resolved only a base, so the value is recomposed from whichever curves the row
    // ends up carrying, before the rollups read it back.
    let corrected = payload.overwrite && inserted > 0;
    if corrected
        && let Some((lo, hi)) = span
        && let Err(e) = sensor_calibrations::service::recompose_from_own_curves_guarded(
            &state.db,
            sea_orm::sea_query::Expr::cust("TRUE"),
            "r.stream_id = $1 AND r.time >= $2 AND r.time <= $3",
            vec![
                payload.stream_id.into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(lo).into(),
                sea_orm::prelude::DateTimeWithTimeZone::from(hi).into(),
            ],
        )
        .await
    {
        tracing::warn!(error = %e, "recompose after overwrite failed");
    }

    // Sample formation retroactively changes served historical points (the group's mean replaces
    // the lone value), and a withdrawal, a reinstatement or a correction rewrites served history
    // the same way, so those passes cannot be left to expire on TTL anywhere. A plain append is
    // confined to its own site.
    //
    // The refresh is best-effort, like the alarm reconstruction: the rows are committed and the
    // cursor is about to advance past them, so a refresh that loses a lock to the janitor must not
    // turn a successful write into a 500 that replays the same batch forever. The rollups converge
    // on the next scheduled refresh. Episodes are rebuilt inline rather than as a tracked job:
    // single ingest fires every sync cycle per stream and would spam `reprocessing_jobs`.
    let rewrote_history =
        corrected || sample_window.is_some() || payload.window.is_some() || diff_withdrawn > 0;
    let written = crate::routes::private::readings::service::Written::new(moved)
        .announced(u64::try_from(inserted).unwrap_or(u64::MAX))
        .over(span)
        .at(vec![crate::routes::private::readings::service::Slot {
            site_id,
            parameter_id,
            stream_id: Some(payload.stream_id),
        }])
        .touching(touched_visits);
    crate::routes::private::readings::service::run(
        &state,
        &written,
        &crate::routes::private::readings::service::Axes {
            cache: if rewrote_history {
                crate::routes::private::readings::service::Cache::All
            } else {
                crate::routes::private::readings::service::Cache::Sites
            },
            refresh: if corrected {
                crate::routes::private::readings::service::Refresh::Range { fatal: false }
            } else {
                crate::routes::private::readings::service::Refresh::Skip
            },
            announce: true,
            reconcile_alarms: true,
            episodes: crate::routes::private::readings::service::Episodes::Inline,
            recompute_derived: false,
            writer: crate::routes::private::collection_events::flows::Writer::Person,
        },
        &crate::common::actor::label(&auth),
    )
    .await?;

    // Update last_data_time on the stream. Every group is admitted (audit disagreements are
    // review records, not gates), so the cursor always advances to the batch's newest instant.
    // The same UPDATE persists the handshake digest of a cleanly applied windowed pass (no
    // brake, no holds, no rejections, no stripped curve claims): the claim the sync client
    // compares its next payload against to skip re-sending unchanged content. A braked or held
    // pass stores none, so those windows keep re-asserting until a person rules.
    let advance_cursor = payload
        .readings
        .iter()
        .map(|r| r.time)
        .max()
        .filter(|max_time| {
            stream
                .last_data_time
                .map(|t| *max_time > t.with_timezone(&Utc))
                .unwrap_or(true)
        });
    let rejected_total: usize = counts.iter().map(|(_, n)| n).sum();
    let clean_digest = payload
        .window
        .as_ref()
        .and_then(|w| w.content_digest.clone())
        .filter(|_| {
            rejected_total == 0
                && stripped_claims.is_empty()
                && diff_outcome
                    .as_ref()
                    .is_some_and(|d| !d.braked && d.holds_raised == 0 && d.proposals_awaiting == 0)
        });
    let digest_changed = clean_digest.is_some() && stream.last_window_digest != clean_digest;
    if advance_cursor.is_some() || digest_changed {
        let mut active: data_streams::ActiveModel = stream.into();
        if let Some(max_time) = advance_cursor {
            active.last_data_time = Set(Some(max_time.into()));
        }
        if digest_changed {
            active.last_window_digest = Set(clean_digest);
        }
        active.updated_at = Set(Utc::now().into());
        if let Err(e) = active.update(db).await {
            tracing::warn!(error = %e, "Failed to update stream sync state");
        }
    }

    // Auto-compute derived parameters for newly ingested timestamps (batched), tracked as a job.
    // Spawn-guard: skip entirely when the site has no active derived parameter, the job would
    // compute nothing, and this is the dominant source of empty `ingest_derived` jobs.
    if paired
        && effect
        && let Some(sid) = site_id
        && crate::routes::private::derived_parameters::flows::site_has_active_derived(db, sid)
            .await
            .unwrap_or(true)
    {
        // A withdrawn key is absent from the payload by construction, so the retracted instants
        // have to be unioned in or the derived output computed from a retracted input is never
        // revisited. Reinstated keys are in the same position from the other direction.
        let mut unique_timestamps: Vec<chrono::DateTime<Utc>> =
            payload.readings.iter().map(|r| r.time).collect();
        if let Some(d) = &diff_outcome {
            unique_timestamps.extend(d.withdrawn_keys.iter().map(|(t, _)| *t));
            unique_timestamps.extend(d.reinstated_keys.iter().map(|(t, _)| *t));
        }
        unique_timestamps.sort();
        unique_timestamps.dedup();
        let source_stream = payload.stream_id;

        crate::routes::private::reprocessing_jobs::service::enqueue(
            db,
            "ingest_derived",
            None,
            None,
            &serde_json::json!({
                "site_id": sid,
                "stream_id": source_stream,
                "timestamps": unique_timestamps,
            }),
            None,
        )
        .await?;
    }

    let mut outcome = ingest_outcome(payload.stream_id, paired, submitted, inserted, &counts);
    if let Some(d) = &diff_outcome {
        // Under a diff, `inserted` from the upsert counts updates too; the classification is the
        // accurate account.
        outcome.inserted = d.new_rows;
        outcome.changed = d.changed;
        outcome.proposed = d.proposed;
        outcome.withdrawn = d.withdrawn;
        outcome.unchanged = d.unchanged;
        outcome.retained = d.retained;
        outcome.accepted_window = payload.window.clone();
        if d.braked {
            tracing::warn!(stream_id = %payload.stream_id, changed = d.changed, withdrawn = d.withdrawn, "Windowed pass braked; corrections and withdrawals held for review");
        }
    }
    tracing::debug!(total, inserted = outcome.inserted, skipped = outcome.skipped, changed = outcome.changed, withdrawn = outcome.withdrawn, reinstated = diff_reinstated, stream_id = %payload.stream_id, paired, "Ingest complete");
    Ok(Json(outcome))
}

#[utoipa::path(
    post,
    path = "/api/ingest/status_events",
    request_body = IngestStatusEventsRequest,
    responses(
        (status = 200, description = "Inserted count and pairing state. Events outside the admissible timestamp window are skipped and counted in `skipped`", body = IngestStatusEventsResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "ingestion"
)]
pub async fn ingest_status_events(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Json(mut payload): Json<IngestStatusEventsRequest>,
) -> AppResult<Json<IngestStatusEventsResponse>> {
    if payload.events.is_empty() {
        return Ok(Json(IngestStatusEventsResponse {
            inserted: 0,
            skipped: 0,
            deduplicated: 0,
            stream_id: payload.stream_id,
            paired: false,
        }));
    }

    let db = &state.db;

    let stream = data_streams::Entity::find_by_id(payload.stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;

    let (site_id, parameter_id) = resolve_stream_slot(db, stream.site_parameter_id).await?;
    let paired = site_id.is_some();

    enforce_ingest_scope(&state.db, &scope, site_id).await?;

    // A status event carries no numeric value, so the timestamp bound is the whole of admission
    // for it. Applied per event and counted, as on `/ingest`: an unbounded timestamp would open a
    // hypertable chunk far outside the data range, and refusing the request would stall the
    // stream that sent it.
    let submitted = payload.events.len();
    let now = Utc::now();
    payload.events.retain(|e| {
        crate::routes::private::readings::service::admission::time_rejection_at(now, e.time)
            .is_none()
    });
    let skipped = submitted - payload.events.len();
    if skipped > 0 {
        tracing::warn!(
            stream_id = %payload.stream_id,
            skipped,
            submitted,
            "Skipped status events outside the admissible timestamp window"
        );
    }
    if payload.events.is_empty() {
        return Ok(Json(IngestStatusEventsResponse {
            inserted: 0,
            skipped,
            deduplicated: 0,
            stream_id: payload.stream_id,
            paired,
        }));
    }

    // A status equal to the stream's latest stored value (and any repeat inside the batch) says
    // nothing new: the series keeps its first value and its transitions, and stops accreting one
    // "still the same" row per poll. Events at or before the stored tip are backfill and insert
    // as before; the primary key already collapses exact duplicates.
    let tip = status_events::Entity::find()
        .select_only()
        .column(status_events::Column::Time)
        .column(status_events::Column::Value)
        .filter(status_events::Column::StreamId.eq(payload.stream_id))
        .order_by_desc(status_events::Column::Time)
        .into_model::<StatusTip>()
        .one(db)
        .await?;
    let tip_time: Option<chrono::DateTime<Utc>> = tip.as_ref().map(|t| t.time.with_timezone(&Utc));
    let mut last_value: Option<String> = tip.and_then(|t| t.value);
    payload.events.sort_by_key(|e| e.time);
    let before_dedup = payload.events.len();
    payload.events.retain(|e| {
        let after_tip = tip_time.is_none_or(|t| e.time > t);
        if after_tip && last_value.as_deref() == Some(e.value.as_str()) {
            return false;
        }
        if after_tip {
            last_value = Some(e.value.clone());
        }
        true
    });
    let deduplicated = before_dedup - payload.events.len();
    if payload.events.is_empty() {
        return Ok(Json(IngestStatusEventsResponse {
            inserted: 0,
            skipped,
            deduplicated,
            stream_id: payload.stream_id,
            paired,
        }));
    }

    let models: Vec<status_events::ActiveModel> = payload
        .events
        .iter()
        .map(|e| status_events::ActiveModel {
            stream_id: Set(payload.stream_id),
            time: Set(e.time.into()),
            site_id: Set(site_id),
            parameter_id: Set(parameter_id),
            value: Set(e.value.clone()),
            // The stream's own instrument is what reported the status, as it is for the readings
            // arm above; a client that names one is naming an instrument the stream does not own.
            sensor_id: Set(e.sensor_id.or(stream.sensor_id)),
        })
        .collect();

    let total = models.len();
    let inserted = status_events::service::insert_ignoring_duplicates(db, models).await?;

    tracing::debug!(total, inserted, skipped, deduplicated, stream_id = %payload.stream_id, paired, "Status events ingest complete");
    Ok(Json(IngestStatusEventsResponse {
        inserted,
        skipped,
        deduplicated,
        stream_id: payload.stream_id,
        paired,
    }))
}

/// Insert field-collected grab sample readings (manual measurements with replicate sets).
/// Each request creates one Sample aggregate per parameter and uses dedicated "grab_sample"
/// streams. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/grab_samples",
    request_body = GrabSampleRequest,
    responses(
        (status = 200, description = "Counts of inserted readings and created Sample rows", body = GrabSampleResponse),
        (status = 400, description = "Empty readings, a parameter the site does not carry, or other validation"),
    ),
    tag = "ingestion"
)]
pub async fn insert_grab_samples(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<GrabSampleRequest>,
) -> AppResult<Json<GrabSampleResponse>> {
    if payload.readings.is_empty() {
        return Err(AppError::BadRequest("No readings provided".to_string()));
    }

    // A project-scoped token may only write to a site within its project.
    enforce_project_scope_for_sites(&state.db, &scope, &[payload.site_id]).await?;

    for r in &payload.readings {
        crate::routes::private::readings::service::admission::admit(
            r.time,
            r.value,
            Some(GRAB_MEASUREMENT_TYPE),
        )?;
    }

    // Validate site exists
    let site = sites::Entity::find_by_id(payload.site_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {} not found", payload.site_id)))?;

    // Validate all parameter_ids exist for this site
    let param_ids: Vec<Uuid> = payload.readings.iter().map(|r| r.parameter_id).collect();
    let site_params = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site.id))
        .filter(site_parameters::Column::ParameterId.is_in(param_ids.clone()))
        .all(&state.db)
        .await?;

    let valid_param_ids: std::collections::HashSet<Uuid> =
        site_params.iter().map(|sp| sp.parameter_id).collect();
    let sp_lookup: HashMap<Uuid, Uuid> = site_params
        .iter()
        .map(|sp| (sp.parameter_id, sp.id))
        .collect();

    // What each slot declares measures it. A slot that declares nothing is undeclared, not a
    // reason to borrow another row's instrument.
    let slot_instruments: HashMap<Uuid, Uuid> = site_params
        .iter()
        .filter_map(|sp| sp.instrument_sensor_id.map(|sid| (sp.parameter_id, sid)))
        .collect();

    // A hand save is held to the slots the site carries: one landing on a slot the site does not
    // carry is refused rather than minting one, because a mint here would create the declaration
    // it is meant to be checked against (Q98, kept by Q193). A publishing run is the other case,
    // and mints its output slot where the site declares the inputs it read.
    for r in &payload.readings {
        if !valid_param_ids.contains(&r.parameter_id) {
            return Err(AppError::BadRequest(format!(
                "Parameter {} is not configured for site {}; add its parameter group to the site \
                 first (POST /api/sites/{}/parameter_groups)",
                r.parameter_id, site.name, site.id
            )));
        }
    }

    // A save that names a seasonal check is held to it: every (parameter, value) pair must have
    // been screened by exactly that check.
    if let Some(check_id) = payload.check_id {
        let pairs: Vec<(Uuid, f64)> = payload
            .readings
            .iter()
            .map(|r| (r.parameter_id, r.value))
            .collect();
        crate::routes::private::readings::service::validate_check_claim(
            &state.db, check_id, site.id, &pairs,
        )
        .await?;
    }

    // Replicate indices, both curves and the served value are computed before anything is
    // written, so the same numbers serve the dry-run preview, the conflict report and the write.
    let indices = assign_replicate_indices(&payload.readings)?;

    let provenance = resolve_tool_run_provenance(
        &state.db,
        payload.tool_run_id,
        site.id,
        &payload.readings,
        &crate::common::actor::label(&auth),
    )
    .await?;

    // What the operator picked, held to the same rule as a slot's declaration and a deployment:
    // a bookkeeping row records that nothing was declared, and a retired instrument is not in the
    // lab. The slot's own declaration is guarded where it is set, so only the request's pick is
    // checked here.
    let picked: Vec<Uuid> = payload
        .readings
        .iter()
        .filter_map(|r| r.sensor_id)
        .collect();
    crate::routes::private::sensors::service::require_measuring_instruments(
        &state.db,
        &picked,
        "named as what measured a grab sample",
    )
    .await?;

    // The chosen standard curves, admitted by the one rule every writer of `standard_curve_id`
    // uses. A grab is spot by construction, so the only claims this path can be refused for are an
    // unknown id, a curve fitted on another instrument, and a curve on a grab that names no
    // instrument at all.
    let claims: Vec<CurveClaim<'_>> = payload
        .readings
        .iter()
        .filter_map(|r| {
            r.standard_curve_id.map(|id| CurveClaim {
                standard_curve_id: id,
                sensor_id: declared_instrument(
                    r.sensor_id,
                    slot_instruments.get(&r.parameter_id).copied(),
                ),
                measurement_type: GRAB_MEASUREMENT_TYPE,
            })
        })
        .collect();
    let standard_curves = admit_standard_curves(&state.db, &claims).await?;

    // The base calibration covering each grab that names an instrument, ranked by the one resolver
    // the ingest and reprocess paths use. Resolving it here is what lets the row carry both the id
    // and the value that id produced: a stamped calibration the stored value was never corrected by
    // is provenance that reads as true and is not.
    let base_curves = {
        let requests: Vec<(Uuid, Option<Uuid>, chrono::DateTime<chrono::Utc>)> = payload
            .readings
            .iter()
            .filter_map(|r| {
                declared_instrument(r.sensor_id, slot_instruments.get(&r.parameter_id).copied())
                    .map(|sid| (sid, Some(r.parameter_id), r.time))
            })
            .collect();
        sensor_calibrations::resolver::resolve_many(&state.db, &requests).await?
    };

    let preview: Vec<GrabPreview> = payload
        .readings
        .iter()
        .zip(&indices)
        .map(|(r, &replicate_index)| {
            let base =
                declared_instrument(r.sensor_id, slot_instruments.get(&r.parameter_id).copied())
                    .and_then(|sid| base_curves.get(&(sid, Some(r.parameter_id), r.time)))
                    .copied();
            let standard = r.standard_curve_id.map(|cid| {
                let c = &standard_curves[&cid];
                sensor_calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            // Both corrections, in the one order the arithmetic is defined in: the instrument's
            // base calibration, then the operator's standard curve on that result. A grab that
            // resolves neither is stored uncorrected, and `calibrated_value` stays NULL so a null
            // still means "no curve was applied" rather than "a curve happened to be identity".
            let calibrated_value = (base.is_some() || standard.is_some())
                .then(|| sensor_calibrations::service::apply_curves(r.value, base, standard));
            let composed_equation = match (base, standard) {
                (Some(b), Some(s)) => Some(equation(
                    s.slope * b.slope,
                    s.slope * b.intercept + s.intercept,
                )),
                _ => None,
            };
            GrabPreview {
                parameter_id: r.parameter_id,
                time: r.time,
                replicate_index,
                raw_value: r.value,
                base_calibration: base.map(|c| CurveApplication {
                    id: c.id,
                    name: None,
                    slope: c.slope,
                    intercept: c.intercept,
                    equation: equation(c.slope, c.intercept),
                }),
                standard_curve: standard.map(|c| CurveApplication {
                    id: c.id,
                    name: standard_curves[&c.id].name.clone(),
                    slope: c.slope,
                    intercept: c.intercept,
                    equation: equation(c.slope, c.intercept),
                }),
                composed_equation,
                calibrated_value,
            }
        })
        .collect();

    let groups: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = {
        let mut seen = std::collections::HashSet::new();
        payload
            .readings
            .iter()
            .filter(|r| seen.insert((r.parameter_id, r.time)))
            .map(|r| (r.parameter_id, r.time))
            .collect()
    };
    let existing_groups = fetch_existing_groups(&state.db, payload.site_id, &groups).await?;

    // Which calculations this save feeds, known before anything is written. The chain's own save
    // is the recompute: it reports nothing and enqueues nothing.
    let run_source = tool_run_source(&state.db, payload.tool_run_id).await?;
    let writer = match run_source.as_deref() {
        Some("chain") => flows::Writer::Chain,
        _ => flows::Writer::Person,
    };
    // Where these values come from. A save that names no run is a person typing a number.
    let provenance_kind =
        crate::routes::private::readings::service::provenance_kind_for_run(run_source.as_deref());
    let calculations = if writer == flows::Writer::Chain {
        Vec::new()
    } else {
        let mut touched: Vec<Uuid> = payload.readings.iter().map(|r| r.parameter_id).collect();
        touched.sort_unstable();
        touched.dedup();
        crate::routes::private::tools::service::calculations_fed_by(&state.db, &touched).await?
    };

    if payload.dry_run {
        return Ok(Json(GrabSampleResponse {
            inserted: 0,
            samples_created: 0,
            created_sample_ids: vec![],
            dry_run: true,
            replaced: 0,
            kept_curated: 0,
            withdrawn: 0,
            preview,
            existing_groups,
            calculations,
        }));
    }

    // An intern enters measurements; a stored value is someone else's to change (Q21). A replace
    // that carries every stored replicate at the number it already holds changes none of them: the
    // entry grid posts the whole group, so a repeat typed into an empty cell arrives this way.
    let carried: Vec<(Uuid, chrono::DateTime<chrono::Utc>, i16, f64)> = payload
        .readings
        .iter()
        .zip(&preview)
        .map(|(r, p)| (r.parameter_id, r.time, p.replicate_index, r.value))
        .collect();
    if crate::routes::private::readings::service::entry_state(auth.highest_role().as_ref())
        .is_some()
        && payload.mode == Some(GrabWriteMode::Replace)
    {
        let moved = crate::routes::private::readings::service::stored_values_moved(
            &carried,
            &existing_groups,
        );
        if moved > 0 {
            return Err(AppError::Forbidden(format!(
                "An intern's entry cannot replace stored values; a manager rewrites them \
                 ({moved} stored replicate(s) would move)"
            )));
        }
    }
    // A value computed from a pending measurement is pending too (M62): the chain says so.
    let entry_state = crate::routes::private::readings::service::entry_kind(
        payload.pending_inputs,
        auth.highest_role().as_ref(),
    );

    // A save built from a stale read would retract a repeat added under it, so a client that says
    // what it read is refused when a group no longer holds that.
    if let Some(expected) = &payload.expected_replicates {
        let changed =
            crate::routes::private::readings::service::groups_changed(expected, &existing_groups);
        if !changed.is_empty() {
            let detail = serde_json::to_value(&existing_groups)
                .map_err(|e| AppError::Internal(e.to_string()))?;
            return Err(AppError::ConflictDetail {
                message: format!(
                    "{} replicate group(s) changed since they were read; re-read the visit and \
                     save again",
                    changed.len()
                ),
                detail,
            });
        }
    }

    if !existing_groups.is_empty() && payload.mode != Some(GrabWriteMode::Replace) {
        let detail = serde_json::to_value(&existing_groups)
            .map_err(|e| AppError::Internal(e.to_string()))?;
        return Err(AppError::ConflictDetail {
            message: format!(
                "{} replicate group(s) are already stored at the requested times; pass mode \
                 \"replace\" to rewrite them",
                existing_groups.len()
            ),
            detail,
        });
    }

    // Resolve stream_ids for each unique (site_id, parameter_id)
    let mut stream_cache: HashMap<Uuid, Uuid> = HashMap::new();
    for r in &payload.readings {
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(r.parameter_id)
        {
            let sp_id = sp_lookup.get(&r.parameter_id).copied();
            let stream_id =
                get_or_create_grab_stream(&state.db, payload.site_id, r.parameter_id, sp_id)
                    .await?;
            entry.insert(stream_id);
        }
    }

    // The channel instrument each parameter's grab stream carries, which a reading naming no
    // instrument of its own is attributed to. A hand-entered value still records what produced it.
    let stream_sensors: HashMap<Uuid, Uuid> = {
        let ids: Vec<Uuid> = stream_cache.values().copied().collect();
        let mut map = HashMap::new();
        let streams = crate::routes::private::data_streams::models::Entity::find()
            .filter(crate::routes::private::data_streams::models::Column::Id.is_in(ids))
            .filter(crate::routes::private::data_streams::models::Column::SensorId.is_not_null())
            .all(&state.db)
            .await?;
        for stream in streams {
            if let Some(sensor_id) = stream.sensor_id {
                map.insert(stream.id, sensor_id);
            }
        }
        map
    };

    // Window-aware attribution for grabs that name a sensor: which deployment the instrument was on
    // at the grab time (site-fixed to payload.site_id), instead of writing NULL. Grabs without a
    // sensor_id keep NULL deployment (manual lab values with no instrument).
    let grab_slots = {
        use crate::routes::private::sensors::models::ResolvedSlot;
        use crate::routes::private::sensors::service::resolve_windows_for_times;
        let mut times_by_channel: HashMap<(Uuid, Uuid), Vec<chrono::DateTime<chrono::Utc>>> =
            HashMap::new();
        for r in &payload.readings {
            if let Some(sid) =
                declared_instrument(r.sensor_id, slot_instruments.get(&r.parameter_id).copied())
            {
                times_by_channel
                    .entry((sid, r.parameter_id))
                    .or_default()
                    .push(r.time);
            }
        }
        let mut slots: HashMap<(Uuid, Uuid, chrono::DateTime<chrono::Utc>), ResolvedSlot> =
            HashMap::new();
        for ((sid, pid), times) in &times_by_channel {
            let resolved = resolve_windows_for_times(
                &state.db,
                *sid,
                Some(payload.site_id),
                Some(*pid),
                times,
            )
            .await
            .unwrap_or_default();
            for (t, slot) in resolved {
                slots.insert((*sid, *pid, t), slot);
            }
        }
        slots
    };

    // Per-parameter time windows for the alarm episode reconstruction below.
    let mut alarm_windows: HashMap<
        Uuid,
        (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
    > = HashMap::new();
    for r in &payload.readings {
        alarm_windows
            .entry(r.parameter_id)
            .and_modify(|(lo, hi)| {
                *lo = (*lo).min(r.time);
                *hi = (*hi).max(r.time);
            })
            .or_insert((r.time, r.time));
    }

    let total = payload.readings.len();

    // One guarded transaction: a replace on a compressed chunk must not fail on the cap, and the
    // delete, the sample rows and the insert land together or not at all.
    let actor = crate::common::actor::label(&auth);
    let (inserted, replaced, kept_curated, withdrawn, created_sample_ids, touched_events) =
        crate::common::bulk_write::guarded(&state.db, async |txn| {
            // A replace rewrites the rows carrying the group's label, notes, authorship and blob,
            // so they are captured first and kept on the rewritten rows wherever the request does
            // not carry its own.
            let mut prior_facts: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), StoredFacts> =
                HashMap::new();
            let (replaced, kept_curated, withdrawn): (usize, usize, usize) =
                if payload.mode == Some(GrabWriteMode::Replace) {
                    // What the replace rewrites is decided before the rows go: a person's
                    // correction of a stored value, or the chain superseding an output with a
                    // fresh run (ADR 0008). Rows whose value does not change decide nothing.
                    let (kind, origin, reason) = match writer {
                        flows::Writer::Chain => (
                            crate::routes::private::readings::models::Kind::Chain,
                            crate::routes::private::readings::models::Origin::Chain,
                            "superseded by a recompute",
                        ),
                        flows::Writer::Person => (
                            crate::routes::private::readings::models::Kind::ValueCorrection,
                            crate::routes::private::readings::models::Origin::Manual,
                            "replaced by a new entry",
                        ),
                    };
                    for (parameter_id, time) in &groups {
                        let rows: Vec<(chrono::DateTime<chrono::Utc>, i16, serde_json::Value)> =
                            payload
                                .readings
                                .iter()
                                .zip(&preview)
                                .filter(|(r, _)| r.parameter_id == *parameter_id && r.time == *time)
                                .map(|(_, p)| {
                                    let new = match (writer, payload.tool_run_id) {
                                        (flows::Writer::Chain, Some(run_id)) => {
                                            serde_json::json!({ "run_id": run_id })
                                        }
                                        _ => serde_json::json!({ "raw_value": p.raw_value }),
                                    };
                                    (*time, p.replicate_index, new)
                                })
                                .collect();
                        // Only the rows the replace rewrites are decided: a flagged, withdrawn
                        // or hand-curved row stays as it is (SB5) and gets a hold, not a
                        // correction. The curve rule is the delete's own, per group.
                        let supplies_curve = payload.readings.iter().zip(&preview).any(|(r, p)| {
                            r.parameter_id == *parameter_id
                                && r.time == *time
                                && p.standard_curve.is_some()
                        });
                        let guard = if supplies_curve {
                            "r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL"
                        } else {
                            "r.is_flagged IS NOT TRUE AND r.withdrawn_at IS NULL \
                             AND r.standard_curve_id IS NULL"
                        };
                        crate::routes::private::readings::service::record_keyed(
                            txn,
                            kind,
                            stream_cache[parameter_id],
                            &rows,
                            &actor,
                            Some(reason),
                            origin,
                            crate::routes::private::readings::service::Keyed::Changed,
                            Some(guard),
                            None,
                        )
                        .await?;
                    }
                    for (parameter_id, time) in &groups {
                        if let Some(row) = readings::Entity::find()
                            .select_only()
                            .column(readings::Column::Label)
                            .column(readings::Column::Notes)
                            .column(readings::Column::CreatedBy)
                            .column(readings::Column::Provenance)
                            .column(readings::Column::ProvenanceKind)
                            .filter(readings::Column::SiteId.eq(payload.site_id))
                            .filter(readings::Column::ParameterId.eq(*parameter_id))
                            .filter(readings::Column::Time.eq(*time))
                            .filter(readings::Column::MeasurementType.eq("spot"))
                            .filter(
                                Condition::any()
                                    .add(readings::Column::Label.is_not_null())
                                    .add(readings::Column::Notes.is_not_null())
                                    .add(readings::Column::CreatedBy.is_not_null())
                                    .add(readings::Column::Provenance.is_not_null()),
                            )
                            .order_by_asc(readings::Column::ReplicateIndex)
                            .into_model::<PriorFactsRow>()
                            .one(txn)
                            .await?
                        {
                            prior_facts.insert((*parameter_id, *time), row.into());
                        }
                    }
                    // The rewrite is scoped to the grab stream: another source's rows at the same
                    // instant are not this request's to rewrite. Curation wins, as in the windowed
                    // diff: a flagged, withdrawn or hand-curved row stays and the disagreement
                    // lands in the review queue.
                    let mut removed: u64 = 0;
                    let mut kept_total: usize = 0;
                    let mut withdrawn: usize = 0;
                    for (parameter_id, time) in &groups {
                        let stream_id = stream_cache[parameter_id];
                        let supplies_curve = payload.readings.iter().zip(&preview).any(|(r, p)| {
                            r.parameter_id == *parameter_id
                                && r.time == *time
                                && p.standard_curve.is_some()
                        });
                        let kept = readings::Entity::find()
                            .select_only()
                            .column(readings::Column::ReplicateIndex)
                            .column_as(kept_reason(), "reason")
                            .filter(readings::Column::StreamId.eq(stream_id))
                            .filter(readings::Column::Time.eq(*time))
                            .filter(readings::Column::MeasurementType.eq("spot"))
                            .filter(curated_or_curved(supplies_curve))
                            .order_by_asc(readings::Column::ReplicateIndex)
                            .into_model::<KeptRow>()
                            .all(txn)
                            .await?;
                        if !kept.is_empty() {
                            let entries = kept
                                .iter()
                                .map(|r| {
                                    serde_json::json!({
                                        "replicate_index": r.replicate_index,
                                        "reason": r.reason,
                                    })
                                })
                                .collect::<Vec<_>>();
                            crate::routes::private::readings::service::upsert_source_modified_hold(
                                txn,
                                stream_id,
                                *time,
                                serde_json::json!({ "claim": "replaced", "kept": entries }),
                                serde_json::json!({ "kept": true }),
                                HoldStatus::Pending,
                            )
                            .await?;
                            kept_total += kept.len();
                        }
                        // What the save carries is rewritten; what it leaves out is retracted,
                        // never deleted. A cleared cell or a narrower pasted block is a person
                        // saying the replicate is not part of the measurement any more, and the
                        // stamp is reversible where a delete is not.
                        let carried: Vec<i16> = payload
                            .readings
                            .iter()
                            .zip(&preview)
                            .filter(|(r, _)| r.parameter_id == *parameter_id && r.time == *time)
                            .map(|(_, p)| p.replicate_index)
                            .collect();
                        let uncurved = |cond: Condition| {
                            if supplies_curve {
                                cond
                            } else {
                                cond.add(readings::Column::StandardCurveId.is_null())
                            }
                        };
                        let dropped = crate::routes::private::readings::service::record_many(
                            txn,
                            crate::routes::private::readings::models::Kind::Withdraw,
                            {
                                use crate::routes::private::collection_events::flows::row;
                                use crate::routes::private::readings::models::Column;
                                use sea_orm::ExprTrait as _;
                                let mut cond = Condition::all()
                                    .add(row(Column::StreamId).eq(stream_id))
                                    .add(row(Column::Time).eq(
                                        sea_orm::prelude::DateTimeWithTimeZone::from(*time),
                                    ))
                                    .add(row(Column::MeasurementType).eq("spot"))
                                    .add(row(Column::ReplicateIndex).is_not_in(carried.clone()))
                                    .add(Expr::cust("r.is_flagged IS NOT TRUE"))
                                    .add(row(Column::WithdrawnAt).is_null());
                                if !supplies_curve {
                                    cond = cond.add(row(Column::StandardCurveId).is_null());
                                }
                                cond
                            },
                            crate::routes::private::readings::service::NewValue::Literal(
                                serde_json::json!({ "reason": "the save no longer carries this replicate" }),
                            ),
                            &actor,
                            Some("dropped by a narrower entry"),
                            crate::routes::private::readings::models::Origin::Manual,
                            None,
                        )
                        .await?;
                        withdrawn += usize::try_from(dropped.rows).unwrap_or(usize::MAX);
                        // The carried rows are rewritten in place by the insert's conflict
                        // clause, under the same guard, so what no entry sets stays on them.
                        use sea_orm::PaginatorTrait as _;
                        removed += readings::Entity::find()
                            .filter(readings::Column::StreamId.eq(stream_id))
                            .filter(readings::Column::Time.eq(*time))
                            .filter(readings::Column::MeasurementType.eq("spot"))
                            .filter(readings::Column::ReplicateIndex.is_in(carried))
                            .filter(Expr::cust("is_flagged IS NOT TRUE"))
                            .filter(readings::Column::WithdrawnAt.is_null())
                            .filter(uncurved(Condition::all()))
                            .count(txn)
                            .await?;
                    }
                    (
                        usize::try_from(removed).unwrap_or(usize::MAX),
                        kept_total,
                        withdrawn,
                    )
                } else {
                    (0, 0, 0)
                };

            // What each row records about the measurement, request first and the rewritten group's
            // own prior values where the request is silent.
            let facts = GrabFacts {
                created_by: Some(&actor),
                label: payload.label.as_deref(),
                notes: payload.notes.as_deref(),
                provenance: provenance.as_ref(),
                kind: provenance_kind,
            };
            let stored_facts: HashMap<(Uuid, chrono::DateTime<chrono::Utc>), StoredFacts> = groups
                .iter()
                .map(|group| (*group, facts.over(prior_facts.get(group))))
                .collect();

            let models: Vec<readings::ActiveModel> = payload
                .readings
                .iter()
                .zip(&preview)
                .map(|(r, p)| readings::ActiveModel {
                    standard_curve_id: Set(p.standard_curve.as_ref().map(|c| c.id)),
                    site_id: Set(Some(payload.site_id)),
                    parameter_id: Set(Some(r.parameter_id)),
                    calibrated_value: Set(p.calibrated_value),
                    sensor_id: Set(declared_instrument(
                        r.sensor_id,
                        slot_instruments.get(&r.parameter_id).copied(),
                    )
                    .or_else(|| stream_sensors.get(&stream_cache[&r.parameter_id]).copied())),
                    calibration_id: Set(p.base_calibration.as_ref().map(|c| c.id)),
                    deployment_id: Set(declared_instrument(
                        r.sensor_id,
                        slot_instruments.get(&r.parameter_id).copied(),
                    )
                    .and_then(|sid| {
                        grab_slots
                            .get(&(sid, r.parameter_id, r.time))
                            .and_then(|s| s.deployment_id)
                    })),
                    measurement_type: Set(Some(GRAB_MEASUREMENT_TYPE.to_string())),
                    label: Set(stored_facts[&(r.parameter_id, r.time)].label.clone()),
                    notes: Set(stored_facts[&(r.parameter_id, r.time)].notes.clone()),
                    created_by: Set(stored_facts[&(r.parameter_id, r.time)].created_by.clone()),
                    provenance: Set(stored_facts[&(r.parameter_id, r.time)].provenance.clone()),
                    provenance_kind: Set(stored_facts[&(r.parameter_id, r.time)].kind.clone()),
                    ..readings::new(
                        stream_cache[&r.parameter_id],
                        r.time.into(),
                        p.replicate_index,
                        r.value,
                    )
                })
                .collect();

            // A replace rewrites each carried row in place, guarded per group by whether the
            // save names a curve; any other save keeps the stored row.
            let curved_groups: HashSet<(Uuid, chrono::DateTime<chrono::Utc>)> = payload
                .readings
                .iter()
                .zip(&preview)
                .filter(|(_, p)| p.standard_curve.is_some())
                .map(|(r, _)| (r.parameter_id, r.time))
                .collect();
            let mut inserted = 0usize;
            for curved in [false, true] {
                let batch: Vec<readings::ActiveModel> = payload
                    .readings
                    .iter()
                    .zip(&models)
                    .filter(|(r, _)| curved_groups.contains(&(r.parameter_id, r.time)) == curved)
                    .map(|(_, m)| m.clone())
                    .collect();
                if batch.is_empty() {
                    continue;
                }
                let replace = if payload.mode == Some(GrabWriteMode::Replace) {
                    Replace::Entry { curved }
                } else {
                    Replace::Nothing
                };
                inserted += match readings::Entity::insert_many(batch)
                    .on_conflict(readings_upsert(replace))
                    .exec_without_returning(txn)
                    .await
                {
                    Ok(rows) => rows as usize,
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("None of the records") {
                            0
                        } else {
                            return Err(AppError::Database(e));
                        }
                    }
                };
            }
            // A curve chosen with the entry is a claim, recorded once (ADR 0008).
            crate::routes::private::readings::service::record_curve_claims(
                txn,
                &models,
                &actor,
                crate::routes::private::readings::models::Origin::Manual,
            )
            .await?;

            // An intern's entry lands pending: the record carries it, the columns project it and
            // the review queue lists it until a manager verifies or rejects (Q21, M44).
            // Only what the save entered is pending: a stored replicate the grid carried at its
            // own number stays as a manager left it.
            if entry_state == Some(crate::routes::private::readings::models::Kind::UnverifiedEntry)
            {
                let entered =
                    crate::routes::private::readings::service::entered_rows(&carried, &existing_groups);
                let entries: Vec<readings::ActiveModel> = models
                    .iter()
                    .zip(&entered)
                    .filter(|(_, entered)| **entered)
                    .map(|(m, _)| m.clone())
                    .collect();
                let entered_groups: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = groups
                    .iter()
                    .filter(|group| {
                        carried
                            .iter()
                            .zip(&entered)
                            .any(|((p, t, _, _), e)| *e && (*p, *t) == **group)
                    })
                    .copied()
                    .collect();
                crate::routes::private::readings::service::record_unverified_entries(
                    txn,
                    &entries,
                    &actor,
                    crate::routes::private::readings::models::Origin::Manual,
                )
                .await?;
                open_unverified_holds(txn, payload.site_id, &entered_groups, &actor).await?;
            }

            // A re-post is the same measurement recorded again: the rows the insert skipped on
            // conflict still take this request's story, so a second run's blob does not sit behind
            // the value it produced. Keyed on the rows this request wrote, so a curated row a
            // replace left in place keeps the provenance of the run that made it.
            for (r, p) in payload.readings.iter().zip(&preview) {
                let stored = &stored_facts[&(r.parameter_id, r.time)];
                if stored.is_empty() {
                    continue;
                }
                let keep = |column: readings::Column, value: sea_orm::Value| {
                    Expr::expr(sea_orm::sea_query::Func::coalesce([
                        Expr::val(value),
                        Expr::col(column),
                    ]))
                };
                readings::Entity::update_many()
                    .col_expr(
                        readings::Column::Label,
                        keep(readings::Column::Label, stored.label.clone().into()),
                    )
                    .col_expr(
                        readings::Column::Notes,
                        keep(readings::Column::Notes, stored.notes.clone().into()),
                    )
                    .col_expr(
                        readings::Column::CreatedBy,
                        keep(
                            readings::Column::CreatedBy,
                            stored.created_by.clone().into(),
                        ),
                    )
                    .col_expr(
                        readings::Column::Provenance,
                        keep(
                            readings::Column::Provenance,
                            stored.provenance.clone().into(),
                        ),
                    )
                    .filter(readings::Column::StreamId.eq(stream_cache[&r.parameter_id]))
                    .filter(readings::Column::Time.eq(r.time))
                    .filter(readings::Column::ReplicateIndex.eq(p.replicate_index))
                    .exec(txn)
                    .await?;
            }

            // The statistics row, for the groups that now hold two or more replicates.
            let created_sample_ids = materialise_grab_samples(
                txn,
                &groups,
                payload.site_id,
            )
            .await?;

            // Every attributed spot instant this request touched belongs to a collection event
            // (D7); a hand-entered grab is a manual visit.
            let mut touched_events = Vec::new();
            if let (Some(lo), Some(hi)) = (
                groups.iter().map(|(_, t)| *t).min(),
                groups.iter().map(|(_, t)| *t).max(),
            ) {
                crate::routes::private::collection_events::service::attach_collection_events(
                    txn,
                    {
                        use crate::routes::private::collection_events::flows::row;
                        use crate::routes::private::readings::models::Column;
                        use sea_orm::ExprTrait as _;
                        sea_orm::Condition::all()
                            .add(row(Column::SiteId).eq(payload.site_id))
                            .add(
                                row(Column::Time)
                                    .gte(sea_orm::prelude::DateTimeWithTimeZone::from(lo)),
                            )
                            .add(
                                row(Column::Time)
                                    .lte(sea_orm::prelude::DateTimeWithTimeZone::from(hi)),
                            )
                    },
                    crate::routes::private::collection_events::service::EventSource::Manual,
                )
                .await?;
                let mut instants: Vec<sea_orm::prelude::DateTimeWithTimeZone> = groups
                    .iter()
                    .map(|(_, t)| sea_orm::prelude::DateTimeWithTimeZone::from(*t))
                    .collect();
                instants.sort_unstable();
                instants.dedup();
                touched_events = flows::touched_events(txn, {
                    use crate::routes::private::readings::models::Column;
                    use sea_orm::ExprTrait as _;
                    sea_orm::Condition::all()
                        .add(flows::row(Column::SiteId).eq(payload.site_id))
                        .add(flows::row(Column::Time).is_in(instants))
                })
                .await?;
            }

            Ok((
                inserted,
                replaced,
                kept_curated,
                withdrawn,
                created_sample_ids,
                touched_events,
            ))
        })
        .await?;

    // The value has landed: the calculations that read it run without anyone asking (ADR 0007),
    // the sampled slots are reconciled and their episodes rebuilt inline (one `reprocessing_jobs`
    // row per field campaign entry would be the noise), and the site's cached responses go. Grabs
    // are excluded from the rollups, so there is nothing to refresh. A stream calculation holding
    // a saved parameter (Q230) is recomputed over the pulses that read it, as a job.
    let saved: Vec<Uuid> = stream_cache.keys().copied().collect();
    let holds =
        crate::routes::private::derived_parameters::flows::held_by_a_calculation(&state.db, &saved)
            .await?;
    let written = crate::routes::private::readings::service::Written::new(
        u64::try_from(inserted + replaced).unwrap_or(u64::MAX),
    )
    .over(
        alarm_windows
            .values()
            .copied()
            .reduce(|(lo, hi), (a, b)| (lo.min(a), hi.max(b))),
    )
    .at(stream_cache
        .keys()
        .map(|pid| crate::routes::private::readings::service::Slot::paired(payload.site_id, *pid))
        .collect())
    .touching(touched_events);
    crate::routes::private::readings::service::run(
        &state,
        &written,
        &crate::routes::private::readings::service::Axes {
            cache: crate::routes::private::readings::service::Cache::Sites,
            refresh: crate::routes::private::readings::service::Refresh::Skip,
            announce: false,
            reconcile_alarms: true,
            episodes: crate::routes::private::readings::service::Episodes::Inline,
            recompute_derived: holds,
            writer,
        },
        &crate::common::actor::label(&auth),
    )
    .await?;

    let samples_created = created_sample_ids.len();
    tracing::info!(total, inserted, replaced, kept_curated, withdrawn, samples_created, site = %site.name, "Grab samples inserted");
    Ok(Json(GrabSampleResponse {
        inserted,
        samples_created,
        created_sample_ids,
        dry_run: false,
        replaced,
        kept_curated,
        withdrawn,
        preview,
        existing_groups,
        calculations,
    }))
}

/// `POST /api/readings/import_csv/chunk`, one slice of a file that does not fit in one request.
/// The chunks accumulate as rows of `csv_import_chunks`; the import then names the session instead
/// of carrying a body. The rows are the session, so an upload survives a restart and a chunk that
/// reaches another replica appends to the same file. An upload that stops part-way is removed by
/// the janitor after `IMPORT_SESSION_RETENTION_MINUTES`.
#[utoipa::path(
    post,
    path = "/api/readings/import_csv/chunk",
    request_body = ImportChunkRequest,
    responses(
        (status = 200, description = "The session and what it now holds", body = ImportChunkResponse),
        (status = 400, description = "The session expired or was never opened"),
        (status = 413, description = "Chunk exceeds the import body limit"),
    ),
    tag = "ingestion"
)]
pub async fn import_csv_chunk(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    Json(req): Json<ImportChunkRequest>,
) -> AppResult<Json<ImportChunkResponse>> {
    let opener = auth.label();
    let session_id = req.session_id.unwrap_or_else(Uuid::new_v4);
    if req.session_id.is_some() {
        require_open_session(&state.db, session_id, &opener).await?;
    }
    let bytes = append_chunk(&state.db, session_id, &opener, &req.chunk).await?;
    Ok(Json(ImportChunkResponse { session_id, bytes }))
}

/// The file this request is about: the body's own text, or the chunks a prior upload staged.
/// Either way the text is in `csv_import_chunks` under the returned session id, so a commit that
/// re-sends the id imports exactly the file the plan was built from, on whichever replica takes it.
async fn staged_csv(
    state: &AppState,
    auth: &crate::common::middleware::AuthContext,
    req: &ImportCsvRequest,
) -> AppResult<(Arc<String>, Uuid)> {
    let opener = auth.label();
    if let Some(csv) = req.csv.as_deref() {
        let sid = Uuid::new_v4();
        append_chunk(&state.db, sid, &opener, csv).await?;
        return Ok((Arc::new(csv.to_owned()), sid));
    }
    if let Some(sid) = req.session_id {
        let text = staged_text(&state.db, sid, &opener).await?;
        return Ok((Arc::new(text), sid));
    }
    Err(AppError::BadRequest(
        "Provide either csv or session_id".into(),
    ))
}

/// One site's rows lifted out of a multi-site file: the file it would have been on its own, and
/// the line each of its rows came from, so an error still names the line the operator can see.
struct SiteRows {
    site_id: Uuid,
    csv: String,
    lines: Vec<usize>,
}

/// A file split by the site each row names. The site column itself is dropped from every share:
/// it is the file's own bookkeeping, not a parameter column.
struct SiteSplit {
    shares: Vec<SiteRows>,
    errors: Vec<RowError>,
    error_count: usize,
}

/// Split a file by its site column, or `None` when it carries no such column and the request's
/// single target stands.
async fn split_by_site(
    db: &sea_orm::DatabaseConnection,
    csv_text: &str,
    declared: Option<&str>,
    fallback: Uuid,
) -> AppResult<Option<SiteSplit>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .trim(csv::Trim::All)
        .flexible(true)
        .from_reader(csv_text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| AppError::BadRequest(format!("Failed to read CSV header: {e}")))?
        .clone();
    let names: Vec<&str> = headers.iter().collect();
    let Some(site_idx) =
        crate::routes::private::readings::service::site_column_index(&names, declared)
            .map_err(AppError::BadRequest)?
    else {
        return Ok(None);
    };

    let lookup = site_lookup(db).await?;
    let header_row: Vec<&str> = names
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != site_idx)
        .map(|(_, h)| *h)
        .collect();

    let mut writers: HashMap<Uuid, (csv::Writer<Vec<u8>>, Vec<usize>)> = HashMap::new();
    let mut order: Vec<Uuid> = Vec::new();
    let mut errors: Vec<RowError> = Vec::new();
    let mut error_count = 0usize;
    let mut line = 1usize;

    for record in reader.records() {
        line += 1;
        let record = match record {
            Ok(r) => r,
            Err(e) => {
                error_count += 1;
                if errors.len() < MAX_ERRORS {
                    errors.push(RowError {
                        row: line,
                        message: format!("CSV parse error: {e}"),
                    });
                }
                continue;
            }
        };
        let site_id = match crate::routes::private::readings::service::resolve_row_site(
            record.get(site_idx).unwrap_or(""),
            &lookup,
            fallback,
        ) {
            Ok(id) => id,
            Err(message) => {
                error_count += 1;
                if errors.len() < MAX_ERRORS {
                    errors.push(RowError { row: line, message });
                }
                continue;
            }
        };
        let entry = match writers.entry(site_id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                order.push(site_id);
                let mut writer = csv::Writer::from_writer(Vec::new());
                writer
                    .write_record(&header_row)
                    .map_err(|e| AppError::Internal(e.to_string()))?;
                e.insert((writer, Vec::new()))
            }
        };
        let cells: Vec<&str> = record
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != site_idx)
            .map(|(_, c)| c)
            .collect();
        entry
            .0
            .write_record(&cells)
            .map_err(|e| AppError::Internal(e.to_string()))?;
        entry.1.push(line);
    }

    let mut shares = Vec::with_capacity(order.len());
    for site_id in order {
        let (writer, lines) = writers.remove(&site_id).unwrap_or_else(|| {
            unreachable!("a site in file order has a writer");
        });
        let bytes = writer
            .into_inner()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        shares.push(SiteRows {
            site_id,
            csv: String::from_utf8(bytes).map_err(|e| AppError::Internal(e.to_string()))?,
            lines,
        });
    }
    Ok(Some(SiteSplit {
        shares,
        errors,
        error_count,
    }))
}

/// The sites a file may name, by the two spellings the request-level `site` accepts.
async fn site_lookup(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<crate::routes::private::readings::service::SiteLookup> {
    let rows = crate::routes::private::sites::Entity::find()
        .all(db)
        .await?;
    let mut by_id = HashSet::with_capacity(rows.len());
    let mut by_name = HashMap::with_capacity(rows.len());
    for row in rows {
        by_id.insert(row.id);
        by_name.insert(row.name.to_lowercase(), row.id);
    }
    Ok(crate::routes::private::readings::service::SiteLookup { by_id, by_name })
}

/// Import every site a file names, each against its own slots, and answer with the file's totals
/// plus what each site took.
async fn import_every_site(
    state: &AppState,
    auth: &crate::common::middleware::AuthContext,
    scope: &AccessScope,
    req: ImportCsvRequest,
    split: SiteSplit,
    session_id: Uuid,
) -> AppResult<Json<ImportCsvResponse>> {
    if req.tool.is_some() {
        return Err(AppError::BadRequest(
            "A tool-entry file is one site's run sheet; import it without a site column".into(),
        ));
    }
    // The seasonal check is one site's distribution and a commit is held to exactly the values
    // one check screened, so a spot file naming several sites has no check to name. Continuous
    // files, which is what the portals' high-frequency exports are, are not screened at all.
    if req.measurement_type.as_deref() == Some("spot") {
        return Err(AppError::BadRequest(
            "A spot file is screened against one site's seasonal history; import it one site at \
             a time"
                .into(),
        ));
    }

    let target = resolve_site_with_project(&state.db, &req.site).await?.0;
    let sites: Vec<Uuid> = split.shares.iter().map(|s| s.site_id).collect();
    // Every site up front: a token confined to one project must not stage half the file before
    // the site it cannot reach refuses.
    enforce_project_scope_for_sites(&state.db, scope, &sites).await?;

    let base = ImportCsvRequest {
        csv: None,
        session_id: None,
        site_column: None,
        ..req
    };

    let mut totals = ImportCsvResponse {
        site_id: target.id,
        site_name: target.name,
        dry_run: base.dry_run,
        session_id: Some(session_id),
        mapped_columns: HashMap::new(),
        skipped_columns: Vec::new(),
        unmapped_columns: Vec::new(),
        warnings: Vec::new(),
        row_count: 0,
        replicate_groups: 0,
        inserted_total: 0,
        earliest: None,
        latest: None,
        derived_job_id: None,
        derived_timestamps: 0,
        duplicates: 0,
        overlaps_identical: 0,
        overlaps_differing: 0,
        overwritten: 0,
        overlap_sample: Vec::new(),
        errors: split.errors,
        error_count: split.error_count,
        tool_runs_created: 0,
        curves: Vec::new(),
        check: None,
        site_imports: Vec::new(),
    };

    for share in split.shares {
        let per_site = ImportCsvRequest {
            site: share.site_id.to_string(),
            csv: Some(share.csv),
            ..base.clone()
        };
        let Json(one) =
            import_one_site(state.clone(), auth.clone(), scope.clone(), per_site).await?;

        totals.site_imports.push(SiteImportOutcome {
            site_id: one.site_id,
            site_name: one.site_name.clone(),
            row_count: one.row_count,
            inserted_total: one.inserted_total,
            derived_job_id: one.derived_job_id,
        });

        for (header, parameter) in one.mapped_columns {
            totals.mapped_columns.insert(header, parameter);
        }
        merge_columns(&mut totals.skipped_columns, one.skipped_columns);
        merge_columns(&mut totals.unmapped_columns, one.unmapped_columns);
        for warning in one.warnings {
            totals
                .warnings
                .push(format!("{}: {warning}", one.site_name));
        }
        totals.row_count += one.row_count;
        totals.replicate_groups += one.replicate_groups;
        totals.inserted_total += one.inserted_total;
        totals.derived_timestamps += one.derived_timestamps;
        totals.duplicates += one.duplicates;
        totals.overlaps_identical += one.overlaps_identical;
        totals.overlaps_differing += one.overlaps_differing;
        totals.overwritten += one.overwritten;
        totals.earliest = min_instant(totals.earliest, one.earliest);
        totals.latest = max_instant(totals.latest, one.latest);
        for diff in one.overlap_sample {
            if totals.overlap_sample.len() < OVERLAP_SAMPLE_CAP {
                totals.overlap_sample.push(diff);
            }
        }
        // A share's rows are numbered from its own header, so each error is reported against the
        // line of the file the operator uploaded.
        totals.error_count += one.error_count;
        for mut error in one.errors {
            if totals.errors.len() >= MAX_ERRORS {
                break;
            }
            if let Some(line) = share.lines.get(error.row.saturating_sub(2)) {
                error.row = *line;
            }
            error.message = format!("{}: {}", one.site_name, error.message);
            totals.errors.push(error);
        }
    }

    if let [only] = totals.site_imports.as_slice() {
        totals.derived_job_id = only.derived_job_id;
    }
    Ok(Json(totals))
}

fn merge_columns(into: &mut Vec<String>, from: Vec<String>) {
    for column in from {
        if !into.contains(&column) {
            into.push(column);
        }
    }
}

fn min_instant(
    a: Option<chrono::DateTime<chrono::Utc>>,
    b: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (one, None) | (None, one) => one,
    }
}

fn max_instant(
    a: Option<chrono::DateTime<chrono::Utc>>,
    b: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (one, None) | (None, one) => one,
    }
}

/// Import historical readings from a wide CSV. Resolves columns to parameters (explicit mapping >
/// public name > alias > catalog), skips derived outputs, inserts raw values idempotently, then
/// recomputes derived parameters and refreshes aggregates. `dry_run` returns the resolution plan
/// only. A file carrying a site column is split by site and each site's rows imported against
/// their own slots. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/import_csv",
    request_body = ImportCsvRequest,
    responses(
        (status = 200, description = "Import summary or dry-run plan", body = ImportCsvResponse),
        (status = 400, description = "Unparseable CSV, missing DateTime column, an unusable site column, or no resolvable parameter columns"),
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
    let (csv_text, session_id) = staged_csv(&state, &auth, &req).await?;
    let target = resolve_site_with_project(&state.db, &req.site).await?.0;
    let split = split_by_site(&state.db, &csv_text, req.site_column.as_deref(), target.id).await?;
    let Some(split) = split else {
        return import_one_site(state, auth, scope, req).await;
    };
    import_every_site(&state, &auth, &scope, req, split, session_id).await
}

/// One site's share of a file: the whole import, for the single site `req.site` names.
async fn import_one_site(
    state: AppState,
    auth: crate::common::middleware::AuthContext,
    scope: AccessScope,
    req: ImportCsvRequest,
) -> AppResult<Json<ImportCsvResponse>> {
    crate::routes::private::readings::service::validate_measurement_type(
        req.measurement_type.as_deref(),
    )?;

    let (csv_text, session_id) = staged_csv(&state, &auth, &req).await?;

    let tz_offset =
        chrono::Duration::milliseconds((req.tz_offset_hours.unwrap_or(0.0) * 3_600_000.0) as i64);

    let (site, _project) = resolve_site_with_project(&state.db, &req.site).await?;
    let site_id = site.id;

    // A project-scoped token may only import into a site within its project.
    enforce_project_scope_for_sites(&state.db, &scope, &[site_id]).await?;

    if req.curves.is_some() && req.tool.is_none() {
        return Err(AppError::BadRequest(
            "curves fills a tool's curve slots; name the tool the file is entry for".into(),
        ));
    }

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
    let sp_rows =
        crate::routes::private::readings::service::site_parameter_columns(&state.db, site_id)
            .await?;

    let mut site_param_map: HashMap<String, (Uuid, String)> = HashMap::new();
    let mut site_alias_map: HashMap<String, (Uuid, String)> = HashMap::new();
    let mut param_names: HashMap<Uuid, String> = HashMap::new();
    let mut site_param_ids: HashSet<Uuid> = HashSet::new();

    for row in sp_rows {
        let (pid, sp_name, param_name, aliases) = (
            row.parameter_id,
            row.sp_name.unwrap_or_default(),
            row.param_name.unwrap_or_default(),
            row.aliases.unwrap_or_default(),
        );

        site_param_map.insert(sp_name.to_lowercase(), (pid, sp_name.clone()));
        site_param_map.insert(param_name.to_lowercase(), (pid, sp_name.clone()));
        param_names.insert(pid, sp_name.clone());
        site_param_ids.insert(pid);

        for alias in &aliases {
            site_alias_map.insert(alias.to_lowercase(), (pid, sp_name.clone()));
        }
    }

    // Derived-output parameters are computed, never ingested.
    let derived_outputs =
        crate::routes::private::readings::service::derived_output_parameter_ids(&state.db).await?;

    // Global catalog fallback: lower(name) -> id, lower(alias) -> id.
    let catalog_rows = parameters::Entity::find().all(&state.db).await?;
    let mut catalog: HashMap<String, Uuid> = HashMap::new();
    for row in catalog_rows {
        let (pid, name, aliases) = (row.id, row.code, row.aliases);
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

    let columns: Vec<&str> = headers.iter().collect();
    let datetime_idx = timestamp_column(&columns).map_err(AppError::BadRequest)?;

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
    // One clock read for the file: every row is judged against the same lead bound, so two rows
    // carrying one timestamp cannot land on opposite sides of it.
    let now = chrono::Utc::now();

    let record_error =
        |row: usize, message: String, errors: &mut Vec<RowError>, count: &mut usize| {
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
        if let Some(reason) =
            crate::routes::private::readings::service::admission::time_rejection_at(now, time)
        {
            record_error(line, reason, &mut errors, &mut error_count);
            continue;
        }
        row_count += 1;
        earliest = Some(earliest.map_or(time, |e| e.min(time)));
        latest = Some(latest.map_or(time, |l| l.max(time)));
        for m in &mappings {
            let stored = match crate::routes::private::readings::service::admission::classify_cell(
                record.get(m.idx).unwrap_or(""),
            ) {
                crate::routes::private::readings::service::admission::Cell::Missing => continue,
                crate::routes::private::readings::service::admission::Cell::Invalid(reason) => {
                    record_error(
                        line,
                        format!("Column '{}': {reason}", m.header),
                        &mut errors,
                        &mut error_count,
                    );
                    continue;
                }
                crate::routes::private::readings::service::admission::Cell::Value(raw) => {
                    (raw - m.conversion_offset) / m.conversion_factor
                }
            };
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
        let family_keys = crate::routes::private::readings::service::replicate_family_keys(
            &state.db,
            &owning_ids,
        )
        .await?;
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

    // One reading per (parameter, timestamp) on every cadence but spot: a repeat is a source
    // defect, and absorbing it as replicate 1 hides the row from the default read and from every
    // rollup while fabricating a grab sample around it. A spot file is a replicate plate, where
    // the repeat is the point. The cadence is the one the write will resolve (request declaration
    // → the stream that will carry the row → the deployed sensor's data_frequency → continuous),
    // never the declaration alone, or a caller who declares nothing escapes the rule.
    {
        let mut api_stream_of: HashMap<Uuid, Uuid> = HashMap::new();
        let mapped_params: Vec<Uuid> = mappings.iter().map(|m| m.parameter_id).collect();
        for row in crate::routes::private::readings::service::api_streams_of_slots(
            &state.db,
            site_id,
            &mapped_params,
        )
        .await?
        {
            api_stream_of.insert(row.parameter_id, row.id);
        }

        let mut stream_ids: Vec<Uuid> = overlap
            .owning_stream
            .values()
            .copied()
            .chain(api_stream_of.values().copied())
            .collect();
        stream_ids.sort_unstable();
        stream_ids.dedup();
        let mut stream_default: HashMap<Uuid, Option<String>> = HashMap::new();
        if !stream_ids.is_empty() {
            for row in data_streams::Entity::find()
                .filter(data_streams::Column::Id.is_in(stream_ids))
                .all(&state.db)
                .await?
            {
                stream_default.insert(row.id, row.measurement_type);
            }
        }

        let mut sensor_ids: Vec<Uuid> = owner_map.values().filter_map(|o| o.sensor_id).collect();
        sensor_ids.sort_unstable();
        sensor_ids.dedup();
        let sensor_types =
            crate::routes::private::readings::service::measurement_types_for_sensors(
                &state.db,
                &sensor_ids,
            )
            .await?;

        let header_of: HashMap<Uuid, String> = mappings
            .iter()
            .map(|m| (m.parameter_id, m.header.clone()))
            .collect();
        let mut seen_slots: HashSet<(Uuid, chrono::DateTime<chrono::Utc>)> = HashSet::new();
        let before = rows.len();
        rows.retain(|(pid, time, _, line)| {
            let stream_id = overlap
                .owning_stream
                .get(&(*pid, *time))
                .or_else(|| api_stream_of.get(pid));
            let resolved = crate::routes::private::readings::service::resolve_measurement_type(
                req.measurement_type.as_deref(),
                stream_id
                    .and_then(|id| stream_default.get(id))
                    .and_then(Option::as_deref),
                owner_map.get(&(*pid, *time)).and_then(|o| o.sensor_id),
                &sensor_types,
            );
            if resolved == "spot" || seen_slots.insert((*pid, *time)) {
                return true;
            }
            record_error(
                *line,
                format!(
                    "Column '{}': timestamp {} is repeated, and a '{resolved}' series holds \
                     one reading per timestamp",
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

    // A spot file is screened against the site's seasonal history, cell by cell; a continuous
    // file is not, the check's history being spot readings. A cell already stored with the same
    // value is not screened: it is its own history, and importing it again changes nothing.
    let check = if req.measurement_type.as_deref() == Some("spot") {
        let cells: Vec<(usize, Uuid, chrono::DateTime<chrono::Utc>, f64)> = rows
            .iter()
            .filter(|(_, _, _, line)| !overlap.identical_lines.contains(line))
            .map(|(pid, time, value, line)| (*line, *pid, *time, *value))
            .collect();
        Some(screen_import(&state, &auth, &req, site_id, &cells).await?)
    } else {
        None
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
            curves: Vec::new(),
            check,
            site_imports: Vec::new(),
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

    // The instrument each candidate target channel carries, for the rows the slot's deployment
    // window does not attribute: an imported measurement names what produced it either way.
    let stream_sensors: HashMap<Uuid, Uuid> = {
        let mut ids: Vec<Uuid> = stream_cache.values().copied().collect();
        ids.extend(overlap.owning_stream.values().copied());
        ids.sort_unstable();
        ids.dedup();
        let mut map = HashMap::new();
        for row in data_streams::Entity::find()
            .filter(data_streams::Column::Id.is_in(ids))
            .filter(data_streams::Column::SensorId.is_not_null())
            .all(&state.db)
            .await?
        {
            if let Some(sensor_id) = row.sensor_id {
                map.insert(row.id, sensor_id);
            }
        }
        map
    };

    let staged: Vec<StagedRow> = rows
        .iter()
        .map(|(parameter_id, time, value, _)| {
            let owner = owner_map
                .get(&(*parameter_id, *time))
                .cloned()
                .unwrap_or_default();
            let stream_id = write_target(parameter_id, time);
            StagedRow {
                stream_id,
                site_id,
                parameter_id: *parameter_id,
                time: *time,
                raw_value: *value,
                // Sensor and deployment are physical facts about the slot at that time and stay
                // stamped either way; the calibration is a claim the row's value is uncorrected
                // input, which only a declared-raw import may make.
                sensor_id: owner
                    .sensor_id
                    .or_else(|| stream_sensors.get(&stream_id).copied()),
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
    // The readings are externalised to `csv_import_staging` so any replica can run the import. The
    // job reads them back by token, so the rows and the job commit together or not at all.
    let derived_job_id = if has_work {
        let import_token = Uuid::new_v4();
        let txn = state.db.begin().await?;
        stage_import_rows(&txn, import_token, &staged).await?;

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
            "param_streams": param_streams,
            "measurement_type": req.measurement_type.as_deref(),
        });

        let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
            &txn,
            "csv_import",
            None,
            None,
            &params,
            None,
        )
        .await?;
        txn.commit().await?;
        job_id
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
        curves: Vec::new(),
        check,
        site_imports: Vec::new(),
    }))
}

// --- The `/readings` surface ---

/// The routes under `/readings`, one router per authorization they carry, each with its own
/// layer (Q143). `service/mod.rs` merges them; it keeps the cross-cutting paths that act on
/// readings without naming them, `/actions/*` and `/sites/{id}/readings`.
///
/// The names carry the component because `tests/route_guards.rs` keys a router block by its
/// function name alone, across every file it scans: two components exporting `read_routes`
/// would be one block there, and the table would lose whichever it read first.
///
/// The body limits travel with the routes they were declared against: `RequestBodyLimitLayer` is
/// the enforced cap and `DefaultBodyLimit` the extractor's, and a batch carries both because the
/// central file applied both to it.
pub fn readings_read_routes(state: &AppState) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/readings/sample_preview", post(sample_preview))
        .route("/readings/seasonal_check", post(seasonal_check))
        .route("/readings/provenance", get(get_reading_provenance))
        .route("/readings/ledger", get(get_reading_ledger))
        .route("/readings/decisions", get(list_decisions))
        .route("/readings/replay", get(replay_derived))
        .route("/readings/edits/inspect", post(inspect))
        .layer(axum::middleware::from_fn(require_read_data))
        .with_state(state.clone())
}

pub fn readings_write_routes(state: &AppState) -> axum::Router {
    use axum::routing::{patch, post};
    axum::Router::new()
        .route("/readings/batch", post(insert_batch_readings))
        .layer(RequestBodyLimitLayer::new(DATA_BODY_LIMIT))
        .route("/readings/import_csv", post(import_csv))
        .route("/readings/import_csv/chunk", post(import_csv_chunk))
        .layer(axum::extract::DefaultBodyLimit::max(IMPORT_BODY_LIMIT))
        .route("/readings/edits/preview", post(preview))
        .route("/readings/edits", post(commit))
        .route("/readings/edits/{id}/rollback", post(rollback))
        .route(
            "/readings/edits/sets/{set_id}/rollback",
            post(rollback_edit_set),
        )
        .route("/readings/flag", patch(flag_readings))
        .route("/readings/unflag", patch(unflag_readings))
        .route("/readings/flag_range", patch(flag_range))
        .route("/readings/unflag_range", patch(unflag_range))
        .layer(axum::middleware::from_fn(require_write_data))
        .with_state(state.clone())
}

/// Detaching a derived output from its inputs and returning it are corrections to what the chain
/// decided, so they sit with the other administrator-only writes rather than with `write_data`.
pub fn readings_admin_routes(state: &AppState) -> axum::Router {
    use axum::routing::post;
    axum::Router::new()
        .route("/readings/detach", post(detach_output))
        .route("/readings/return", post(return_output))
        .layer(RequestBodyLimitLayer::new(ACTION_BODY_LIMIT))
        .layer(axum::middleware::from_fn(require_admin))
        .with_state(state.clone())
}

/// Readings whose curation columns disagree with the decisions recorded against them.
///
/// The columns are the projection of the record, written by the same trigger in the writer's
/// transaction, so a disagreement means something wrote a column without recording the decision,
/// or a decision failed to project. Read-only: which side is wrong is itself a decision, a
/// rollback or a fresh decision, so nothing here picks one.
#[utoipa::path(
    get,
    path = "/api/actions/curation_drift",
    params(("limit" = Option<u32>, Query, description = "How many rows to list, default 50, max 500")),
    responses((status = 200, description = "Readings that disagree with their decision record", body = CurationDriftResponse)),
    tag = "actions"
)]
pub async fn curation_drift(
    State(app_state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Query(params): Query<CurationDriftQuery>,
) -> AppResult<Json<CurationDriftResponse>> {
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let rows = crate::routes::private::readings::service::curation_drift_rows(
        &app_state.db,
        &scope,
        limit,
    )
    .await?;

    Ok(Json(CurationDriftResponse {
        total: crate::routes::private::readings::service::curation_drift_count(&app_state.db)
            .await?,
        rows,
    }))
}
