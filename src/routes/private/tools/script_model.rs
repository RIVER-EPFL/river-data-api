use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::script_operations::ToolScriptOperations;

/// A calculation: its identity, which version is live, and whether it is in the calculation set.
/// The code lives in `tool_script_versions`; nothing here is versioned.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "tool_scripts")]
#[crudcrate(
    api_struct = "ToolScript",
    name_singular = "tool_script",
    name_plural = "tool_scripts",
    derive_partial_eq,
    operations = ToolScriptOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    /// The stable machine name `/tools/{name}/calculate` is reached by. Lower-cased and refused
    /// unless `[a-z0-9_]` (`ToolScriptOperations`).
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable, exclude(update))]
    pub name: String,
    #[crudcrate(fulltext, sortable)]
    pub label: String,
    #[crudcrate(fulltext)]
    pub description: Option<String>,
    /// The version `/tools` executes. Moved by activation, which also writes the audit row, so it
    /// is not editable here.
    #[crudcrate(exclude(create, update), filterable)]
    pub active_version_id: Option<Uuid>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_by: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(exclude(create, update), sortable)]
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Whether the tool is part of the calculation set: fired at visits by the chain, audited,
    /// and listed on the Tools page. Off, it can still be run by name.
    #[crudcrate(filterable, sortable, on_create = true)]
    pub enabled: bool,
    /// `script` (R in the sandbox) or `formula` (the definitions attached to the calculation).
    #[crudcrate(filterable, sortable, on_create = "script".to_string())]
    pub engine: String,
    /// The parameter group this calculation reads and writes. One calculation per group (Q43).
    #[crudcrate(filterable)]
    pub parameter_group_id: Option<Uuid>,
    /// The version history, newest first, without the code: a history is read to choose a
    /// version, and the content is fetched for the one that was chosen. Detail only, since a list
    /// of calculations is not a list of their versions.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update, list), default = vec![])]
    pub versions: Vec<super::version_model::ToolScriptVersionList>,
    /// `version_no` of `active_version_id`, so a reader does not have to fetch the version to say
    /// which one is live.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), default = None)]
    pub active_version_no: Option<i32>,
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), default = 0)]
    pub version_count: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::version_model::Entity")]
    Versions,
}

impl Related<super::version_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Versions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
