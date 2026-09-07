use crudcrate::EntityToModels;
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
    // Optional on the wire, mandatory in practice: a DB trigger derives the project's default
    // subproject when a site is created/moved without one, and keeps `project_id` in sync with it.
    #[crudcrate(filterable, sortable)]
    pub subproject_id: Option<Uuid>,
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
    /// Stamped by every sync path that mints a site, and by nothing else, so it is what says a
    /// row arrived from a source rather than by hand.
    #[crudcrate(exclude(create, update), filterable, sortable)]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub public_code: Option<String>,
    /// The MeteoSwiss SMN station abbreviation (`MOB`, `SIO`, ...) supplying this site's
    /// barometric pressure. Null means the site takes no pressure series.
    #[crudcrate(filterable, sortable)]
    pub meteoswiss_station_abbr: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::Entity",
        from = "Column::ProjectId",
        to = "crate::routes::private::projects::Column::Id"
    )]
    Project,
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::subprojects::Entity",
        from = "Column::SubprojectId",
        to = "crate::routes::private::projects::subprojects::Column::Id"
    )]
    Subproject,
    #[sea_orm(has_many = "crate::routes::private::sites::parameters::Entity")]
    SiteParameters,
    #[sea_orm(has_many = "crate::routes::private::sensors::deployments::Entity")]
    SensorDeployments,
    #[sea_orm(has_many = "crate::routes::private::readings::Entity")]
    Readings,
}

impl Related<crate::routes::private::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl Related<crate::routes::private::projects::subprojects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Subproject.def()
    }
}

impl Related<crate::routes::private::sites::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameters.def()
    }
}

impl Related<crate::routes::private::sensors::deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployments.def()
    }
}

impl Related<crate::routes::private::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
