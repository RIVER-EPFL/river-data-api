use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "collection_events")]
#[crudcrate(
    api_struct = "CollectionEvent",
    name_singular = "collection_event",
    name_plural = "collection_events",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub site_id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub collected_at: chrono::DateTime<chrono::Utc>,
    /// How the event came to exist. CRUD creation is always a person staging a visit; the sync
    /// attach path writes `portal_sync` rows directly.
    #[crudcrate(filterable, exclude(create, update), on_create = "manual".to_string())]
    pub source: String,
    pub created_by: Option<String>,
    pub notes: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
