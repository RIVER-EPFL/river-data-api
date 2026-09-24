use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::Query;
use axum::extract::State;
use chrono::Utc;
use sea_orm::ColumnTrait;
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
use crate::routes::private::readings::service::admit_standard_curves;
use crate::routes::private::readings::service::readings_on_conflict;
use crate::routes::private::readings::service::rows_at;
use crate::routes::private::readings::service::run_id_of;
use crate::routes::private::readings::status_events;
use crate::routes::private::sensor_calibrations;
use crate::routes::private::sensor_calibrations::resolver;
use crate::routes::private::sensors::models::ResolvedOwner;
use crate::routes::private::sensors::service::resolve_slot_owner_for_times;
use crate::routes::private::collection_events::flows::enqueue_at_slot;
use crate::routes::private::sync::models::GroupAudit;
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

/// Replace a calculated value by hand (Q263): the slot is detached from its calculation and the
/// one value corrected in one transaction, which the provenance record reads back as the computed
/// value it replaced. Return is the way back. Requires Administrator.
#[utoipa::path(
    post,
    path = "/api/readings/override",
    request_body = OverrideRequest,
    responses(
        (status = 200, description = "Overridden", body = EditResponse),
        (status = 400, description = "The slot holds several replicates and none was named"),
        (status = 404, description = "No readings at the slot instant"),
        (status = 409, description = "The slot is already detached; correct its value instead"),
    ),
    tag = "readings"
)]
pub async fn override_output(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(req): Json<OverrideRequest>,
) -> AppResult<Json<EditResponse>> {
    let actor = label(&auth);
    let rows = output_rows_at(&state.db, req.site_id, req.parameter_id, req.time).await?;
    if rows.is_empty() {
        return Err(AppError::NotFound(
            "No spot readings at that site, parameter and instant".to_string(),
        ));
    }
    let target = override_target(&rows, req.replicate_index).map_err(AppError::BadRequest)?;
    if output_owner(&state.db, req.site_id, req.parameter_id, req.time).await? == Owner::Manual {
        return Err(AppError::Conflict(
            "The slot is already detached; correct its value instead".to_string(),
        ));
    }
    refuse_unrouted(
        &state.db,
        &override_selection(target, req.time),
        EditOption::Override,
    )
    .await?;
    let (set_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let (set_id, recorded) = record_override(txn, &rows, target, &req, &actor).await?;
        queue_visit_recomputes(txn, &recorded, &actor).await?;
        Ok((set_id, recorded))
    })
    .await?;
    refresh_edited_rollups(&state, &recorded).await;
    let decision_ids = set_decisions(&state.db, set_id).await?;
    Ok(Json(EditResponse {
        rows_decided: recorded.rows,
        decision_ids,
        set_id,
    }))
}

/// Return a detached output slot to its calculation: the value the last correction since the
/// detach replaced is restored, the tool owns the slot again, and the visit recomputes to catch
/// up with inputs that moved while it was detached. Requires Administrator.
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
            // The value the first correction since the detach replaced is the tool's last value; an
            // override records both in one transaction, so at one instant.
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
                .filter(sea_orm::ExprTrait::gte(
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
        enqueue_at_slot(txn, req.site_id, req.parameter_id, req.time, &actor).await?;
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
    require_checked_correction(&state.db, &req.selection, &req.decision, kind).await?;

    let (set_id, recorded) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let (set_id, recorded) =
            apply(txn, &req.selection, &req.decision, &actor, auth.origin()).await?;
        queue_visit_recomputes(txn, &recorded, &actor).await?;
        Ok((set_id, recorded))
    })
    .await?;

    refresh_edited_rollups(&state, &recorded).await;

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
        (status = 409, description = "Already rolled back, a decision that projects nothing, or a verification ruling's, which its hold's reopen undoes"),
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
        let decision = load(txn, id).await?;
        refuse_ruling_rollback(txn, decision.set_id).await?;
        let (rollback_id, recorded) = crate::routes::private::readings::service::rollback(
            txn,
            id,
            &actor,
            Some("edit rolled back"),
        )
        .await?;
        queue_visit_recomputes(txn, &recorded, &actor).await?;
        // Inverting a pin changes what the window resolves for that reading, and only the
        // reprocess writes it.
        crate::routes::private::readings::service::enqueue_pin_reprocess_for_decision(txn, id)
            .await?;
        Ok((rollback_id, recorded))
    })
    .await?;
    refresh_edited_rollups(&state, &recorded).await;
    Ok(Json(RollbackResponse { rollback_id }))
}

