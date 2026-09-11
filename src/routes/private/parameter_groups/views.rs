//! The two handlers a parameter group serves: the definition document a form renders from, and
//! the declaration of the intermediates a group's calculation computes.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, Set,
    Statement,
};
use uuid::Uuid;

use super::models::*;
use super::service::ordering::Column;
use super::service::rules::Role;
use super::service::{
    calculation_roles, lowered_code_in, manifest_sections, ordering, plan_for, site_declarations,
};
use crate::common::state::AppState;
use crate::error::{AppError, AppResult};
use crate::routes::private::parameters;
use sea_orm::FromQueryResult;

/// GET /api/parameter_groups/{id}/definition
#[utoipa::path(
    get,
    path = "/api/parameter_groups/{id}/definition",
    params(("id" = Uuid, Path, description = "Parameter group id"), DefinitionQuery),
    responses((status = 200, body = GroupDefinition)),
    tag = "parameter_groups"
)]
pub async fn group_definition(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<DefinitionQuery>,
) -> AppResult<Json<GroupDefinition>> {
    let header = group_model::Entity::find_by_id(id)
        .one(&state.db)
        .await
        .map_err(AppError::Database)?
        .ok_or_else(|| AppError::NotFound(format!("parameter group {id}")))?;

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT m.parameter_id, p.code, COALESCE(m.label, p.name) AS label, \
                    COALESCE(m.units, NULLIF(p.default_units, '')) AS units, \
                    COALESCE(m.description, p.description) AS description, \
                    m.ordinal, m.replicates \
               FROM parameter_group_members m \
               JOIN parameters p ON p.id = m.parameter_id \
              WHERE m.group_id = $1",
            [id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    let sections_by_code = manifest_sections(&state.db, id).await?;
    // The role follows from the calculations, never from the stored column (Q135): a parameter a
    // formula writes is an output, one a formula reads is measured, the rest are entered and read
    // by nothing.
    let roles = calculation_roles(&state.db).await?;
    let declared = match query.site_id {
        Some(site_id) => site_declarations(&state.db, id, site_id).await?,
        None => std::collections::HashMap::new(),
    };

    let mut members = Vec::with_capacity(rows.len());
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let member = MemberRow::from_query_result(&row, "").map_err(AppError::Database)?;
        let section = sections_by_code.get(&member.code.to_lowercase()).cloned();
        columns.push(Column {
            parameter_id: member.parameter_id,
            code: member.code.clone(),
            ordinal: member.ordinal,
            role: *roles.get(&member.parameter_id).unwrap_or(&Role::EntryOnly),
            section: section.clone(),
        });
        // Places are the slot's declaration or nothing at all: a group declares none (Q120), and
        // nothing here invents one, so an undeclared slot renders through the formatter's own
        // fallback like every other value in the app (Q128).
        let slot = declared.get(&member.parameter_id);
        let decimal_places = slot.and_then(|d| d.decimal_places).map(i32::from);
        let statistics = member_statistics(
            &member.code,
            member.replicates.as_ref(),
            decimal_places,
            slot.and_then(|d| d.sd_estimator.clone()),
        );
        members.push(DefinitionMember {
            parameter_id: member.parameter_id,
            code: member.code,
            label: member.label,
            units: member.units,
            decimal_places,
            description: member.description,
            role: roles
                .get(&member.parameter_id)
                .unwrap_or(&Role::EntryOnly)
                .as_str()
                .to_string(),
            ordinal: member.ordinal,
            section,
            replicates: member.replicates,
            statistics,
        });
    }

    // One ordering, shared with the grid and the Toolbox: the member ordinal, never the manifest.
    let order: Vec<Uuid> = ordering::column_order(&columns)
        .into_iter()
        .map(|c| c.parameter_id)
        .collect();
    members.sort_by_key(|m| {
        order
            .iter()
            .position(|id| *id == m.parameter_id)
            .unwrap_or(usize::MAX)
    });
    let sections = ordering::section_order(&columns);

    Ok(Json(GroupDefinition {
        id: header.id,
        code: header.code,
        label: header.label,
        description: header.description,
        ordinal: header.ordinal,
        members,
        sections,
    }))
}

