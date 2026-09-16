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
use super::models::ApplyGroupRequest;
use super::models::ApplyGroupResponse;
use super::models::Column;
use super::models::Entity;
use super::models::GroupMember;
use super::service::MergeSiteParametersRequest;
use super::service::MergeSiteParametersResponse;
use super::service::partition_members;
use crate::common::AppState;
use crate::common::middleware::ProjectScope;
use crate::common::scope::Unowned;
use crate::common::scope::project_of_site_parameter;
use crate::common::scope::require_target_in_scope;
use crate::error::AppError;
use crate::error::AppResult;
use crate::routes::private::parameter_groups::member_model;
use crate::routes::private::parameter_groups::service::rules::Role;

#[utoipa::path(
    post,
    path = "/api/sites/{site_id}/parameter_groups",
    request_body = ApplyGroupRequest,
    responses(
        (status = 200, body = ApplyGroupResponse),
        (status = 404, description = "No site or no parameter group with this id"),
    ),
    tag = "site_parameters"
)]
pub async fn apply_group(
    State(state): State<AppState>,
    Path(site_id): Path<Uuid>,
    Json(payload): Json<ApplyGroupRequest>,
) -> AppResult<Json<ApplyGroupResponse>> {
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

    // Background the multi-table move on the worker pool; the job's `detail` carries the counts the
    // UI used to read synchronously. Alarm reconcile runs on job completion (central lifecycle).
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
