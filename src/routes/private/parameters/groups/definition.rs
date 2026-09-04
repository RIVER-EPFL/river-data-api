//! The group definition document: the one thing the visit grid, the tool form and the Toolbox
//! render from. Members in the group's own order, each with the role, units and decimals the group
//! declares over the catalog's.

use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement};
use serde::Serialize;
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};

use super::ordering::{self, Column};
use super::rules::Role;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DefinitionMember {
    pub parameter_id: Uuid,
    /// The catalog code; the stable machine identity and the CSV column header.
    pub code: String,
    /// The group's label over the catalog's name.
    pub label: String,
    pub units: Option<String>,
    pub decimal_places: Option<i32>,
    pub description: Option<String>,
    pub role: String,
    pub ordinal: i32,
    /// Display hint: the manifest section the group's calculation renders this field under. It
    /// labels a run of columns and never reorders them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replicates: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GroupDefinition {
    pub id: Uuid,
    pub code: String,
    pub label: String,
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
    params(("id" = Uuid, Path, description = "Parameter group id")),
    responses((status = 200, body = GroupDefinition)),
    tag = "parameter_groups"
)]
pub async fn group_definition(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<GroupDefinition>> {
    let group = state
        .db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id, code, label, description, ordinal FROM parameter_groups WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(AppError::Database)?
        .ok_or_else(|| AppError::NotFound(format!("parameter group {id}")))?;

    let rows = state
        .db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT m.parameter_id, p.code, COALESCE(m.label, p.name) AS label, \
                    COALESCE(m.units, NULLIF(p.default_units, '')) AS units, \
                    COALESCE(m.decimal_places, NULL) AS decimal_places, \
                    COALESCE(m.description, p.description) AS description, \
                    m.role, m.ordinal, m.replicates \
               FROM parameter_group_members m \
               JOIN parameters p ON p.id = m.parameter_id \
              WHERE m.group_id = $1",
            [id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    let sections_by_code = manifest_sections(&state.db, id).await?;

    let mut members = Vec::with_capacity(rows.len());
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let code: String = row.try_get("", "code").map_err(AppError::Database)?;
        let role: String = row.try_get("", "role").map_err(AppError::Database)?;
        let parameter_id: Uuid = row
            .try_get("", "parameter_id")
            .map_err(AppError::Database)?;
        let ordinal: i32 = row.try_get("", "ordinal").map_err(AppError::Database)?;
        let section = sections_by_code.get(&code.to_lowercase()).cloned();
        columns.push(Column {
            parameter_id,
            code: code.clone(),
            ordinal,
            role: Role::parse(&role).unwrap_or(Role::EntryOnly),
            section: section.clone(),
        });
        members.push(DefinitionMember {
            parameter_id,
            code,
            label: row.try_get("", "label").map_err(AppError::Database)?,
            units: row.try_get("", "units").map_err(AppError::Database)?,
            decimal_places: row
                .try_get("", "decimal_places")
                .map_err(AppError::Database)?,
            description: row.try_get("", "description").map_err(AppError::Database)?,
            role,
            ordinal,
            section,
            replicates: row.try_get("", "replicates").map_err(AppError::Database)?,
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
        id: group.try_get("", "id").map_err(AppError::Database)?,
        code: group.try_get("", "code").map_err(AppError::Database)?,
        label: group.try_get("", "label").map_err(AppError::Database)?,
        description: group
            .try_get("", "description")
            .map_err(AppError::Database)?,
        ordinal: group.try_get("", "ordinal").map_err(AppError::Database)?,
        members,
        sections,
    }))
}

/// The section each field renders under, by catalog code, taken from the active manifest of the
/// calculation bound to this group. Empty where no calculation is bound or it declares none.
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