/// `POST /parameter_groups/{id}/intermediates`: declare the intermediates a group's calculation
/// computes, minting the catalog parameters it names and adding them to the group.
#[utoipa::path(
    post,
    path = "/api/parameter_groups/{id}/intermediates",
    params(("id" = Uuid, Path, description = "Parameter group id")),
    request_body = DeclareIntermediatesRequest,
    responses(
        (status = 200, body = DeclareIntermediatesResponse),
        (status = 400, description = "A code the catalog holds under another group"),
        (status = 404, description = "Unknown group"),
    ),
    tag = "parameters"
)]
pub async fn declare_intermediates(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Json(payload): Json<DeclareIntermediatesRequest>,
) -> AppResult<Json<DeclareIntermediatesResponse>> {
    let db = &state.db;
    if group_model::Entity::find_by_id(group_id)
        .one(db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Parameter group not found".to_string()));
    }

    let codes: Vec<String> = payload
        .intermediates
        .iter()
        .map(|i| i.code.to_lowercase())
        .collect();
    if codes.is_empty() {
        return Err(AppError::BadRequest(
            "Declare at least one intermediate".to_string(),
        ));
    }

    let catalog: Vec<(String, Uuid)> = parameters::Entity::find()
        .filter(lowered_code_in(&codes))
        .all(db)
        .await?
        .into_iter()
        .map(|parameter| (parameter.code, parameter.id))
        .collect();

    let members: Vec<Uuid> = member_model::Entity::find()
        .filter(member_model::Column::GroupId.eq(group_id))
        .select_only()
        .column(member_model::Column::ParameterId)
        .into_tuple::<Uuid>()
        .all(db)
        .await?;

    let mut declared = Vec::with_capacity(payload.intermediates.len());
    let (mut parameters_created, mut members_created) = (0usize, 0usize);
    for item in &payload.intermediates {
        let plan = plan_for(&item.code, &catalog, &members);
        let parameter_id = if plan.mint_parameter {
            let minted = parameters::ActiveModel {
                id: Set(Uuid::new_v4()),
                code: Set(item.code.clone()),
                name: Set(item.name.clone()),
                default_units: Set(item.units.clone().unwrap_or_default()),
                category: Set("measurement".to_string()),
                description: Set(Some(item.description.clone().unwrap_or_default())),
                ..Default::default()
            }
            .insert(db)
            .await?;
            parameters_created += 1;
            minted.id
        } else {
            catalog
                .iter()
                .find(|(c, _)| c.eq_ignore_ascii_case(&item.code))
                .map(|(_, id)| *id)
                .ok_or_else(|| AppError::Internal("Catalog row vanished".to_string()))?
        };

        // The membership goes through the same refusals a hand-added member meets: a parameter
        // already grouped elsewhere is refused here rather than moved.
        let member_created = if plan.add_member {
            let next_ordinal = member_model::Entity::find()
                .filter(member_model::Column::GroupId.eq(group_id))
                .select_only()
                .column_as(member_model::Column::Ordinal.max(), "ordinal")
                .into_tuple::<Option<i32>>()
                .one(db)
                .await?
                .flatten()
                .map_or(0, |highest| highest + 1);
            member_model::ActiveModel {
                id: Set(Uuid::new_v4()),
                group_id: Set(group_id),
                parameter_id: Set(parameter_id),
                ordinal: Set(next_ordinal),
                role: Set("output".to_string()),
                replicates: Set(item.replicates.clone()),
                ..Default::default()
            }
            .insert(db)
            .await
            .map_err(|e| {
                AppError::BadRequest(format!("{} could not join the group: {e}", item.code))
            })?;
            members_created += 1;
            true
        } else {
            false
        };

        declared.push(DeclaredResult {
            code: item.code.clone(),
            parameter_id,
            parameter_created: plan.mint_parameter,
            member_created,
        });
    }

    Ok(Json(DeclareIntermediatesResponse {
        group_id,
        declared,
        parameters_created,
        members_created,
    }))
}

#[cfg(test)]
#[path = "tests/intermediates.rs"]
mod tests;