/// Every decision an edit set recorded, across the streams it reached, with each reading's
/// parameter: what rolling the set back restores. Requires `read_data`.
#[utoipa::path(
    get,
    path = "/api/readings/edits/sets/{set_id}",
    params(("set_id" = Uuid, Path, description = "The set the edit recorded")),
    responses(
        (status = 200, description = "The set and its decisions", body = EditSetResponse),
        (status = 404, description = "No such set"),
    ),
    tag = "readings"
)]
pub async fn get_edit_set(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    axum::extract::Path(set_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<EditSetResponse>> {
    let sites = decided_sites(&state.db, decision_model::Column::SetId, set_id).await?;
    require_edit_in_scope(&state.db, &scope, &sites).await?;
    Ok(Json(
        crate::routes::private::readings::service::set_members(&state.db, set_id).await?,
    ))
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
        (status = 409, description = "Already rolled back, or a verification ruling's set, which its hold's reopen undoes"),
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
        refuse_ruling_rollback(txn, Some(set_id)).await?;
        let (rolled_back, recorded) = crate::routes::private::readings::service::rollback_set(
            txn,
            set_id,
            &actor,
            Some("edit set rolled back"),
        )
        .await?;
        queue_visit_recomputes(txn, &recorded, &actor).await?;
        crate::routes::private::readings::service::enqueue_pin_reprocess_for_set(txn, set_id)
            .await?;
        Ok((rolled_back, recorded))
    })
    .await?;
    refresh_edited_rollups(&state, &recorded).await;
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
    // An accepted correction rewrites a served value, so it takes the same tail every other
    // curation write takes: the rollups over the span it moved, the cache, the visits whose
    // calculations read it and the derived values its slots feed.
    let (response, written) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let (response, written) = decide(
            txn,
            &req.ids,
            accept,
            &actor,
            req.reason.as_deref(),
            projects.as_deref(),
        )
        .await?;
        crate::routes::private::readings::service::queue(txn, &written, &ACCEPT_TAIL).await?;
        Ok((response, written))
    })
    .await?;
    crate::routes::private::readings::service::run(&state, &written, &ACCEPT_TAIL, &actor).await?;
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
/// a (site, parameter) pair has none, and pairs one to the site's slot once the site carries it; a
/// row is attributed from its stream's pairing. 10MB body limit. Requires `write_data`.
#[utoipa::path(
    post,
    path = "/api/readings/batch",
    request_body = BatchReadingsRequest,
    responses(
        (status = 200, description = "Inserted count", body = BatchReadingsResponse),
        (status = 400, description = "Timestamp outside the admissible window, non-finite value, or a calibrated_value the row's calibration does not produce"),
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

    // Each (site_id, parameter_id) pair's api channel, paired to the site's slot once it exists.
    // What the rows are attributed to is read from that pairing in the transaction storing them.
    let mut stream_cache: HashMap<(Uuid, Uuid), Uuid> = HashMap::new();
    let actor = crate::common::actor::label(&auth);
    for r in &payload.readings {
        let key = (r.site_id, r.parameter_id);
        if let std::collections::hash_map::Entry::Vacant(entry) = stream_cache.entry(key) {
            let stream_id = get_or_create_api_stream(&state.db, r.site_id, r.parameter_id).await?;
            let slot = crate::routes::private::data_streams::service::site_parameter_of(
                &state.db,
                r.site_id,
                r.parameter_id,
            )
            .await?;
            crate::routes::private::data_streams::flows::pair_entry_channel(
                &state.db, stream_id, slot, &actor,
            )
            .await?;
            entry.insert(stream_id);
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

    // For rows that don't carry an explicit sensor, the deployment covering the time at the slot:
    // what a row on a paired stream is attributed to where it declares nothing of its own.
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

    // Sensor-frequency defaults for readings that don't declare a measurement_type: every
    // instrument a row could be stored against or classified by, one query.
    let sensor_types = {
        let mut candidate_sensors: Vec<Uuid> = payload
            .readings
            .iter()
            .filter_map(|r| r.sensor_id)
            .chain(owner_map.values().filter_map(|o| o.sensor_id))
            .chain(stream_sensors.values().copied())
            .collect();
        candidate_sensors.sort_unstable();
        candidate_sensors.dedup();
        crate::routes::private::readings::service::measurement_types_for_sensors(
            &state.db,
            &candidate_sensors,
        )
        .await?
    };

    // Per-reading context, resolved before the transaction: which stream the row lands on and
    // which instrument the slot's deployment would give it when it names none.
    struct Resolved {
        stream_id: Uuid,
        owner: ResolvedOwner,
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
            Resolved { stream_id, owner }
        })
        .collect();

    // The base calibration every row sits on, so the stored value is the one its recorded curves
    // produce: instrument correction first, hand-picked curve on its result.
    let base_calibrations: HashMap<Uuid, sensor_calibrations::service::Curve> = {
        let mut ids: Vec<Uuid> = payload
            .readings
            .iter()
            .zip(&resolved)
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

    // What a row is stored against is decided in the transaction storing it, from the pairing its
    // stream reads there: a row on an unpaired stream keeps only what it declared. Its cadence and
    // its curve claim are judged against that same attribution, never an instrument it does not
    // name.
    let rows: Vec<(ReadingInput, Resolved)> = payload.readings.into_iter().zip(resolved).collect();
    struct Decided {
        attribution: crate::routes::private::readings::service::BatchAttribution,
        measurement_type: String,
    }
    let decide = |r: &ReadingInput, res: &Resolved, paired: bool| {
        let channel = stream_sensors.get(&res.stream_id).copied();
        let attribution = crate::routes::private::readings::service::batch_attribution(
            crate::routes::private::readings::service::BatchAttribution {
                sensor_id: r.sensor_id,
                deployment_id: r.deployment_id,
                calibration_id: r.calibration_id,
            },
            &res.owner,
            channel,
            paired,
        );
        let measurement_type = crate::routes::private::readings::service::resolve_measurement_type(
            r.measurement_type.as_deref(),
            stream_defaults
                .get(&res.stream_id)
                .and_then(|d| d.as_deref()),
            attribution.cadence_instrument(channel),
            &sensor_types,
        );
        Decided {
            attribution,
            measurement_type,
        }
    };
    let model_of =
        |r: &ReadingInput,
         res: &Resolved,
         decided: &Decided,
         standard_curve_models: &HashMap<Uuid, crate::routes::private::standard_curves::Model>|
         -> AppResult<readings::ActiveModel> {
            let attribution = decided.attribution;
            let standard = r.standard_curve_id.map(|id| {
                let c = &standard_curve_models[&id];
                sensor_calibrations::service::Curve {
                    id: c.id,
                    slope: c.slope,
                    intercept: c.intercept,
                }
            });
            let base = attribution
                .calibration_id
                .and_then(|id| base_calibrations.get(&id).copied());
            let correction = crate::routes::private::readings::service::batch_correction(
                r.raw_value,
                r.calibrated_value,
                attribution.calibration_id,
                base,
                standard,
            )
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
            Ok(readings::ActiveModel {
                standard_curve_id: Set(r.standard_curve_id),
                provenance_kind: Set(Some("batch".to_string())),
                calibrated_value: Set(correction.calibrated_value),
                sensor_id: Set(attribution.sensor_id),
                calibration_id: Set(correction.calibration_id),
                deployment_id: Set(attribution.deployment_id),
                measurement_type: Set(Some(decided.measurement_type.clone())),
                sample_id: Set(r.sample_id),
                ..readings::new(
                    res.stream_id,
                    r.time.into(),
                    r.replicate_index.unwrap_or(0),
                    r.raw_value,
                )
            })
        };

    let total = rows.len();
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
    let span = batch_span(&site_timestamps_for_derived);
    let calculations;
    let tail;
    (inserted, overwritten, calculations, tail) = crate::common::bulk_write::guarded(&state.db, async |txn| {
        let attributions = crate::routes::private::readings::service::lock_stream_attributions(
            txn,
            stream_cache.values().copied(),
        )
        .await?;
        let attributed_sites: Vec<Uuid> = attributions.values().filter_map(|(s, _)| *s).collect();
        enforce_project_scope_for_sites(&state.db, &scope, &attributed_sites).await?;
        let decisions: Vec<_> = rows
            .iter()
            .map(|(r, res)| decide(r, res, attributions[&res.stream_id].0.is_some()))
            .collect();
        // The cadence a reading is stored under is resolved, not declared, so the replicate index
        // is judged against the resolved value rather than the request's.
        for ((r, _), decided) in rows.iter().zip(&decisions) {
            crate::routes::private::readings::service::admission::admit_replicate_index(
                Some(decided.measurement_type.as_str()),
                r.replicate_index.unwrap_or(0),
            )?;
        }
        // A caller-supplied standard curve is held to the same rule as a grab entry: the reading
        // must be that instrument's own spot measurement, and the corrected value is computed here
        // from the curve rather than taken from the request.
        let claims: Vec<CurveClaim<'_>> = rows
            .iter()
            .zip(&decisions)
            .filter_map(|((r, _), decided)| {
                r.standard_curve_id.map(|id| CurveClaim {
                    standard_curve_id: id,
                    sensor_id: decided.attribution.sensor_id,
                    measurement_type: &decided.measurement_type,
                })
            })
            .collect();
        let standard_curve_models = admit_standard_curves(txn, &claims).await?;
        let mut models = Vec::with_capacity(rows.len());
        for ((r, res), decided) in rows.iter().zip(&decisions) {
            let (site_id, parameter_id) = attributions[&res.stream_id];
            let mut m = model_of(r, res, decided, &standard_curve_models)?;
            m.site_id = Set(site_id);
            m.parameter_id = Set(parameter_id);
            models.push(m);
        }

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

        let tail = batch_tail(
            inserted + overwritten,
            overwritten,
            span,
            &stream_cache,
            &attributions,
            touched_events.clone(),
        );
        crate::routes::private::readings::service::queue(txn, &tail.0, &tail.1).await?;
        // The calculations the values feed run without anyone asking (ADR 0007); what they are is
        // reported back alongside the counts.
        let calculations = crate::routes::private::tools::service::calculations_fed_by(
            txn,
            &crate::routes::private::readings::service::chained_parameters(&touched_events),
        )
        .await?;
        if conflict == ConflictMode::Overwrite && overwritten > 0 {
            crate::routes::private::readings::service::recompose_batch_overwrite(txn, &models)
                .await?;
        }
        if inserted > 0 || overwritten > 0 {
            crate::routes::private::readings::service::enqueue_batch_derived(
                txn,
                &site_timestamps_for_derived,
            )
            .await?;
        }
        Ok((inserted, overwritten, calculations, tail))
    })
    .await?;

    tracing::debug!(
        total,
        inserted,
        overwritten,
        "Batch readings insert complete"
    );

    crate::routes::private::readings::service::run(&state, &tail.0, &tail.1, &actor).await?;

    Ok(Json(BatchReadingsResponse {
        inserted,
        overwritten,
        calculations,
    }))
}

/// The instants a batch landed at, earliest and latest.
fn batch_span(
    site_timestamps: &HashMap<Uuid, Vec<chrono::DateTime<chrono::Utc>>>,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let instants = || site_timestamps.values().flatten().copied();
    instants().min().zip(instants().max())
}

/// What a batch landed, in the shape the shared tail reads, and the tail it takes.
///
/// An overwrite replaced values the rollups have already materialised; an insert appended past
/// them, which the next scheduled refresh covers. Best-effort: the rows are committed, so a
/// refresh losing a lock to the janitor must not report a write that happened as one that did
/// not. Episodes go to the `alarm_backfill` job because a batch can span a long window.
fn batch_tail(
    moved: usize,
    overwritten: usize,
    span: Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>,
    streams: &HashMap<(Uuid, Uuid), Uuid>,
    attributions: &HashMap<Uuid, (Option<Uuid>, Option<Uuid>)>,
    touched_events: Vec<crate::routes::private::collection_events::flows::TouchedEvent>,
) -> (
    crate::routes::private::readings::service::Written,
    crate::routes::private::readings::service::Axes,
) {
    use crate::routes::private::readings::service::{
        Axes, Cache, Episodes, Refresh, Slot, Written,
    };
    let written = Written::new(u64::try_from(moved).unwrap_or(u64::MAX))
        .over(span)
        .at(streams
            .values()
            .map(|stream_id| {
                let (site_id, parameter_id) = attributions[stream_id];
                Slot {
                    site_id,
                    parameter_id,
                    stream_id: Some(*stream_id),
                }
            })
            .collect())
        .touching(touched_events);
    let axes = Axes {
        cache: Cache::Sites,
        refresh: if overwritten > 0 {
            Refresh::Range { fatal: false }
        } else {
            Refresh::Skip
        },
        announce: true,
        reconcile_alarms: true,
        episodes: Episodes::Job,
        recompute_derived: false,
        writer: crate::routes::private::collection_events::flows::Writer::Person,
    };
    (written, axes)
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
    refuse_sync_only_claims(&payload, is_sync_service)?;
    if is_nothing_to_ingest(&payload) {
        return Ok(Json(ingest_outcome(payload.stream_id, false, 0, 0, &[])));
    }

    let txn = sea_orm::TransactionTrait::begin(&state.db).await?;
    let stream = lock_ingest_stream(&txn, payload.stream_id).await?;
    let (site_id, parameter_id) = resolve_stream_slot(&txn, stream.site_parameter_id).await?;
    let paired = site_id.is_some();
    require_spot_window(payload.window.as_ref(), stream.measurement_type.as_deref())?;
    enforce_ingest_scope(&state.db, &scope, site_id).await?;
    let mut funnel = IngestFunnel::new(&payload.readings, payload.window.is_some());
    funnel.admit(&mut payload.readings, Utc::now());
    funnel.keep_last_per_key(&mut payload.readings);
    if is_nothing_to_ingest(&payload) {
        return Ok(Json(funnel.outcome(payload.stream_id, paired, 0)));
    }
    let mut attribution =
        resolve_ingest_attribution(&txn, &stream, (site_id, parameter_id), &payload.readings)
            .await?;
    let requests = attribution.calibration_requests(&payload.readings);
    let resolved = resolver::resolve_many(&txn, &requests).await?;
    let declared = find_declared_calibrations(&txn, &payload.readings).await?;
    funnel.drop_unknown_calibrations(&mut payload.readings, &declared);
    if is_nothing_to_ingest(&payload) {
        return Ok(Json(funnel.outcome(payload.stream_id, paired, 0)));
    }
    attribution.read_cadences(&txn, &payload.readings).await?;
    funnel.drop_replicates_off_spot(&mut payload.readings, &attribution);
    let claimed = find_claimed_standard_curves(&txn, &payload.readings).await?;
    let stripped = strip_inadmissible_curve_claims(&mut payload.readings, &claimed, &attribution);
    if is_nothing_to_ingest(&payload) {
        return Ok(Json(funnel.outcome(payload.stream_id, paired, 0)));
    }
    let curves = IngestCurves {
        resolved,
        declared,
        standard: claimed.curves,
    };
    let models = reading_models(payload.stream_id, &payload.readings, &attribution, &curves);
    let sample_window = spot_window(&models, paired);
    let actor = label(&auth);
    let pass = IngestPass {
        payload: &payload,
        models: &models,
        funnel: &funnel,
        stripped: &stripped,
        sample_window,
        paired,
        is_sync_service,
        actor: &actor,
    };
    let IngestWritten {
        inserted,
        diff,
        touched_events,
    } = pass.write(&txn).await?;
    let effect = IngestEffect::of(&payload, sample_window, inserted, diff.as_ref());
    enqueue_ingest_derived(
        &txn,
        payload.stream_id,
        site_id,
        &effect,
        &payload.readings,
        diff.as_ref(),
    )
    .await?;
    txn.commit().await?;

    recompose_corrected_span(&state.db, payload.stream_id, &effect).await;
    let written_at = Slot {
        site_id,
        parameter_id,
        stream_id: Some(payload.stream_id),
    };
    super::service::run(
        &state,
        &effect.written(written_at, touched_events),
        &effect.axes(),
        &actor,
    )
    .await?;
    let cursor = advance_cursor(&payload.readings, stream.last_data_time);
    let digest = clean_digest(
        payload.window.as_ref(),
        funnel.rejected_total(),
        stripped.is_empty(),
        diff.as_ref(),
    );
    record_stream_pass(&state.db, &stream, cursor, digest).await;
    let outcome = funnel.outcome(payload.stream_id, paired, inserted);
    Ok(Json(classified_outcome(
        outcome,
        diff.as_ref(),
        payload.window.as_ref(),
    )))
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

    let txn = sea_orm::TransactionTrait::begin(&state.db).await?;
    let stream = lock_ingest_stream(&txn, payload.stream_id).await?;
    let (site_id, parameter_id) = resolve_stream_slot(&txn, stream.site_parameter_id).await?;
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
        .one(&txn)
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
    let inserted = status_events::service::insert_ignoring_duplicates(&txn, models).await?;
    txn.commit().await?;

    tracing::debug!(total, inserted, skipped, deduplicated, stream_id = %payload.stream_id, paired, "Status events ingest complete");
    Ok(Json(IngestStatusEventsResponse {
        inserted,
        skipped,
        deduplicated,
        stream_id: payload.stream_id,
        paired,
    }))
}

/// The step of a grab save that keeps a typed value off a calculated parameter (Q263).
async fn refuse_hand_values_over_calculations(
    db: &impl sea_orm::ConnectionTrait,
    tool_run_id: Option<Uuid>,
    site_id: Uuid,
    readings: &[GrabSampleReading],
) -> AppResult<()> {
    if tool_run_id.is_some() {
        return Ok(());
    }
    let parameter_ids: Vec<Uuid> = readings.iter().map(|r| r.parameter_id).collect();
    let writers =
        crate::routes::private::tools::service::calculations_writing(db, &parameter_ids).await?;
    refuse_hand_save_over_calculated(tool_run_id, site_id, &parameter_ids, &writers)
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
    let db = &state.db;
    let readings = &payload.readings;
    require_grab_readings(readings)?;
    enforce_project_scope_for_sites(db, &scope, &[payload.site_id]).await?;
    admit_grab_readings(readings)?;
    let site = find_grab_site(db, payload.site_id).await?;
    let slots = load_grab_slots(db, site.id, readings).await?;
    slots.require_configured(&site, readings)?;
    refuse_hand_values_over_calculations(db, payload.tool_run_id, site.id, readings).await?;
    require_checked_values(db, payload.check_id, site.id, readings).await?;
    let indices = assign_replicate_indices(readings)?;
    let actor = label(&auth);
    let provenance =
        resolve_tool_run_provenance(db, payload.tool_run_id, site.id, readings, &actor).await?;
    require_picked_instruments(db, readings).await?;
    let standard_curves = admit_grab_curves(db, readings, &slots).await?;
    let base_curves = resolve_grab_calibrations(db, readings, &slots).await?;
    let preview = grab_preview(readings, &indices, &slots, &base_curves, &standard_curves);
    let groups = grab_groups(readings);
    let existing_groups = fetch_existing_groups(db, payload.site_id, &groups).await?;
    let run_source = tool_run_source(db, payload.tool_run_id).await?;
    let writer = grab_writer(run_source.as_deref());
    let provenance_kind = provenance_kind_for_run(run_source.as_deref());
    let calculations = calculations_fed_by_grab(db, writer, readings).await?;
    if payload.dry_run {
        return Ok(Json(grab_dry_run(preview, existing_groups, calculations)));
    }

    let carried = carried_replicates(readings, &preview);
    refuse_intern_rewrite(
        auth.highest_role().as_ref(),
        payload.mode,
        &carried,
        &existing_groups,
    )?;
    let entry_state = entry_kind(payload.pending_inputs, auth.highest_role().as_ref());
    refuse_stale_read(payload.expected_replicates.as_deref(), &existing_groups)?;
    refuse_unasked_replace(payload.mode, &existing_groups)?;
    let stream_cache = grab_streams(db, payload.site_id, readings, &slots).await?;
    let stream_instruments = grab_stream_instruments(db, &stream_cache).await?;
    let deployments = grab_deployments(db, payload.site_id, readings, &slots).await;
    let total = readings.len();

    // One guarded transaction: a replace on a compressed chunk must not fail on the cap, and the
    // decisions, the sample rows and the insert land together or not at all.
    let write = GrabWrite {
        payload: &payload,
        preview: &preview,
        groups: &groups,
        existing_groups: &existing_groups,
        carried: &carried,
        slots: &slots,
        streams: &stream_cache,
        stream_instruments: &stream_instruments,
        deployments: &deployments,
        writer,
        actor: &actor,
        provenance: provenance.as_ref(),
        provenance_kind,
        entry_state,
    };
    let (written, tail) = crate::common::bulk_write::guarded(db, async |txn| {
        let written = write.run(txn).await?;
        let tail = write.tail(txn, &written).await?;
        crate::routes::private::readings::service::queue(txn, &tail.0, &tail.1).await?;
        Ok((written, tail))
    })
    .await?;
    crate::routes::private::readings::service::run(&state, &tail.0, &tail.1, &actor).await?;
    let GrabWritten {
        inserted,
        replaced,
        kept_curated,
        withdrawn,
        created_sample_ids,
        edit_set_id,
        ..
    } = written;

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
        edit_set_id,
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
    let db = &state.db;
    validate_measurement_type(req.measurement_type.as_deref())?;
    let (csv_text, session_id) = staged_csv(&state, &auth, &req).await?;
    let tz_offset = declared_offset(req.tz_offset_hours);
    let (site, _project) = resolve_site_with_project(db, &req.site).await?;
    enforce_project_scope_for_sites(db, &scope, &[site.id]).await?;
    require_tool_for_curves(&req)?;
    // Tool entry: the file's columns are tool inputs, not catalog parameters. Same write path as
    // typing the rows into the tool (D15).
    if let Some(tool_name) = req.tool.as_deref() {
        let response = import_tool_csv(
            &state, &auth, &scope, &req, tool_name, &csv_text, session_id, &site, tz_offset,
        )
        .await?;
        return Ok(Json(response));
    }

    let resolver = column_resolver(db, site.id).await?;
    let mut reader = csv_reader(&csv_text);
    let headers = file_headers(&mut reader)?;
    let headers: Vec<&str> = headers.iter().collect();
    let datetime_idx = timestamp_column(&headers).map_err(AppError::BadRequest)?;
    let plan = plan_columns(
        &headers,
        datetime_idx,
        req.mapping.as_ref(),
        &resolver,
        &site.name,
    );
    let mut parsed = parse_rows(
        &mut reader,
        datetime_idx,
        tz_offset,
        &plan.mappings,
        Utc::now(),
    );
    let overlap =
        compute_overlaps(db, site.id, &parsed.rows, parsed.earliest, parsed.latest).await?;
    let overlap = refuse_family_rows(db, site.id, &plan.mappings, &mut parsed, overlap).await?;
    let owners = slot_owners(db, site.id, &parsed.rows).await?;
    let overlap = refuse_repeated_rows(
        db,
        site.id,
        &req,
        &plan.mappings,
        &owners,
        &mut parsed,
        overlap,
    )
    .await?;
    let check = screen_spot_file(&state, &auth, &req, site.id, &parsed.rows, &overlap).await?;
    let analysis = SiteAnalysis {
        site_id: site.id,
        site_name: site.name,
        session_id,
        replicate_groups: replicate_groups(req.measurement_type.as_deref(), &parsed.rows),
        plan,
        parsed,
        overlap,
        check,
    };
    if req.dry_run {
        return Ok(Json(analysis.response(None)));
    }

    require_columns(&analysis.plan)?;
    let api_streams = importer_streams(db, analysis.site_id, &analysis.plan.mappings).await?;
    pair_importer_streams(db, analysis.site_id, &api_streams, &label(&auth)).await?;
    let targets = WriteTargets::new(&analysis.overlap.owning_stream, &api_streams);
    let streams = target_streams(db, &api_streams, &analysis.overlap.owning_stream).await?;
    let staged = staged_rows(
        &analysis.parsed.rows,
        &owners,
        &targets,
        &streams,
        req.values,
    );
    let tally = import_tally(
        analysis.parsed.rows.len(),
        analysis.overlap.identical,
        analysis.overlap.differing,
        req.conflict,
    );
    let derived_job_id = if tally.has_work {
        stage_and_enqueue_import(
            db,
            &analysis,
            &req,
            &api_streams,
            &staged,
            tally.overlapping,
        )
        .await?
    } else {
        None
    };
    let derived_timestamps = distinct_instants(&analysis.parsed.rows);
    Ok(Json(analysis.response(Some(Committed {
        tally,
        derived_job_id,
        derived_timestamps,
    }))))
}

/// How a header resolves at this site: its slots, the catalog, and the calculations' outputs.
async fn column_resolver(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
) -> AppResult<ColumnResolver> {
    let slots = site_parameter_columns(db, site_id).await?;
    let catalog = parameters::Entity::find().all(db).await?;
    let derived_outputs = derived_output_parameter_ids(db).await?;
    Ok(ColumnResolver::new(
        slots,
        catalog.into_iter().map(|p| (p.id, p.code, p.aliases)),
        derived_outputs,
    ))
}

/// Refuse the rows a replicate-family stream serves, and the overlap as the remaining rows stand.
async fn refuse_family_rows(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    mappings: &[ColumnMapping],
    parsed: &mut ParsedFile,
    overlap: OverlapReport,
) -> AppResult<OverlapReport> {
    let owning_streams = distinct_ids(overlap.owning_stream.values().copied());
    let family_keys = replicate_family_keys(db, &owning_streams).await?;
    let refused = refuse_family_slots(
        &mut parsed.rows,
        &overlap.owning_stream,
        &family_keys,
        &headers_by_parameter(mappings),
        &mut parsed.errors,
    );
    if !refused {
        return Ok(overlap);
    }
    compute_overlaps(db, site_id, &parsed.rows, parsed.earliest, parsed.latest).await
}

/// The sensor, deployment and calibration whose windows cover each row's slot and time, so a row
/// inside an existing deployment lands attributed. A row outside every window resolves to none.
async fn slot_owners(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    rows: &[ImportRow],
) -> AppResult<HashMap<SlotInstant, ResolvedOwner>> {
    let mut times_by_parameter: HashMap<Uuid, Vec<chrono::DateTime<Utc>>> = HashMap::new();
    for (pid, t, _, _) in rows {
        times_by_parameter.entry(*pid).or_default().push(*t);
    }
    let mut owners = HashMap::new();
    for (pid, times) in &times_by_parameter {
        for (t, owner) in resolve_slot_owner_for_times(db, site_id, *pid, times).await? {
            owners.insert((*pid, t), owner);
        }
    }
    Ok(owners)
}

/// Refuse a repeated timestamp on every cadence but spot, and the overlap as the remaining rows
/// stand.
async fn refuse_repeated_rows(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    req: &ImportCsvRequest,
    mappings: &[ColumnMapping],
    owners: &HashMap<SlotInstant, ResolvedOwner>,
    parsed: &mut ParsedFile,
    overlap: OverlapReport,
) -> AppResult<OverlapReport> {
    let cadence = slot_cadence(
        db,
        site_id,
        req.measurement_type.as_deref(),
        mappings,
        owners,
        &overlap.owning_stream,
    )
    .await?;
    let refused = refuse_repeated_slots(
        &mut parsed.rows,
        |pid, time| cadence.of(pid, time),
        &headers_by_parameter(mappings),
        &mut parsed.errors,
    );
    if !refused {
        return Ok(overlap);
    }
    compute_overlaps(db, site_id, &parsed.rows, parsed.earliest, parsed.latest).await
}

/// What a row's cadence resolves from: the importer's streams at this site, the declared default
/// of every stream a row may land on, and the frequency of every sensor that owns a row's slot.
async fn slot_cadence<'a>(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    declared: Option<&'a str>,
    mappings: &[ColumnMapping],
    owners: &'a HashMap<SlotInstant, ResolvedOwner>,
    owning_stream: &'a HashMap<SlotInstant, Uuid>,
) -> AppResult<SlotCadence<'a>> {
    let mapped: Vec<Uuid> = mappings.iter().map(|m| m.parameter_id).collect();
    let api_stream_of: HashMap<Uuid, Uuid> = api_streams_of_slots(db, site_id, &mapped)
        .await?
        .into_iter()
        .map(|row| (row.parameter_id, row.id))
        .collect();
    let streams = distinct_ids(
        owning_stream
            .values()
            .chain(api_stream_of.values())
            .copied(),
    );
    let stream_default = stream_cadences(db, streams).await?;
    let sensors = distinct_ids(owners.values().filter_map(|o| o.sensor_id));
    let sensor_types = measurement_types_for_sensors(db, &sensors).await?;
    Ok(SlotCadence {
        declared,
        owning_stream,
        api_stream_of,
        stream_default,
        owners,
        sensor_types,
    })
}

