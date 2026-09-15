//! The handler a parameter group serves: the definition document a form renders from.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Statement};
use uuid::Uuid;

use super::models::*;
use super::service::ordering::Column;
use super::service::rules::Role;
use super::service::{calculation_roles, manifest_sections, ordering, site_declarations};
use crate::common::state::AppState;
use crate::error::{AppError, AppResult};
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
                    m.ordinal, m.replicates, m.source_calculation \
               FROM parameter_group_members m \
               JOIN parameters p ON p.id = m.parameter_id \
              WHERE m.group_id = $1",
            [id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    let sections_by_code = manifest_sections(&state.db, id).await?;
    // The role follows from the calculations (Q135): a parameter a formula writes is an output,
    // one a formula reads is measured, the rest are entered and read by nothing.
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
            source_calculation: member.source_calculation,
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
    let calculations = group_calculations(&state.db, id).await?;

    Ok(Json(GroupDefinition {
        id: header.id,
        code: header.code,
        label: header.label,
        description: header.description,
        ordinal: header.ordinal,
        members,
        sections,
        calculations,
    }))
}

/// The calculations of a group's columns: those whose active version reads or publishes one of its
/// members, named as the page links them.
async fn group_calculations(
    db: &sea_orm::DatabaseConnection,
    group_id: Uuid,
) -> AppResult<Vec<GroupCalculation>> {
    use crate::routes::private::tools::models::script;
    let names: Vec<String> =
        crate::routes::private::tools::service::calculations_of_group(db, group_id)
            .await?
            .into_iter()
            .map(|calculation| calculation.name)
            .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let mut rows: Vec<GroupCalculation> = script::Entity::find()
        .filter(script::Column::Name.is_in(names))
        .all(db)
        .await
        .map_err(AppError::Database)?
        .into_iter()
        .map(|row| GroupCalculation {
            id: row.id,
            name: row.name,
            label: row.label,
        })
        .collect();
    rows.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(rows)
}
