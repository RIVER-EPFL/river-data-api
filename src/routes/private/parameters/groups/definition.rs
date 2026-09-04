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
}

/// GET /api/parameter_groups/{id}/definition
#[utoipa::path(
    get,
    path = "/parameter_groups/{id}/definition",
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
              WHERE m.group_id = $1 \
              ORDER BY m.ordinal, p.code",
            [id.into()],
        ))
        .await
        .map_err(AppError::Database)?;

    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        members.push(DefinitionMember {
            parameter_id: row
                .try_get("", "parameter_id")
                .map_err(AppError::Database)?,
            code: row.try_get("", "code").map_err(AppError::Database)?,
            label: row.try_get("", "label").map_err(AppError::Database)?,
            units: row.try_get("", "units").map_err(AppError::Database)?,
            decimal_places: row
                .try_get("", "decimal_places")
                .map_err(AppError::Database)?,
            description: row.try_get("", "description").map_err(AppError::Database)?,
            role: row.try_get("", "role").map_err(AppError::Database)?,
            ordinal: row.try_get("", "ordinal").map_err(AppError::Database)?,
            replicates: row.try_get("", "replicates").map_err(AppError::Database)?,
        });
    }

    Ok(Json(GroupDefinition {
        id: group.try_get("", "id").map_err(AppError::Database)?,
        code: group.try_get("", "code").map_err(AppError::Database)?,
        label: group.try_get("", "label").map_err(AppError::Database)?,
        description: group
            .try_get("", "description")
            .map_err(AppError::Database)?,
        ordinal: group.try_get("", "ordinal").map_err(AppError::Database)?,
        members,
    }))
}
