use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "annotations")]
#[crudcrate(
    api_struct = "Annotation",
    name_singular = "annotation",
    name_plural = "annotations",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_id: Uuid,
    #[crudcrate(sortable)]
    pub start_time: chrono::DateTime<chrono::Utc>,
    #[crudcrate(sortable)]
    pub end_time: chrono::DateTime<chrono::Utc>,
    #[crudcrate(fulltext)]
    pub text: String,
    #[crudcrate(filterable)]
    pub category: String,
    pub created_by: Option<String>,
    /// The replicate-audit hold whose resolution minted this annotation, when one did. Written
    /// only by that resolution and cleared by its reopen, so a CRUD caller cannot dress an
    /// annotation up as an audit decision. An admin may still delete the row itself.
    #[crudcrate(exclude(create, update), filterable)]
    pub audit_hold_id: Option<Uuid>,
    /// Where a source-authored annotation came from, written only by `/annotations/register` so a
    /// CRUD caller cannot claim sync provenance. NULL on hand-entered annotations.
    #[crudcrate(exclude(create, update), filterable)]
    pub source_system: Option<String>,
    #[crudcrate(exclude(create, update), filterable)]
    pub source_key: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "crate::routes::private::parameters::Entity",
        from = "Column::ParameterId",
        to = "crate::routes::private::parameters::Column::Id"
    )]
    Parameter,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<crate::routes::private::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Parameter.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
