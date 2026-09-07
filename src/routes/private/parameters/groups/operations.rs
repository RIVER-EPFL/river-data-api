use async_trait::async_trait;
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use uuid::Uuid;

use super::group_model::ParameterGroup;
use super::member_model::ParameterGroupMember;
use super::rules::{self, Member, Role};

pub struct ParameterGroupOperations;

/// One membership row by id, reduced to what the reshape rules read.
async fn member_row(db: &DatabaseConnection, id: Uuid) -> Result<Option<Member>, ApiError> {
    let Some(row) = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT group_id, parameter_id, role FROM parameter_group_members WHERE id = $1",
            [id.into()],
        ))
        .await
        .map_err(ApiError::database)?
    else {
        return Ok(None);
    };
    let role: String = row.try_get("", "role").map_err(ApiError::database)?;
    let Some(role) = Role::parse(&role) else {
        return Ok(None);
    };
    Ok(Some(Member {
        group_id: row.try_get("", "group_id").map_err(ApiError::database)?,
        parameter_id: row
            .try_get("", "parameter_id")
            .map_err(ApiError::database)?,
        role,
    }))
}

/// Every membership row, reduced to what the reshape rules read.
async fn all_members(db: &DatabaseConnection) -> Result<Vec<Member>, ApiError> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT group_id, parameter_id, role FROM parameter_group_members".to_string(),
        ))
        .await
        .map_err(ApiError::database)?;
    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        let role: String = row.try_get("", "role").map_err(ApiError::database)?;
        let Some(role) = Role::parse(&role) else {
            continue;
        };
        members.push(Member {
            group_id: row.try_get("", "group_id").map_err(ApiError::database)?,
            parameter_id: row
                .try_get("", "parameter_id")
                .map_err(ApiError::database)?,
            role,
        });
    }
    Ok(members)
}

#[async_trait]
impl CRUDOperations for ParameterGroupOperations {
    type Resource = ParameterGroup;

    /// One row at a time, through the single-row path: the crudcrate default `create_many`
    /// delegates to the resource, which delegates back here, so the default recurses; the loop
    /// also runs the single-row hooks for every item.
    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<ParameterGroup as CRUDResource>::CreateModel>,
    ) -> Result<Vec<ParameterGroup>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    /// The FK is `ON DELETE RESTRICT`, which would surface as a raw 500; the rule says what to do
    /// about it instead.
    async fn before_delete(&self, db: &DatabaseConnection, id: Uuid) -> Result<(), ApiError> {
        rules::may_delete(id, &all_members(db).await?)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))
    }

    async fn before_delete_many(
        &self,
        db: &DatabaseConnection,
        ids: &[Uuid],
    ) -> Result<(), ApiError> {
        let members = all_members(db).await?;
        for id in ids {
            rules::may_delete(*id, &members)
                .map_err(|refusal| ApiError::bad_request(refusal.to_string()))?;
        }
        Ok(())
    }
}

pub struct ParameterGroupMemberOperations;

#[async_trait]
impl CRUDOperations for ParameterGroupMemberOperations {
    type Resource = ParameterGroupMember;

    async fn create_many(
        &self,
        db: &DatabaseConnection,
        data: Vec<<ParameterGroupMember as CRUDResource>::CreateModel>,
    ) -> Result<Vec<ParameterGroupMember>, ApiError> {
        let mut created = Vec::with_capacity(data.len());
        for item in data {
            created.push(self.create(db, item).await?);
        }
        Ok(created)
    }

    /// A parameter belongs to at most one group. The UNIQUE index is the backstop; this names the
    /// group that already holds it.
    async fn before_create(
        &self,
        db: &DatabaseConnection,
        data: &<ParameterGroupMember as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if Role::parse(&data.role).is_none() {
            return Err(ApiError::bad_request(format!(
                "role {} is not measured, entry_only or output",
                data.role
            )));
        }
        rules::may_add(data.parameter_id, &all_members(db).await?)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))
    }

    /// The role CHECK is the backstop; this names the value instead of raising a raw 500. A move
    /// between groups is the reshape, so it is held to [`rules::may_move`]: an `output` does not
    /// leave while a calculation in its group still writes it.
    async fn before_update(
        &self,
        db: &DatabaseConnection,
        id: Uuid,
        data: &<ParameterGroupMember as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        if let Some(Some(role)) = data.role.as_ref()
            && Role::parse(role).is_none()
        {
            return Err(ApiError::bad_request(format!(
                "role {role} is not measured, entry_only or output"
            )));
        }
        let Some(Some(to_group)) = data.group_id else {
            return Ok(());
        };
        let Some(member) = member_row(db, id).await? else {
            return Ok(());
        };
        let calculations =
            crate::routes::private::tools::calculation_versions::calculations_of_group(
                db,
                member.group_id,
            )
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        rules::may_move(member, to_group, &calculations)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))
    }
}
