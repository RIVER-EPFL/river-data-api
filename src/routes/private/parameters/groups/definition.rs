//! The group definition document: the one thing the visit grid, the tool form and the Toolbox
//! render from. Members in the group's own order, each with the role, units and decimals the group
//! declares over the catalog's.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect, Statement,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};

use super::group_model;
use super::ordering::{self, Column};
use super::rules::Role;
use crate::routes::private::parameters::derived::{definition_model, source_model};

/// The two columns a replicated member also shows: the mean and the sd the `samples` trigger
/// computes over its replicates. Read-only wherever they render, since nothing writes them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct MemberStatistics {
    pub mean_label: String,
    pub sd_label: String,
    /// The divisor the slot declares, NULL where it declares none. Never inferred.
    #[schema(required)]
    pub sd_estimator: Option<String>,
    /// The places the slot declares, NULL where it declares none. Never inferred: the formatter
    /// that renders the number owns the fallback (Q128).
    #[schema(required)]
    pub decimal_places: Option<i32>,
}

/// The statistics a member shows, which is nothing at all unless it is entered several times.
fn member_statistics(
    code: &str,
    replicates: Option<&serde_json::Value>,
    decimal_places: Option<i32>,
    sd_estimator: Option<String>,
) -> Option<MemberStatistics> {
    replicates?;
    Some(MemberStatistics {
        mean_label: format!("{code} mean"),
        sd_label: format!("{code} sd"),
        sd_estimator,
        decimal_places,
    })
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct DefinitionQuery {
    /// The site the group is rendered for; it is what declares the sd estimator.
    pub site_id: Option<Uuid>,
}

/// The rows this file's raw queries return. Derived rather than hand-decoded so a column added to
/// a query and not to its reader is a compile error rather than a field silently left behind.
#[derive(FromQueryResult)]
struct MemberRow {
    parameter_id: Uuid,
    code: String,
    label: String,
    units: Option<String>,
    description: Option<String>,
    ordinal: i32,
    replicates: Option<serde_json::Value>,
}

/// What one slot declares for a member: the divisor and the places, both nullable.
#[derive(FromQueryResult)]
struct SlotDeclaration {
    parameter_id: Uuid,
    sd_estimator: Option<String>,
    decimal_places: Option<i16>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DefinitionMember {
    pub parameter_id: Uuid,
    /// The catalog code; the stable machine identity and the CSV column header.
    pub code: String,
    /// The group's label over the catalog's name.
    pub label: String,
    #[schema(required)]
    pub units: Option<String>,
    /// The places the slot declares, NULL where it declares none (Q128).
    #[schema(required)]
    pub decimal_places: Option<i32>,
    #[schema(required)]
    pub description: Option<String>,
    pub role: String,
    pub ordinal: i32,
    /// Display hint: the manifest section the group's calculation renders this field under. It
    /// labels a run of columns and never reorders them.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<std::collections::HashMap<String, serde_json::Value>>)]
    pub replicates: Option<serde_json::Value>,
    /// The mean and sd columns a replicated member also shows. Absent on a member entered once.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub statistics: Option<MemberStatistics>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GroupDefinition {
    pub id: Uuid,
    pub code: String,
    pub label: String,
    #[schema(required)]
    pub description: Option<String>,
    pub ordinal: i32,
    pub members: Vec<DefinitionMember>,
    /// The section labels, in the order their first column appears. Empty where the group's
    /// calculation declares none.
    pub sections: Vec<String>,
}

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

/// What each of the group's parameters declares at one site: the sd estimator, which is never
/// inferred, and the decimal places a form renders at. A slot with no row, or a row declaring
/// neither, carries NULL for both.
async fn site_declarations(
    db: &sea_orm::DatabaseConnection,
    group_id: Uuid,
    site_id: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, SlotDeclaration>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.parameter_id, sp.sd_estimator, sp.decimal_places \
               FROM site_parameters sp \
               JOIN parameter_group_members m ON m.parameter_id = sp.parameter_id \
              WHERE m.group_id = $1 AND sp.site_id = $2",
            [group_id.into(), site_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    let mut declared = std::collections::HashMap::new();
    for row in rows {
        let row = SlotDeclaration::from_query_result(&row, "").map_err(AppError::Database)?;
        declared.insert(row.parameter_id, row);
    }
    Ok(declared)
}

/// The section each field renders under, by catalog code, taken from the active manifest of the
/// calculation bound to this group. Empty where no calculation is bound or it declares none.
/// What each parameter is to the formulas: written by one, read by one, or neither.
///
/// One pass over every formula and its sources, since a group's definition is read for a page and
/// the calculation set is catalog-sized. A formula's output parameter is its own; its inputs are
/// the `derived_parameter_sources` rows naming it.
async fn calculation_roles(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<std::collections::HashMap<Uuid, Role>> {
    let mut roles = std::collections::HashMap::new();
    // A source naming a site property carries no parameter, so its NULL is skipped rather than
    // decoded.
    let read = source_model::Entity::find()
        .select_only()
        .column(source_model::Column::ParameterId)
        .distinct()
        .into_tuple::<Option<Uuid>>()
        .all(db)
        .await
        .map_err(AppError::Database)?;
    for id in read.into_iter().flatten() {
        roles.insert(id, Role::Measured);
    }
    // Written last: what a formula writes is what the parameter is, even where another reads it.
    let written = definition_model::Entity::find()
        .select_only()
        .column(definition_model::Column::OutputParameterId)
        .distinct()
        .filter(definition_model::Column::OutputParameterId.is_not_null())
        .into_tuple::<Option<Uuid>>()
        .all(db)
        .await
        .map_err(AppError::Database)?;
    for id in written.into_iter().flatten() {
        roles.insert(id, Role::Output);
    }
    Ok(roles)
}

async fn manifest_sections(
    db: &sea_orm::DatabaseConnection,
    group_id: Uuid,
) -> AppResult<std::collections::HashMap<String, String>> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT v.manifest FROM tool_scripts s \
               JOIN tool_script_versions v ON v.id = s.active_version_id \
              WHERE s.parameter_group_id = $1",
            [group_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    let mut sections = std::collections::HashMap::new();
    let Some(row) = row else {
        return Ok(sections);
    };
    let manifest: serde_json::Value = row.try_get("", "manifest").map_err(AppError::Database)?;
    let params = manifest
        .get("params")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for param in params {
        let (Some(code), Some(section)) = (
            param
                .get("parameter_code")
                .and_then(serde_json::Value::as_str),
            param.get("section").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        sections.insert(code.to_lowercase(), section.to_string());
    }
    Ok(sections)
}

#[cfg(test)]
#[path = "tests/definition.rs"]
mod tests;
