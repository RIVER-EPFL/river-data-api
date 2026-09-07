//! Declaring the intermediates a group's calculation computes.
//!
//! Q95 settled that a two-stage calculation's stage-1 values are stored parameters, one reading per
//! replicate index, with the `samples` trigger deriving the mean and sd. There are roughly 45 of
//! them across pCO2, DIC, Chl a and Nutrients, and creating each catalog parameter by hand before
//! the group can name it is the ergonomics Evan refused. The group declares them here instead: each
//! is found or created by `code`, exactly as a derived definition's output parameter is, and joins
//! the group as an `output`, which is what it is — a value the group's calculation produces.

use axum::{
    Json,
    extract::{Path, State},
};
use sea_orm::{ConnectionTrait, Statement};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::common::state::AppState;
use crate::error::{AppError, AppResult};

/// One intermediate, as the group declares it.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclaredIntermediate {
    /// The catalog code, which is the identity: a re-declaration finds the same row.
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub units: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The replicate spec of the family this intermediate is computed over, member-shaped, so a
    /// stage-1 output entered at several indexes carries its statistics like any other family.
    #[serde(default)]
    pub replicates: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DeclareIntermediatesRequest {
    pub intermediates: Vec<DeclaredIntermediate>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeclaredResult {
    pub code: String,
    pub parameter_id: Uuid,
    /// Whether this call created the catalog parameter, rather than finding it.
    pub parameter_created: bool,
    /// Whether this call added the membership, rather than finding it.
    pub member_created: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeclareIntermediatesResponse {
    pub group_id: Uuid,
    pub declared: Vec<DeclaredResult>,
    pub parameters_created: usize,
    pub members_created: usize,
}

/// What a declaration has to do, decided before anything is written: a code the catalog already
/// holds is reused, and a parameter the group already carries gains no second membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub mint_parameter: bool,
    pub add_member: bool,
}

/// Fold what exists into what each declared code needs. Codes are compared case-insensitively,
/// which is how the catalog's uniqueness is defined (`UNIQUE (LOWER(code))`).
#[must_use]
pub fn plan_for(code: &str, catalog: &[(String, Uuid)], members: &[Uuid]) -> Plan {
    let existing = catalog
        .iter()
        .find(|(c, _)| c.eq_ignore_ascii_case(code))
        .map(|(_, id)| *id);
    Plan {
        mint_parameter: existing.is_none(),
        add_member: existing.is_none_or(|id| !members.contains(&id)),
    }
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
    if db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM parameter_groups WHERE id = $1",
            [group_id.into()],
        ))
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

    let catalog: Vec<(String, Uuid)> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT code, id FROM parameters WHERE LOWER(code) = ANY($1)",
            [codes.into()],
        ))
        .await?
        .iter()
        .map(|row| -> AppResult<(String, Uuid)> {
            Ok((row.try_get("", "code")?, row.try_get("", "id")?))
        })
        .collect::<AppResult<Vec<_>>>()?;

    let members: Vec<Uuid> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT parameter_id FROM parameter_group_members WHERE group_id = $1",
            [group_id.into()],
        ))
        .await?
        .iter()
        .map(|row| row.try_get::<Uuid>("", "parameter_id"))
        .collect::<Result<Vec<_>, _>>()?;

    let mut declared = Vec::with_capacity(payload.intermediates.len());
    let (mut parameters_created, mut members_created) = (0usize, 0usize);
    for item in &payload.intermediates {
        let plan = plan_for(&item.code, &catalog, &members);
        let parameter_id = if plan.mint_parameter {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "INSERT INTO parameters (id, code, name, default_units, category, description) \
                     VALUES (gen_random_uuid(), $1, $2, $3, 'measurement', $4) RETURNING id",
                    [
                        item.code.clone().into(),
                        item.name.clone().into(),
                        item.units.clone().unwrap_or_default().into(),
                        item.description.clone().unwrap_or_default().into(),
                    ],
                ))
                .await?
                .ok_or_else(|| {
                    AppError::Internal("No row returned from parameter insert".to_string())
                })?;
            parameters_created += 1;
            row.try_get::<Uuid>("", "id")?
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
            db.execute_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO parameter_group_members \
                 (id, group_id, parameter_id, ordinal, role, replicates) \
                 VALUES (gen_random_uuid(), $1, $2, \
                         COALESCE((SELECT MAX(ordinal) + 1 FROM parameter_group_members \
                                   WHERE group_id = $1), 0), \
                         'output', $3)",
                [group_id.into(), parameter_id.into(), item.replicates.clone().into()],
            ))
            .await
            .map_err(|e| {
                AppError::BadRequest(format!(
                    "{} could not join the group: {e}",
                    item.code
                ))
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
mod tests {
    use super::plan_for;
    use uuid::Uuid;

    #[test]
    fn a_code_the_catalog_holds_is_reused_and_a_member_is_added_once() {
        let doc = Uuid::new_v4();
        let catalog = vec![("DOC_A".to_string(), doc)];

        let fresh = super::plan_for("CO2_HS_Um_A", &catalog, &[]);
        assert!(fresh.mint_parameter, "a code nothing holds is minted");
        assert!(fresh.add_member);

        let existing = plan_for("doc_a", &catalog, &[]);
        assert!(
            !existing.mint_parameter,
            "the catalog's uniqueness is case-insensitive, so this is the same parameter"
        );
        assert!(existing.add_member);

        let already = plan_for("DOC_A", &catalog, &[doc]);
        assert!(!already.mint_parameter);
        assert!(
            !already.add_member,
            "re-declaring what the group already carries adds nothing"
        );
    }
}
