use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// A non-numeric time-series row: a device status string at an instant on a stream. The key is
/// the pair, so no route addresses one row by its id (Q153); rows arrive in batches through the
/// ingest and batch routes and are read per site.
#[derive(
    Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "status_events")]
#[crudcrate(
    api_struct = "StatusEvent",
    name_singular = "status_event",
    name_plural = "status_events",
    derive_partial_eq
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update), filterable)]
    pub stream_id: Uuid,
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update), filterable, sortable)]
    pub time: DateTimeWithTimeZone,
    #[crudcrate(filterable)]
    pub site_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub parameter_id: Option<Uuid>,
    #[crudcrate(filterable)]
    pub value: String,
    #[crudcrate(filterable)]
    pub sensor_id: Option<Uuid>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::data_streams::Entity",
        from = "Column::StreamId",
        to = "crate::routes::private::data_streams::Column::Id"
    )]
    DataStream,
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
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
}

impl Related<crate::routes::private::data_streams::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DataStream.def()
    }
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

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
