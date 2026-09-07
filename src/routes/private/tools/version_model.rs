use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

/// One immutable version of a calculation: its code, the manifest it declares and the cases it
/// must pass. Nothing updates a row here; a change is a new version, which is what makes a run's
/// pinned `version_id` mean something.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "tool_script_versions")]
#[crudcrate(
    api_struct = "ToolScriptVersion",
    name_singular = "tool_script_version",
    name_plural = "tool_script_versions",
    derive_partial_eq
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub tool_script_id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub version_no: i32,
    /// The R source. Heavy, and a version list is a history rather than a reader.
    #[crudcrate(exclude(list))]
    pub script: String,
    pub entry_function: String,
    #[sea_orm(column_type = "JsonBinary")]
    #[crudcrate(exclude(list))]
    pub manifest: serde_json::Value,
    #[sea_orm(column_type = "JsonBinary")]
    #[crudcrate(exclude(list))]
    pub test_cases: serde_json::Value,
    #[crudcrate(filterable, sortable)]
    pub content_hash: String,
    #[crudcrate(sortable)]
    pub created_by: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the stored cases last passed against the runner. A version goes live only on a pass
    /// taken at activation time, so this is a record, never a permission.
    #[crudcrate(sortable)]
    pub validated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// What changed in this version and why, as its author wrote it.
    pub note: Option<String>,
    /// Whether the script points at this version. Filled per parent, not stored.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), default = false)]
    pub active: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::script_model::Entity",
        from = "Column::ToolScriptId",
        to = "super::script_model::Column::Id"
    )]
    Script,
}

impl Related<super::script_model::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Script.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
