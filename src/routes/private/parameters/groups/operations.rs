use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{ConnectionTrait, EntityTrait, Statement, TransactionTrait};
use uuid::Uuid;

use super::group_model::ParameterGroup;
use super::member_model::ParameterGroupMember;
use super::rules::{self, Member, Role};

pub struct ParameterGroupOperations;

/// One membership row by id, reduced to what the reshape rules read.
async fn member_row<C: ConnectionTrait>(db: &C, id: Uuid) -> Result<Option<Member>, ApiError> {
    let Some(row) = super::member_model::Entity::find_by_id(id)
        .one(db)
        .await
        .map_err(ApiError::database)?
    else {
        return Ok(None);
    };
    let Some(role) = Role::parse(&row.role) else {
        return Ok(None);
    };
    Ok(Some(Member {
        group_id: row.group_id,
        parameter_id: row.parameter_id,
        role,
    }))
}

/// Every membership row, reduced to what the reshape rules read.
async fn all_members<C: ConnectionTrait>(db: &C) -> Result<Vec<Member>, ApiError> {
    let rows = super::member_model::Entity::find()
        .all(db)
        .await
        .map_err(ApiError::database)?;
    let mut members = Vec::with_capacity(rows.len());
    for row in rows {
        // A role outside the vocabulary is a corrupt row, not a decode failure.
        let Some(role) = Role::parse(&row.role) else {
            continue;
        };
        members.push(Member {
            group_id: row.group_id,
            parameter_id: row.parameter_id,
            role,
        });
    }
    Ok(members)
}

/// The candidate's catalog code, and the codes of the group's members entered several times. The
/// statistics rule reads both: what is being added, and what the group already computes.
async fn codes_for_statistics_rule<C: ConnectionTrait>(
    db: &C,
    group_id: Uuid,
    parameter_id: Uuid,
) -> Result<(String, Vec<String>), ApiError> {
    let row = crate::routes::private::parameters::Entity::find_by_id(parameter_id)
        .one(db)
        .await
        .map_err(ApiError::database)?;
    let Some(code) = row.map(|p| p.code) else {
        return Ok((String::new(), Vec::new()));
    };
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.code FROM parameter_group_members m \
               JOIN parameters p ON p.id = m.parameter_id \
              WHERE m.group_id = $1 AND m.replicates IS NOT NULL",
            [group_id.into()],
        ))
        .await
        .map_err(ApiError::database)?;
    let mut replicated = Vec::with_capacity(rows.len());
    for row in rows {
        let code: String = row.try_get("", "code").map_err(ApiError::database)?;
        replicated.push(code);
    }
    Ok((code, replicated))
}

impl CRUDOperations for ParameterGroupOperations {
    type Resource = ParameterGroup;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it (B185).
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// The FK is `ON DELETE RESTRICT`, which would surface as a raw 500; the rule says what to do
    /// about it instead.
    async fn before_delete<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        id: Uuid,
    ) -> Result<(), ApiError> {
        rules::may_delete(id, &all_members(db).await?)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))
    }
}

pub struct ParameterGroupMemberOperations;

impl CRUDOperations for ParameterGroupMemberOperations {
    type Resource = ParameterGroupMember;

    /// The change-audit trigger reads the writer from the transaction, so the label is declared on
    /// every write this entity makes, before any hook or statement on it (B185).
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// A parameter belongs to at most one group. The UNIQUE index is the backstop; this names the
    /// group that already holds it. A group's replicated members carry their own mean and sd, so
    /// the catalog parameters the portals stored those in are refused as members.
    async fn before_create<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        data: &<ParameterGroupMember as CRUDResource>::CreateModel,
    ) -> Result<(), ApiError> {
        if Role::parse(&data.role).is_none() {
            return Err(ApiError::bad_request(format!(
                "role {} is not measured, entry_only or output",
                data.role
            )));
        }
        rules::may_add(data.parameter_id, &all_members(db).await?)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))?;
        let (code, replicated) =
            codes_for_statistics_rule(db, data.group_id, data.parameter_id).await?;
        let replicated: Vec<&str> = replicated.iter().map(String::as_str).collect();
        rules::may_add_code(&code, &replicated)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))
    }

    /// The role CHECK is the backstop; this names the value instead of raising a raw 500. A move
    /// between groups is the reshape, so it is held to [`rules::may_move`]: an `output` does not
    /// leave while a calculation in its group still writes it.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
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
            crate::routes::private::tools::service::calculations_of_group(db, member.group_id)
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        rules::may_move(member, to_group, &calculations)
            .map_err(|refusal| ApiError::bad_request(refusal.to_string()))
    }
}
