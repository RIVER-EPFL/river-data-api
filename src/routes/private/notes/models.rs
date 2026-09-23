use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::service::NoteOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "notes")]
#[crudcrate(
    api_struct = "Note",
    name_singular = "note",
    name_plural = "notes",
    generate_router,
    operations = NoteOperations,
    upsert_key(source_system, source_key)
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Uuid,
    #[sea_orm(column_type = "Text")]
    pub text: String,
    #[crudcrate(on_create = false)]
    pub verified: bool,
    /// The caller who created the row, stamped from the request; an update naming it is refused.
    #[crudcrate(exclude(create), on_create = crate::common::actor::current().unwrap_or_default())]
    pub created_by: Option<String>,
    /// Where a source-authored note came from, written only by `/notes/register` so a CRUD caller
    /// cannot claim sync provenance. NULL on hand-entered notes.
    #[crudcrate(exclude(create, update), filterable)]
    pub source_system: Option<String>,
    #[crudcrate(exclude(create, update), filterable)]
    pub source_key: Option<String>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(exclude(create, update), on_update = chrono::Utc::now())]
    pub updated_at: chrono::DateTime<chrono::Utc>,
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

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterNotesRequest {
    /// The sync source the notes come from, e.g. "metalp".
    pub source_system: String,
    pub notes: Vec<NoteItem>,
}

/// One source-authored site note. The note's fields are `river_data_core::models::NoteUpsert`,
/// which the sync services build from; `verified` has always been optional on this route and core
/// declares it required, so an omitted flag is filled in before the body is read.
#[derive(Debug, utoipa::ToSchema)]
#[schema(value_type = river_data_core::models::NoteUpsert)]
pub struct NoteItem(pub river_data_core::models::NoteUpsert);

impl<'de> serde::Deserialize<'de> for NoteItem {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::routes::private::wire::defaulted(
            deserializer,
            &[("verified", serde_json::json!(false))],
        )
        .map(Self)
    }
}

impl std::ops::Deref for NoteItem {
    type Target = river_data_core::models::NoteUpsert;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct RegisterNotesResponse {
    pub notes: Vec<NoteOutcome>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct NoteOutcome {
    pub source_key: String,
    /// None when the note was not stored (`unresolved`).
    #[schema(required)]
    pub id: Option<Uuid>,
    /// created | updated | unchanged | unresolved
    pub status: String,
}
