//! A frozen formula, as an entity.
//!
//! An edit to a standalone derived definition is a new calculation rather than a correction of the
//! old one (Q89), so each text is kept under its own version number and a reading names the version
//! it was made with. Rows are append-only and no route lists them: a version is reached through its
//! definition, so the entity carries no router.

use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "derived_parameter_definition_versions")]
#[crudcrate(
    api_struct = "DerivedDefinitionVersion",
    name_singular = "derived_definition_version",
    name_plural = "derived_definition_versions"
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub definition_id: Uuid,
    /// One-based and per definition, so `(definition_id, version_no)` is unique.
    #[crudcrate(filterable, sortable)]
    pub version_no: i32,
    pub formula: String,
    /// The hash the migration computes for the same text, so a re-save of an unchanged formula
    /// mints no version.
    #[crudcrate(filterable)]
    pub content_hash: String,
    pub created_by: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::definition_model::Entity",
        from = "Column::DefinitionId",
        to = "super::definition_model::Column::Id"
    )]
    Definition,
}

impl Related<super::definition_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Definition.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
