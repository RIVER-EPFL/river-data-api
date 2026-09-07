//! The group definition document: the one thing the visit grid, the tool form and the Toolbox
//! render from. Members in the group's own order, each with the role, units and decimals the group
//! declares over the catalog's.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use sea_orm::{ConnectionTrait, FromQueryResult, Statement};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};

use super::ordering::{self, Column};
use super::rules::Role;

/// The two columns a replicated member also shows: the mean and the sd the `samples` trigger
/// computes over its replicates. Read-only wherever they render, since nothing writes them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct MemberStatistics {
    pub mean_label: String,
    pub sd_label: String,
    /// The divisor the slot declares, NULL where it declares none. Never inferred.
    pub sd_estimator: Option<String>,
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
    role: String,
    ordinal: i32,
    decimal_places: Option<i32>,
    replicates: Option<serde_json::Value>,
}

#[derive(FromQueryResult)]
struct GroupRow {
    id: Uuid,
    code: String,
    label: String,
    description: Option<String>,
    ordinal: i32,
}

#[derive(FromQueryResult)]
struct DeclaredEstimator {
    parameter_id: Uuid,
    sd_estimator: Option<String>,
}

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
    /// The mean and sd columns a replicated member also shows. Absent on a member entered once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statistics: Option<MemberStatistics>,
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
    params(("id" = Uuid, Path, description = "Parameter group id"), DefinitionQuery),
    responses((status = 200, body = GroupDefinition)),
    tag = "parameter_groups"
)]
pub async fn group_definition(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<DefinitionQuery>,
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
    let estimators = match query.site_id {
        Some(site_id) => declared_estimators(&state.db, id, site_id).await?,
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
            role: Role::parse(&member.role).unwrap_or(Role::EntryOnly),
            section: section.clone(),
        });
        let statistics = member_statistics(
            &member.code,
            member.replicates.as_ref(),
            member.decimal_places,
            estimators.get(&member.parameter_id).cloned().flatten(),
        );
        members.push(DefinitionMember {
            parameter_id: member.parameter_id,
            code: member.code,
            label: member.label,
            units: member.units,
            decimal_places: member.decimal_places,
            description: member.description,
            role: member.role,
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

    let header = GroupRow::from_query_result(&group, "").map_err(AppError::Database)?;
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

/// What each of the group's parameters declares as its sd estimator at one site. A slot with no
/// row, or a row declaring none, carries NULL: the divisor is a declaration and is never inferred.
async fn declared_estimators(
    db: &sea_orm::DatabaseConnection,
    group_id: Uuid,
    site_id: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, Option<String>>> {
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sp.parameter_id, sp.sd_estimator \
               FROM site_parameters sp \
               JOIN parameter_group_members m ON m.parameter_id = sp.parameter_id \
              WHERE m.group_id = $1 AND sp.site_id = $2",
            [group_id.into(), site_id.into()],
        ))
        .await
        .map_err(AppError::Database)?;
    let mut declared = std::collections::HashMap::new();
    for row in rows {
        let row = DeclaredEstimator::from_query_result(&row, "").map_err(AppError::Database)?;
        declared.insert(row.parameter_id, row.sd_estimator);
    }
    Ok(declared)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::private::parameters::groups::ordering::{self, Column};
    use crate::routes::private::parameters::groups::rules::Role;

    fn spec() -> serde_json::Value {
        serde_json::json!({ "positions": 3 })
    }

    #[test]
    fn test_a_replicated_member_shows_a_mean_and_an_sd() {
        let stats = member_statistics("doc", Some(&spec()), Some(2), Some("sample".into()))
            .expect("a replicated member carries statistics");
        assert_eq!(stats.mean_label, "doc mean");
        assert_eq!(stats.sd_label, "doc sd");
        assert_eq!(stats.sd_estimator.as_deref(), Some("sample"));
        assert_eq!(stats.decimal_places, Some(2));
    }

    #[test]
    fn test_a_member_entered_once_shows_none() {
        assert_eq!(
            member_statistics("ph", None, Some(2), Some("sample".into())),
            None
        );
    }

    #[test]
    fn test_an_undeclared_estimator_stays_undeclared() {
        let stats = member_statistics("doc", Some(&spec()), None, None).expect("statistics");
        assert_eq!(stats.sd_estimator, None);
    }

    // The statistics are the member's own columns, not members of the group, so the order the grid
    // and the Toolbox share stays the members' own.
    #[test]
    fn test_statistics_are_not_columns_of_their_own() {
        let columns = [
            Column {
                parameter_id: Uuid::new_v4(),
                code: "doc".into(),
                ordinal: 1,
                role: Role::Measured,
                section: None,
            },
            Column {
                parameter_id: Uuid::new_v4(),
                code: "ph".into(),
                ordinal: 2,
                role: Role::Measured,
                section: None,
            },
        ];
        let order: Vec<&str> = ordering::column_order(&columns)
            .into_iter()
            .map(|c| c.code.as_str())
            .collect();
        assert_eq!(order, vec!["doc", "ph"]);
    }
}
