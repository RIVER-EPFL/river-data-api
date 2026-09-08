//! Applying a parameter group to a site.
//!
//! Under Q98 a calculation applies at a site when the group's output slots are configured there,
//! so the site parameters *are* the declaration and this is the flow that writes it. One action
//! per group rather than one per member: pCO2, DIC and Chl a carry roughly 45 stage-1
//! intermediates between them (Q95), and there are 23 CNET stations.
//!
//! Applying twice adds only what is missing, so a group that grows is applied again rather than
//! diffed by hand.

use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::parameters::groups::member_model;
use crate::routes::private::sites::parameters::model as site_parameters;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyGroupRequest {
    pub group_id: Uuid,
    /// The instrument that measures these slots at this site, written onto every row this call
    /// creates. Declared per site parameter, never per group: a later row may say otherwise
    /// (M111). Omitted leaves the slots undeclared, which is a legitimate state.
    #[serde(default)]
    pub instrument_sensor_id: Option<Uuid>,
    /// Report what would be created without creating it.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AppliedSlot {
    pub parameter_id: Uuid,
    pub parameter_code: String,
    /// The group's role for this member: `measured`, `entry_only` or `output`.
    pub role: String,
    /// The slot's id, absent on a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub site_parameter_id: Option<Uuid>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ApplyGroupResponse {
    pub site_id: Uuid,
    pub group_id: Uuid,
    pub dry_run: bool,
    /// The slots this call created, or would create.
    pub created: Vec<AppliedSlot>,
    /// Members the site already carried, left exactly as they are.
    pub existing: Vec<AppliedSlot>,
}

/// One member of a group as this flow reads it: the catalog parameter, its code, and the role the
/// group gives it.
pub type GroupMember = (Uuid, String, String);

/// The group's members and the site's slots, decided in one place so the route and its test agree
/// on what "already carried" means: a slot exists for a member when the site holds a row for that
/// catalog parameter, whatever the group says about it.
#[must_use]
pub fn partition_members(
    members: &[GroupMember],
    held: &std::collections::HashSet<Uuid>,
) -> (Vec<GroupMember>, Vec<GroupMember>) {
    let mut create = Vec::new();
    let mut existing = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for member in members {
        if held.contains(&member.0) {
            existing.push(member.clone());
        } else if seen.insert(member.0) {
            create.push(member.clone());
        }
    }
    (create, existing)
}

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
    let site = crate::routes::private::sites::Entity::find_by_id(site_id)
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

    let rows: Vec<GroupMember> = members
        .iter()
        .map(|m| {
            (
                m.parameter_id,
                codes.get(&m.parameter_id).cloned().unwrap_or_default(),
                m.role.clone(),
            )
        })
        .collect();

    let held: std::collections::HashSet<Uuid> = site_parameters::Entity::find()
        .filter(site_parameters::Column::SiteId.eq(site_id))
        .select_only()
        .column(site_parameters::Column::ParameterId)
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
        site_parameters::ActiveModel {
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
