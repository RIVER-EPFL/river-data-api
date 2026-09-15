//! What a parameter group's surfaces are built from: the CRUD hooks, the reshape rules, the one
//! render order, and the queries the definition document and the intermediate declaration run.

use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
    Statement, TransactionTrait,
};
use uuid::Uuid;

use self::rules::{Member, Role};
use super::models::group_model::ParameterGroup;
use super::models::member_model::ParameterGroupMember;
use super::models::SlotDeclaration;
use crate::error::{AppError, AppResult};
use crate::routes::private::derived_parameters::models::{definition, source};

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
    Ok((code, replicated_codes(db, group_id).await?))
}

/// The catalog codes of a group's members entered several times at a visit. Replicate-ness is a
/// property of the parameter in its group, so this is what a manifest reads to decide whether a
/// source carries one value or the whole family (Q155).
pub async fn replicated_codes<C: ConnectionTrait>(
    db: &C,
    group_id: Uuid,
) -> Result<Vec<String>, ApiError> {
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
    Ok(replicated)
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

pub mod ordering {
    //! The one order the grid, the tool form and the Toolbox render a group in.
    //!
    //! Four things looked like grouping and only two of them are: `parameters.category` is the
    //! device-health split that gates alarms and the public arm, and the group is the scientific
    //! category. A manifest `section` is neither: it labels a run of columns inside a group, so it is
    //! a display hint that never reorders anything. The member's `ordinal` is the order, everywhere.

    use uuid::Uuid;

    use super::rules::Role;

    /// One column of a group as the three surfaces see it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Column {
        pub parameter_id: Uuid,
        /// The catalog code, which breaks a tie between two members sharing an ordinal so the order is
        /// the same in every database rather than the store's.
        pub code: String,
        pub ordinal: i32,
        pub role: Role,
        /// The manifest section this member's field renders under, when a calculation names one.
        pub section: Option<String>,
    }

    /// The group's columns in the order they are rendered: by member ordinal, then by code.
    pub fn column_order(members: &[Column]) -> Vec<&Column> {
        let mut ordered: Vec<&Column> = members.iter().collect();
        ordered.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then_with(|| a.code.cmp(&b.code)));
        ordered
    }

    /// The section labels a group renders, in the order their first column appears under
    /// [`column_order`]. A run of columns naming no section carries no label.
    pub fn section_order(members: &[Column]) -> Vec<String> {
        let mut sections: Vec<String> = Vec::new();
        for column in column_order(members) {
            if let Some(section) = &column.section
                && !sections.contains(section)
            {
                sections.push(section.clone());
            }
        }
        sections
    }
}

pub mod rules {
    //! The reshape rules for a parameter group, as pure decisions over its members and the
    //! calculations attached to it. The database holds the unique membership and the role CHECK; these
    //! are the rules SQL cannot state, and the CRUD hooks are their only callers.

    use std::fmt;

    use uuid::Uuid;

