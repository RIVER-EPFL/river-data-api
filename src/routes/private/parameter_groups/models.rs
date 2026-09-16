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
//! public API rounds with.

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
        /// The replicate spec for a member entered several times at one visit.
        pub replicates: Option<serde_json::Value>,
        /// What the source computed this member with: `{ function, inputs }`, the portal's own
        /// calculation and the columns it reads. NULL where nothing computed the column, or where
        /// the source declares no calculation.
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
) -> Option<MemberStatistics> {
    replicates?;
    Some(MemberStatistics {
        mean_label: format!("{code} mean"),
        sd_label: format!("{code} sd"),
        decimal_places,
    })
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct DefinitionQuery {
    /// The site the group is rendered for; it is what declares the decimal places.
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
    pub source_calculation: Option<serde_json::Value>,
}

/// What one slot declares for a member: the places, nullable.
#[derive(FromQueryResult)]
pub struct SlotDeclaration {
    pub parameter_id: Uuid,
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
    /// What the source computed this column with: `{ function, inputs }`, the portal's own
    /// calculation name and the columns it reads. It is the reference an author writes the formula
    /// against (Q149). Absent where nothing computed the column.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<std::collections::HashMap<String, serde_json::Value>>)]
    pub source_calculation: Option<serde_json::Value>,
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
    /// The section labels, in the order their first column appears. Empty where no calculation
    /// naming one of these columns declares a section.
    pub sections: Vec<String>,
    /// The calculations that read or publish one of the members, by name. A calculation belongs to
    /// no group (Q169), so the tie between the two is the parameters they share.
    pub calculations: Vec<GroupCalculation>,
}

/// One calculation of a group's columns, as the group page lists it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GroupCalculation {
    pub id: Uuid,
    pub name: String,
    pub label: String,
}
