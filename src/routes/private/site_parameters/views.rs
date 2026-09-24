//! The site-parameter handlers: applying a parameter group to a site, and merging two slots.
//!
//! A calculation applies at a site when the site declares what it reads (Q193, narrowing Q98), so
//! the site parameters *are* the declaration and `apply_group` is the flow that writes it; the
//! output slots follow, minted by the run that first computes them. One
//! action per group rather than one per member: pCO2, DIC and Chl a carry roughly 45 stage-1
//! intermediates between them (Q95), and there are 23 CNET stations. Applying twice adds only what
//! is missing, so a group that grows is applied again rather than diffed by hand.

use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use sea_orm::ColumnTrait;
use sea_orm::EntityTrait;
use sea_orm::QueryFilter;
use sea_orm::QueryOrder;
use sea_orm::QuerySelect;
use sea_orm::TransactionTrait;
use uuid::Uuid;

use super::models::ActiveModel;
use super::models::AppliedSlot;
use super::models::ApplyCalculationRequest;
use super::models::ApplyCalculationResponse;
use super::models::ApplyGroupRequest;
use super::models::ApplyGroupResponse;
use super::models::Column;
use super::models::Entity;
use super::models::GroupMember;
use super::service::CalculationMember;
use super::service::MergeSiteParametersRequest;
use super::service::MergeSiteParametersResponse;
use super::service::applied_cadence;
use super::service::partition_calculation;
use super::service::partition_members;
use super::service::slot_cadence;
use super::service::stream_refusal;
use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::scope::Unowned;
use crate::common::scope::project_of_site_parameter;
use crate::common::scope::require_sites_in_scope;
use crate::common::scope::require_target_in_scope;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::parameter_groups::member_model;
use crate::routes::private::parameter_groups::service::rules::Role;
use crate::routes::private::tools;

