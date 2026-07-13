use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    DeriveEntityModel,
    Serialize,
    Deserialize,
    EntityToModels,
)]
#[sea_orm(table_name = "data_streams")]
#[crudcrate(
    api_struct = "DataStream",
    name_singular = "data_stream",
    name_plural = "data_streams",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub source_system: String,
    #[crudcrate(filterable, fulltext, sortable)]
    pub source_key: String,
    #[crudcrate(fulltext, sortable)]
    pub source_name: Option<String>,
    #[crudcrate(fulltext)]
    pub source_path: Option<String>,
    #[sea_orm(column_type = "JsonBinary")]
    pub metadata: serde_json::Value,
    #[crudcrate(filterable)]
    pub site_parameter_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub sensor_id: Option<Uuid>,
    /// Stream-level default for readings.measurement_type ('continuous' | 'spot' | 'derived').
    /// NULL defers to the owning sensor's data_frequency, then falls back to 'continuous'.
    #[crudcrate(filterable)]
    pub measurement_type: Option<String>,
    #[crudcrate(filterable, on_create = true)]
    pub is_active: bool,
    #[crudcrate(sortable, exclude(create, update))]
    pub discovered_at: DateTimeWithTimeZone,
    #[crudcrate(sortable)]
    pub paired_at: Option<DateTimeWithTimeZone>,
    #[crudcrate(sortable)]
    pub last_data_time: Option<DateTimeWithTimeZone>,
    #[crudcrate(filterable, exclude(create, update))]
    pub pairing_plan_id: Option<Uuid>,
    #[crudcrate(exclude(create, update))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(exclude(create, update))]
    pub updated_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sites::parameters::Entity",
        from = "Column::SiteParameterId",
        to = "crate::routes::private::sites::parameters::Column::Id"
    )]
    SiteParameter,
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
    #[sea_orm(
        belongs_to = "crate::routes::private::data_streams::pairing_plans::Entity",
        from = "Column::PairingPlanId",
        to = "crate::routes::private::data_streams::pairing_plans::Column::Id"
    )]
    PairingPlan,
}

impl Related<crate::routes::private::sites::parameters::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SiteParameter.def()
    }
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl Related<crate::routes::private::data_streams::pairing_plans::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PairingPlan.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
