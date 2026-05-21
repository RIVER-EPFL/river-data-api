use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "sites")]
#[crudcrate(
    api_struct = "Site",
    name_singular = "site",
    name_plural = "sites",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub project_id: Option<Uuid>,
    #[sea_orm(unique)]
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    #[crudcrate(sortable)]
    pub latitude: Option<f64>,
    #[crudcrate(sortable)]
    pub longitude: Option<f64>,
    #[crudcrate(sortable)]
    pub altitude_m: Option<f64>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub public_slug: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::entity::projects::Entity",
        from = "Column::ProjectId",
        to = "crate::entity::projects::Column::Id"
    )]
    Project,
    #[sea_orm(has_many = "crate::entity::site_parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "crate::entity::sensor_deployments::Entity")]
    SensorDeployments,
    #[sea_orm(has_many = "crate::entity::readings::Entity")]
    Readings,
}

impl Related<crate::entity::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl Related<crate::entity::site_parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<crate::entity::sensor_deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployments.def()
    }
}

impl Related<crate::entity::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