#[utoipa::path(
    post,
    path = "/api/sites/{site_id}/parameter_groups",
    request_body = ApplyGroupRequest,
    responses(
        (status = 200, body = ApplyGroupResponse),
        (status = 403, description = "The site is outside the caller's projects"),
        (status = 404, description = "No site or no parameter group with this id"),
    ),
    tag = "site_parameters"
)]
pub async fn apply_group(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(site_id): Path<Uuid>,
    Json(payload): Json<ApplyGroupRequest>,
) -> AppResult<Json<ApplyGroupResponse>> {
    require_sites_in_scope(&state.db, &scope, &[site_id]).await?;
    let site = crate::routes::private::sites::models::Entity::find_by_id(site_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {site_id} not found")))?;

    let members = member_model::Entity::find()
        .filter(member_model::Column::GroupId.eq(payload.group_id))
        .order_by_asc(member_model::Column::Ordinal)
        .all(&state.db)
        .await?;
    if members.is_empty() {
        return Err(AppError::NotFound(format!(
            "Parameter group {} holds no members",
            payload.group_id
        )));
    }

    let codes: std::collections::HashMap<Uuid, String> =
        crate::routes::private::parameters::Entity::find()
            .filter(
                crate::routes::private::parameters::Column::Id
                    .is_in(members.iter().map(|m| m.parameter_id)),
            )
            .all(&state.db)
            .await?
            .into_iter()
            .map(|p| (p.id, p.code))
            .collect();

    // The role follows from the calculations, never from the membership row (Q135).
    let roles =
        crate::routes::private::parameter_groups::service::calculation_roles(&state.db).await?;
    let rows: Vec<GroupMember> = members
        .iter()
        .map(|m| {
            (
                m.parameter_id,
                codes.get(&m.parameter_id).cloned().unwrap_or_default(),
                roles
                    .get(&m.parameter_id)
                    .copied()
                    .unwrap_or(Role::EntryOnly)
                    .as_str()
                    .to_string(),
            )
        })
        .collect();

    let held: std::collections::HashSet<Uuid> = Entity::find()
        .filter(Column::SiteId.eq(site_id))
        .select_only()
        .column(Column::ParameterId)
        .into_tuple::<Uuid>()
        .all(&state.db)
        .await?
        .into_iter()
        .collect();

    let (to_create, already) = partition_members(&rows, &held);
    let slot = |(parameter_id, parameter_code, role): &GroupMember,
                site_parameter_id: Option<Uuid>| AppliedSlot {
        parameter_id: *parameter_id,
        parameter_code: parameter_code.clone(),
        role: role.clone(),
        site_parameter_id,
    };

    if payload.dry_run {
        return Ok(Json(ApplyGroupResponse {
            site_id,
            group_id: payload.group_id,
            dry_run: true,
            created: to_create.iter().map(|m| slot(m, None)).collect(),
            existing: already.iter().map(|m| slot(m, None)).collect(),
        }));
    }

    // One transaction: a half-applied group is a site whose calculations partly apply, which is
    // the state this whole flow exists to prevent.
    let txn = state.db.begin().await?;
    // The change-audit trigger reads the writer from the transaction it fires in.
    crate::common::actor::declare(&txn).await?;
    let mut created = Vec::with_capacity(to_create.len());
    for member in &to_create {
        let id = Uuid::new_v4();
        ActiveModel {
            id: Set(id),
            site_id: Set(site_id),
            parameter_id: Set(member.0),
            name: Set(format!("{} {}", site.name, member.1).trim().to_string()),
            sensor_type: Set(String::new()),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
            needs_review: Set(false),
            instrument_sensor_id: Set(payload.instrument_sensor_id),
            // A group is the form a visit is entered on, so the slots it applies are the visit
            // arm's and the chain computes their calculated members there.
            cadence: Set("low".to_string()),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        created.push(slot(member, Some(id)));
    }
    txn.commit().await?;

    if !created.is_empty() {
        crate::common::cache::invalidate_site(&state.response_cache, site_id);
    }

    Ok(Json(ApplyGroupResponse {
        site_id,
        group_id: payload.group_id,
        dry_run: false,
        created,
        existing: already.iter().map(|m| slot(m, None)).collect(),
    }))
}

/// Apply a calculation at a site: check the site declares everything the calculation reads, and
/// mint the output slots it lacks.
///
/// Refused while an input is undeclared, naming each one: the calculation would be
/// `not_applicable` there, learned after the fact from a job log. The outputs a successful apply
/// mints are the site's declaration of the calculation, so they carry no review flag; the ones the
/// chain mints on its own still do (Q193). Applying twice creates nothing the second time.
#[utoipa::path(
    post,
    path = "/api/sites/{site_id}/calculations",
    request_body = ApplyCalculationRequest,
    responses(
        (status = 200, body = ApplyCalculationResponse),
        (status = 400, description = "The site does not declare every parameter the calculation reads"),
        (status = 403, description = "The site is outside the caller's projects"),
        (status = 404, description = "No site or no calculation with this id"),
        (status = 409, description = "The calculation is switched off"),
    ),
    tag = "site_parameters"
)]
pub async fn apply_calculation(
    State(state): State<AppState>,
    ProjectScope(scope): ProjectScope,
    Path(site_id): Path<Uuid>,
    Json(payload): Json<ApplyCalculationRequest>,
) -> AppResult<Json<ApplyCalculationResponse>> {
    require_sites_in_scope(&state.db, &scope, &[site_id]).await?;
    let site = crate::routes::private::sites::models::Entity::find_by_id(site_id)
        .one(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Site {site_id} not found")))?;
    let tool = tools::service::find_active_tool_by_id(&state.db, payload.calculation_id).await?;
    let catalog =
        tools::service::load_parameter_catalog(&state.db, std::iter::once(&tool.manifest)).await?;
    let inputs = tools::flows::read_input_members(&tool, &catalog);
    let outputs: Vec<CalculationMember> = tool
        .manifest
        .outputs
        .iter()
        .filter_map(|o| catalog.resolve(o).map(|p| (p.id, p.code)))
        .collect();
    let declared = tools::flows::declared_parameters(&state.db, site_id).await?;
    let partition = partition_calculation(&inputs, &outputs, &declared);

    let slot = |(parameter_id, parameter_code): &CalculationMember,
                role: &str,
                site_parameter_id: Option<Uuid>| AppliedSlot {
        parameter_id: *parameter_id,
        parameter_code: parameter_code.clone(),
        role: role.to_string(),
        site_parameter_id,
    };
    let answer = |dry_run: bool, outputs_created: Vec<AppliedSlot>| ApplyCalculationResponse {
        site_id,
        calculation_id: payload.calculation_id,
        calculation_name: tool.name.clone(),
        dry_run,
        inputs_present: partition
            .inputs_present
            .iter()
            .map(|m| slot(m, Role::Measured.as_str(), None))
            .collect(),
        inputs_missing: partition
            .inputs_missing
            .iter()
            .map(|m| slot(m, Role::Measured.as_str(), None))
            .collect(),
        outputs_existing: partition
            .outputs_existing
            .iter()
            .map(|m| slot(m, Role::Output.as_str(), None))
            .collect(),
        outputs_created,
    };

    if payload.dry_run {
        let would_create = partition
            .outputs_to_create
            .iter()
            .map(|m| slot(m, Role::Output.as_str(), None))
            .collect();
        return Ok(Json(answer(true, would_create)));
    }

    if !partition.inputs_missing.is_empty() {
        let missing: Vec<&str> = partition
            .inputs_missing
            .iter()
            .map(|(_, code)| code.as_str())
            .collect();
        return Err(AppError::BadRequest(format!(
            "{} does not measure {}, which '{}' reads. Add those parameters to the site before \
             applying it.",
            site.name,
            missing.join(", "),
            tool.name
        )));
    }

    // The cadence the outputs take is read before the transaction opens: it is the site's
    // declaration for the inputs the calculation reads, which this call does not touch.
    let mut input_cadences = Vec::with_capacity(partition.inputs_present.len());
    for (parameter_id, code) in &partition.inputs_present {
        input_cadences.push((
            code.clone(),
            slot_cadence(&state.db, site_id, *parameter_id).await?,
        ));
    }
    let cadence = applied_cadence(
        &input_cadences
            .iter()
            .map(|(_, cadence)| cadence.clone())
            .collect::<Vec<_>>(),
    );
    if cadence == "high"
        && let Some(reason) = stream_refusal(&input_cadences)
    {
        return Err(AppError::BadRequest(format!(
            "'{}' cannot run on the stream at {}: {reason}",
            tool.name, site.name
        )));
    }

    // One transaction: a half-applied calculation is one whose chain writes some of its outputs
    // and mints the rest flagged for review, which is the state this action exists to prevent.
    let txn = state.db.begin().await?;
    crate::common::actor::declare(&txn).await?;
    let mut created = Vec::with_capacity(partition.outputs_to_create.len());
    for member in &partition.outputs_to_create {
        let parameter = crate::routes::private::parameters::Entity::find_by_id(member.0)
            .one(&txn)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Parameter {} not found", member.0)))?;
        let id = Uuid::new_v4();
        ActiveModel {
            id: Set(id),
            site_id: Set(site_id),
            parameter_id: Set(member.0),
            name: Set(parameter.name),
            sensor_type: Set(String::new()),
            is_active: Set(Some(true)),
            is_public: Set(Some(false)),
            // Applied by a person against a checked declaration, so there is nothing to confirm.
            needs_review: Set(false),
            entry_mode: Set("tool".to_string()),
            cadence: Set(cadence.to_string()),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        created.push(slot(member, Role::Output.as_str(), Some(id)));
    }
    txn.commit().await?;

    if !created.is_empty() {
        crate::common::cache::invalidate_site(&state.response_cache, site_id);
    }

    Ok(Json(answer(false, created)))
}

// --- The slot merge action ---

/// Merge two `site_parameters`, absorb `source` into `target`. Moves every slot-keyed table's rows
/// (readings, status events, samples, annotations) and the streams feeding the slot, then deletes
/// the source row. All or nothing: the whole move is one transaction. Requires `write_metadata`.
///
/// Refused with 409 when source and target both hold a grab sample at the same instant: merging two
/// separately collected groups would rewrite the survivor's stored mean, sd and n.
#[utoipa::path(
    post,
    path = "/api/actions/merge_site_parameters",
    request_body = MergeSiteParametersRequest,
    responses(
        (status = 200, description = "Counts of moved rows and source deletion status", body = MergeSiteParametersResponse),
        (status = 403, description = "Either slot is outside the caller's projects"),
        (status = 404, description = "Source or target not found"),
        (status = 409, description = "Source and target hold a sample at the same instant"),
    ),
    tag = "actions"
)]
pub async fn merge_site_parameters_handler(
    State(state): State<AppState>,
    axum::Extension(auth): axum::Extension<crate::common::middleware::AuthContext>,
    ProjectScope(scope): ProjectScope,
    Json(payload): Json<MergeSiteParametersRequest>,
) -> AppResult<Json<serde_json::Value>> {
    // Both slots must be in scope: absorbing one slot into another is a write to both sides, so a
    // merge spanning two projects is a cross-project write even when one side is granted. Refuse
    // before enqueueing, otherwise a refused request leaves a job that performs the merge anyway.
    for site_parameter_id in [
        payload.source_site_parameter_id,
        payload.target_site_parameter_id,
    ] {
        let row = project_of_site_parameter(&state.db, site_parameter_id).await?;
        require_target_in_scope(&scope, &row, Unowned::Deny, "site parameter")?;
    }

    // Background the multi-table move on the worker pool; the job's `detail` carries the counts.
    // Alarm reconcile runs on job completion (central lifecycle).
    let trigger_id = payload.source_site_parameter_id;
    let job_id = crate::routes::private::reprocessing_jobs::service::enqueue(
        &state.db,
        "merge_site_parameters",
        None,
        Some(trigger_id),
        &serde_json::json!({
            "source_site_parameter_id": payload.source_site_parameter_id,
            "target_site_parameter_id": payload.target_site_parameter_id,
            "actor": crate::common::actor::label(&auth),
            "origin": auth.origin().as_str(),
        }),
        None,
    )
    .await?;
    Ok(Json(
        serde_json::json!({ "job_id": job_id, "status": "queued" }),
    ))
}
