//! Parameter groups: the portal's categories, their members and the document a form renders from.
//!
//! **Presentation precedence.** Every field a form shows resolves member over catalog, one
//! `COALESCE` each (`service.rs`):
//!
//! - `label`: the member's, else `parameters.name`
//! - `units`: the member's, else `parameters.default_units` (empty string reads as absent)
//! - `description`: the member's, else `parameters.description`
//!
//! `decimal_places` is the exception and resolves nothing from the member, because a group does not
//! declare one (Q120): it is `site_parameters.decimal_places` when the caller names a site, else the
//! platform default of 2. Rounding is presentation and full resolution is what is stored, so a slot
//! that wants more or fewer places declares them, and the number a form shows is the number the
//! public API rounds with. `site_parameters` also supplies `sd_estimator`, and only when the caller
//! names a site: the divisor is declared per slot and is never inferred.

use sea_orm::FromQueryResult;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The group itself. Its own module because a SeaORM entity owns the names `Entity`, `Model` and
/// `Column`, and this component has two.
pub mod group_model {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    use crate::routes::private::parameter_groups::service::ParameterGroupOperations;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "parameter_groups")]
    #[crudcrate(
        api_struct = "ParameterGroup",
        name_singular = "parameter_group",
        name_plural = "parameter_groups",
        generate_router,
        derive_partial_eq,
        operations = ParameterGroupOperations
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[sea_orm(unique)]
        #[crudcrate(filterable, fulltext, sortable)]
        pub code: String,
        #[crudcrate(fulltext, sortable)]
        pub label: String,
        #[crudcrate(sortable)]
        pub description: Option<String>,
        /// Where the group sits in the portal's category order.
        #[crudcrate(filterable, sortable, on_create = 0)]
        pub ordinal: i32,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(has_many = "super::member_model::Entity")]
        Members,
    }

    impl Related<super::member_model::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Members.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

/// One parameter's membership of a group.
pub mod member_model {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    use crate::routes::private::parameter_groups::service::ParameterGroupMemberOperations;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "parameter_group_members")]
    #[crudcrate(
        api_struct = "ParameterGroupMember",
        name_singular = "parameter_group_member",
        name_plural = "parameter_group_members",
        generate_router,
        derive_partial_eq,
        operations = ParameterGroupMemberOperations
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
        pub id: Uuid,
        #[crudcrate(filterable, sortable)]
        pub group_id: Uuid,
        #[crudcrate(filterable)]
        pub parameter_id: Uuid,
        #[crudcrate(filterable, sortable, on_create = 0)]
        pub ordinal: i32,
        /// `measured`, `entry_only` or `output`, held by a DB CHECK and by
        /// [`super::rules::Role`].
        #[crudcrate(filterable, sortable)]
        pub role: String,
        /// The replicate spec for a member entered several times at one visit.
        pub replicates: Option<serde_json::Value>,
        /// What the source computed an `output` member with: `{ function, inputs }`, the portal's
        /// own calculation and the columns it reads. NULL where nothing computed the column, or
        /// where the source declares no calculation.
        pub source_calculation: Option<serde_json::Value>,
        /// Per-group presentation overrides. NULL means the catalog parameter's own. Decimal places
        /// are not among them: they are declared per slot, never per group (Q120).
        pub label: Option<String>,
        pub units: Option<String>,
        pub description: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: chrono::DateTime<chrono::Utc>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::group_model::Entity",
            from = "Column::GroupId",
            to = "super::group_model::Column::Id"
        )]
        Group,
        #[sea_orm(
            belongs_to = "crate::routes::private::parameters::Entity",
            from = "Column::ParameterId",
            to = "crate::routes::private::parameters::Column::Id"
        )]
        Parameter,
    }

    impl Related<super::group_model::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Group.def()
        }
    }

    impl Related<crate::routes::private::parameters::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Parameter.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

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
pub fn member_statistics(
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
pub struct MemberRow {
    pub parameter_id: Uuid,
    pub code: String,
    pub label: String,
    pub units: Option<String>,
    pub description: Option<String>,
    pub ordinal: i32,
    pub replicates: Option<serde_json::Value>,
}

/// What one slot declares for a member: the divisor and the places, both nullable.
#[derive(FromQueryResult)]
pub struct SlotDeclaration {
    pub parameter_id: Uuid,
    pub sd_estimator: Option<String>,
    pub decimal_places: Option<i16>,
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