/// The measurement_type each stream declares, if any.
async fn stream_cadences(
    db: &sea_orm::DatabaseConnection,
    stream_ids: Vec<Uuid>,
) -> AppResult<HashMap<Uuid, Option<String>>> {
    if stream_ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(stream_ids))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.id, row.measurement_type))
        .collect())
}

/// A spot file screened against the site's seasonal history; a continuous file is not, the
/// check's history being spot readings.
async fn screen_spot_file(
    state: &AppState,
    auth: &crate::common::middleware::AuthContext,
    req: &ImportCsvRequest,
    site_id: Uuid,
    rows: &[ImportRow],
    overlap: &OverlapReport,
) -> AppResult<Option<ImportCheck>> {
    if req.measurement_type.as_deref() != Some("spot") {
        return Ok(None);
    }
    let cells = screened_cells(rows, &overlap.identical_lines);
    Ok(Some(
        screen_import(state, auth, req, site_id, &cells).await?,
    ))
}

/// The importer's `api` stream for each mapped parameter at this site, created where missing.
async fn importer_streams(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    mappings: &[ColumnMapping],
) -> AppResult<HashMap<Uuid, Uuid>> {
    let mut streams = HashMap::new();
    for m in mappings {
        if let std::collections::hash_map::Entry::Vacant(e) = streams.entry(m.parameter_id) {
            e.insert(get_or_create_api_stream(db, site_id, m.parameter_id).await?);
        }
    }
    Ok(streams)
}