    /// What a parameter is doing inside its group.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Role {
        /// Entered at a visit and read by a calculation.
        Measured,
        /// Entered at a visit and read by nothing.
        EntryOnly,
        /// Produced by the group's calculation.
        Output,
    }

    impl Role {
        pub fn parse(value: &str) -> Option<Self> {
            match value {
                "measured" => Some(Self::Measured),
                "entry_only" => Some(Self::EntryOnly),
                "output" => Some(Self::Output),
                _ => None,
            }
        }

        pub fn as_str(self) -> &'static str {
            match self {
                Self::Measured => "measured",
                Self::EntryOnly => "entry_only",
                Self::Output => "output",
            }
        }
    }

    /// One membership row, reduced to what the rules read.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Member {
        pub group_id: Uuid,
        pub parameter_id: Uuid,
        pub role: Role,
    }

    /// A calculation attached to a group, naming the parameters it consumes and produces.
    #[derive(Clone, Debug)]
    pub struct Calculation {
        pub group_id: Uuid,
        pub name: String,
        pub inputs: Vec<Uuid>,
        pub outputs: Vec<Uuid>,
    }

    /// Why a reshape was refused. Each carries what the caller has to say back.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Refusal {
        /// The parameter already belongs to another group.
        AlreadyGrouped { group_id: Uuid },
        /// An `output` member cannot leave while a calculation in its group produces it.
        ProducedHere { calculation: String },
        /// A group with members is not deleted; its members are moved first.
        GroupHasMembers { count: usize },
        /// The parameter is the mean or sd of a replicated member of the same group.
        StatisticOfMember { member_code: String },
    }

    impl fmt::Display for Refusal {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::AlreadyGrouped { group_id } => {
                    write!(f, "parameter already belongs to group {group_id}")
                }
                Self::ProducedHere { calculation } => write!(
                    f,
                    "calculation {calculation} produces this parameter; retire it from the calculation first"
                ),
                Self::GroupHasMembers { count } => {
                    write!(f, "group still has {count} members; move them out first")
                }
                Self::StatisticOfMember { member_code } => write!(
                    f,
                    "this is the mean or sd of member {member_code}, which the samples trigger computes; it renders beside that member and is not one"
                ),
            }
        }
    }

    /// A parameter belongs to at most one group, so adding it anywhere else is refused.
    pub fn may_add(parameter_id: Uuid, members: &[Member]) -> Result<(), Refusal> {
        match members.iter().find(|m| m.parameter_id == parameter_id) {
            Some(existing) => Err(Refusal::AlreadyGrouped {
                group_id: existing.group_id,
            }),
            None => Ok(()),
        }
    }

    /// The segments a portal statistics column is named with, between the analyte and its units:
    /// `DOC_avg_ppb` and `DOC_sd_ppb` are the mean and sd of `DOC_ppb`.
    const STATISTIC_SEGMENTS: [&str; 5] = ["avg", "mean", "sd", "stdev", "std"];

    /// A replicated member's statistics are computed by the `samples` trigger and shown beside it, so
    /// the group holds no member for them. The portals stored each as a parameter of its own, and one
    /// taken in as a member renders a second mean nothing maintains and an operator can type into.
    ///
    /// `replicated` is the code of every member of the group that is entered several times.
    pub fn may_add_code(code: &str, replicated: &[&str]) -> Result<(), Refusal> {
        let segments: Vec<&str> = code.split('_').collect();
        for (index, segment) in segments.iter().enumerate() {
            if !STATISTIC_SEGMENTS.contains(&segment.to_lowercase().as_str()) {
                continue;
            }
            let mut rest = segments.clone();
            rest.remove(index);
            let stripped = rest.join("_");
            if let Some(member) = replicated
                .iter()
                .find(|m| m.eq_ignore_ascii_case(&stripped))
            {
                return Err(Refusal::StatisticOfMember {
                    member_code: (*member).to_string(),
                });
            }
        }
        Ok(())
    }

    /// A member leaves its group only when nothing in that group produces it. A move within the same
    /// group is a reorder, not a move, and is always allowed.
    pub fn may_move(
        member: Member,
        to_group: Uuid,
        calculations: &[Calculation],
    ) -> Result<(), Refusal> {
        if member.group_id == to_group {
            return Ok(());
        }
        if member.role != Role::Output {
            return Ok(());
        }
        match calculations
            .iter()
            .find(|c| c.group_id == member.group_id && c.outputs.contains(&member.parameter_id))
        {
            Some(producer) => Err(Refusal::ProducedHere {
                calculation: producer.name.clone(),
            }),
            None => Ok(()),
        }
    }

    /// A group is deleted only once it is empty; a split is a new group plus moves.
    pub fn may_delete(group_id: Uuid, members: &[Member]) -> Result<(), Refusal> {
        let count = members.iter().filter(|m| m.group_id == group_id).count();
        if count > 0 {
            return Err(Refusal::GroupHasMembers { count });
        }
        Ok(())
    }

    /// What a parameter is to the calculations, which is what its role says (Q135).
    ///
    /// The role is read off the calculations, never set by hand: a parameter one writes is an output,
    /// one a calculation reads is measured, and a parameter no calculation touches is entered and read
    /// by nothing. A calculation may read any catalog parameter, so a group is a way to list many
    /// parameters together, not a boundary a calculation is confined to.
    #[must_use]
    pub fn derive_role(parameter_id: Uuid, calculations: &[Calculation]) -> Role {
        if calculations
            .iter()
            .any(|c| c.outputs.contains(&parameter_id))
        {
            return Role::Output;
        }
        if calculations
            .iter()
            .any(|c| c.inputs.contains(&parameter_id))
        {
            return Role::Measured;
        }
        Role::EntryOnly
    }
}

/// What each of the group's parameters declares at one site: the sd estimator, which is never
/// inferred, and the decimal places a form renders at. A slot with no row, or a row declaring
/// neither, carries NULL for both.
pub(super) async fn site_declarations(
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
pub(super) async fn calculation_roles(
    db: &sea_orm::DatabaseConnection,
) -> AppResult<std::collections::HashMap<Uuid, Role>> {
    let mut roles = std::collections::HashMap::new();
    // A source naming a site property carries no parameter, so its NULL is skipped rather than
    // decoded.
    let read = source::Entity::find()
        .select_only()
        .column(source::Column::ParameterId)
        .distinct()
        .into_tuple::<Option<Uuid>>()
        .all(db)
        .await
        .map_err(AppError::Database)?;
    for id in read.into_iter().flatten() {
        roles.insert(id, Role::Measured);
    }
    // Written last: what a formula writes is what the parameter is, even where another reads it.
    let written = definition::Entity::find()
        .select_only()
        .column(definition::Column::OutputParameterId)
        .distinct()
        .filter(definition::Column::OutputParameterId.is_not_null())
        .into_tuple::<Option<Uuid>>()
        .all(db)
        .await
        .map_err(AppError::Database)?;
    for id in written.into_iter().flatten() {
        roles.insert(id, Role::Output);
    }
    Ok(roles)
}

pub(super) async fn manifest_sections(
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

#[cfg(test)]
#[path = "tests/ordering.rs"]
mod ordering_tests;

#[cfg(test)]
#[path = "tests/rules.rs"]
mod rules_tests;
