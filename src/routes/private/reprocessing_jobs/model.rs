use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels)]
#[sea_orm(table_name = "reprocessing_jobs")]
#[crudcrate(
    api_struct = "ReprocessingJob",
    name_singular = "reprocessing_job",
    name_plural = "reprocessing_jobs",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, exclude(update, create))]
    pub sensor_id: Option<Uuid>,
    #[crudcrate(filterable, exclude(update, create))]
    pub trigger_type: String,
    #[crudcrate(filterable, exclude(update, create))]
    pub trigger_id: Option<Uuid>,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub status: String,
    #[crudcrate(exclude(update, create))]
    pub readings_updated: Option<i32>,
    #[crudcrate(exclude(update, create))]
    pub progress: Option<i32>,
    #[crudcrate(exclude(update, create))]
    pub total: Option<i32>,
    #[crudcrate(exclude(update, create))]
    pub error_message: Option<String>,
    #[crudcrate(exclude(update, create))]
    pub retry_count: i32,
    /// Classification driving UI grouping/filtering: operator | metadata | maintenance.
    #[crudcrate(filterable, exclude(update, create))]
    pub category: String,
    /// Scope promoted from `detail` so the jobs list can filter by site.
    #[crudcrate(filterable, exclude(update, create))]
    pub site_id: Option<Uuid>,
    /// Originating job for a cascade (e.g. a derived recompute spawned by a reprocess).
    #[crudcrate(filterable, exclude(update, create))]
    pub parent_job_id: Option<Uuid>,
    /// Structured per-job summary + provenance (scope, time range, counts, source, samples).
    #[crudcrate(exclude(update, create))]
    pub detail: serde_json::Value,
    #[crudcrate(sortable, exclude(update, create))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(sortable, exclude(update, create))]
    pub completed_at: Option<DateTimeWithTimeZone>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::sensors::Entity",
        from = "Column::SensorId",
        to = "crate::routes::private::sensors::Column::Id"
    )]
    Sensor,
}

impl Related<crate::routes::private::sensors::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sensor.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
