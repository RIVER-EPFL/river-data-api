use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels)]
#[sea_orm(table_name = "parameters")]
#[crudcrate(
    api_struct = "Parameter",
    name_singular = "parameter",
    name_plural = "parameters",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable)]
    pub site_id: Uuid,
    #[crudcrate(filterable)]
    pub parameter_type_id: Uuid,
    #[crudcrate(filterable, fulltext)]
    pub name: String,
    #[crudcrate(filterable)]
    pub sensor_type: String,
    pub display_units: Option<String>,
    pub units_name: Option<String>,
    pub units_min: Option<f64>,
    pub units_max: Option<f64>,
    pub decimal_places: Option<i16>,
    pub channel_id: Option<i32>,
    pub sample_interval_sec: Option<i32>,
    #[crudcrate(filterable)]
    pub is_active: Option<bool>,
    #[crudcrate(filterable)]
    pub is_derived: Option<bool>,
    #[crudcrate(filterable)]
    pub derived_definition_id: Option<Uuid>,
    #[sea_orm(column_type = "JsonBinary", nullable)]
    pub variable_mappings: Option<serde_json::Value>,
    #[crudcrate(exclude(create, update))]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update))]
    pub discovered_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::sites::Entity",
        from = "Column::SiteId",
        to = "super::sites::Column::Id"
    )]
    Site,
    #[sea_orm(
        belongs_to = "super::parameter_types::Entity",
        from = "Column::ParameterTypeId",
        to = "super::parameter_types::Column::Id"
    )]
    ParameterType,
    #[sea_orm(
        belongs_to = "super::derived_parameter_definitions::Entity",
        from = "Column::DerivedDefinitionId",
        to = "super::derived_parameter_definitions::Column::Id"
    )]
    DerivedParameterDefinition,
    #[sea_orm(has_many = "super::readings::Entity")]
    Readings,
    #[sea_orm(has_many = "super::device_status::Entity")]
    DeviceStatus,
    #[sea_orm(has_many = "super::sensor_deployments::Entity")]
    SensorDeployments,
    #[sea_orm(has_one = "super::sync_state::Entity")]
    SyncState,
    #[sea_orm(has_one = "super::alarm_thresholds::Entity")]
    AlarmThresholds,
}

impl Related<super::sites::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Site.def()
    }
}

impl Related<super::parameter_types::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::ParameterType.def()
    }
}

impl Related<super::derived_parameter_definitions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DerivedParameterDefinition.def()
    }
}

impl Related<super::readings::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Readings.def()
    }
}

impl Related<super::device_status::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::DeviceStatus.def()
    }
}

impl Related<super::sensor_deployments::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SensorDeployments.def()
    }
}

impl Related<super::sync_state::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::SyncState.def()
    }
}

impl Related<super::alarm_thresholds::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::AlarmThresholds.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
