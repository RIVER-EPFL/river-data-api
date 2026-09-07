use sea_orm::entity::prelude::*;

/// One flip of a calculation's live version, under the caller who made it. Append-only: a
/// rollback is another row, never an edit of the one it undoes.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "tool_script_activations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tool_script_id: Uuid,
    pub from_version_id: Option<Uuid>,
    pub to_version_id: Uuid,
    pub activated_by: Option<String>,
    pub activated_at: chrono::DateTime<chrono::Utc>,
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