/// Pair each importer channel to its slot once the site carries it, with the backfill and the
/// visit recomputes every pairing owes.
async fn pair_importer_streams(
    db: &sea_orm::DatabaseConnection,
    site_id: Uuid,
    api_streams: &HashMap<Uuid, Uuid>,
    actor: &str,
) -> AppResult<()> {
    for (&parameter_id, &stream_id) in api_streams {
        let slot = data_streams::service::site_parameter_of(db, site_id, parameter_id).await?;
        data_streams::flows::pair_entry_channel(db, stream_id, slot, actor).await?;
    }
    Ok(())
}

/// The slot each candidate target stream is paired to and the instrument it carries: a row is
/// attributed from its stream's pairing, and the instrument stands in for a row no deployment
/// window attributes.
async fn target_streams(
    db: &sea_orm::DatabaseConnection,
    api_streams: &HashMap<Uuid, Uuid>,
    owning_stream: &HashMap<SlotInstant, Uuid>,
) -> AppResult<HashMap<Uuid, TargetStream>> {
    let ids = distinct_ids(api_streams.values().chain(owning_stream.values()).copied());
    let rows = data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(ids))
        .all(db)
        .await?;
    let slot_ids = distinct_ids(rows.iter().filter_map(|row| row.site_parameter_id));
    let slots: HashMap<Uuid, (Uuid, Uuid)> =
        crate::routes::private::site_parameters::models::Entity::find()
            .filter(crate::routes::private::site_parameters::models::Column::Id.is_in(slot_ids))
            .all(db)
            .await?
            .into_iter()
            .map(|sp| (sp.id, (sp.site_id, sp.parameter_id)))
            .collect();
    Ok(rows
        .into_iter()
        .map(|row| {
            let stream = TargetStream {
                slot: row.site_parameter_id.and_then(|id| slots.get(&id).copied()),
                instrument: row.sensor_id,
            };
            (row.id, stream)
        })
        .collect())
}

/// Stage the rows in `csv_import_staging` and enqueue the `csv_import` job that reads them back,
/// in one transaction, so any replica can run the import and the rows and the job commit together
/// or not at all.
async fn stage_and_enqueue_import(
    db: &sea_orm::DatabaseConnection,
    analysis: &SiteAnalysis,
    req: &ImportCsvRequest,
    api_streams: &HashMap<Uuid, Uuid>,
    staged: &[StagedRow],
    overlapping: usize,
) -> AppResult<Option<Uuid>> {
    let import_token = Uuid::new_v4();
    let params = import_job_params(
        import_token,
        analysis.site_id,
        &analysis.site_name,
        req,
        &analysis.parsed,
        overlapping,
        param_streams(&analysis.plan.mappings, api_streams),
    );
    let txn = db.begin().await?;
    stage_import_rows(&txn, import_token, staged).await?;
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
    Ok(job_id)
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
        .route("/readings/edits/sets/{set_id}", get(get_edit_set))
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
        .route("/readings/override", post(override_output))
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
