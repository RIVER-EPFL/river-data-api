use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

use super::operations::SensorDeploymentOperations;

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    DeriveEntityModel,
    serde::Serialize,
    serde::Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "sensor_deployments")]
#[crudcrate(
    api_struct = "SensorDeployment",
    name_singular = "sensor_deployment",
    name_plural = "sensor_deployments",
    generate_router,
    derive_partial_eq,
    derive_eq,
    operations = SensorDeploymentOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub sensor_id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Uuid,
    /// Denormalized from the sensor's (immutable) parameter by the `set_deployment_parameter_id`
    /// trigger. Read-only over the API (`exclude(create, update)`); serialized so the UI can resolve
    /// the slot incumbent for adopt/swap.
    #[crudcrate(filterable, exclude(create, update))]
    pub parameter_id: Uuid,
    #[crudcrate(sortable)]
    pub deployed_from: chrono::DateTime<chrono::Utc>,
    #[crudcrate(filterable, sortable)]
    pub deployed_until: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(filterable, on_create = String::from("permanent"))]
    pub deployment_type: String,
    pub notes: Option<String>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::Entity",
        from = "Column::SiteId",
        to = "crate::routes::private::sites::Column::Id"
    )]
    Site,
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
