use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::operations::ParameterGroupMemberOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
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
    /// Per-group presentation overrides. NULL means the catalog parameter's own.
    pub label: Option<String>,
    pub units: Option<String>,
    pub decimal_places: Option<i32>,
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
