//! A schedule row as an entity.
//!
//! `schedules` is a projection of the job registry: the scheduler inserts one row per registered
//! job at boot and nothing else creates one, so the entity mounts read and update only
//! (`routes(read, update)`). A create would make a schedule for a name the registry does not know,
//! which would never fire, and a delete would be back at the next boot.
//!
//! `job_name` is the key the API is addressed by, so it is the declared primary key even though the
//! table's own is `id`; the column is `UNIQUE`, so the two agree on every row.

use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

use super::schedule_operations::ScheduleOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "schedules")]
#[crudcrate(
    api_struct = "Schedule",
    name_singular = "schedule",
    name_plural = "schedules",
    generate_router,
    routes(read, update),
    operations = ScheduleOperations
)]
pub struct Model {
    /// The registered job this schedule drives, and the id every route addresses it by.
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, filterable, sortable, exclude(update))]
    pub job_name: String,
    #[crudcrate(exclude(update))]
    pub id: Uuid,
    #[crudcrate(filterable, sortable)]
    pub enabled: bool,
    /// The next slot the scheduler will fire, maintained by the scheduler and by an edit that
    /// brings the grid forward.
    #[crudcrate(sortable, exclude(update))]
    pub next_run_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(sortable)]
    pub interval_seconds: Option<i64>,
    pub overlap_policy: Option<String>,
    pub catchup_policy: Option<String>,
    pub tunables: serde_json::Value,
    #[crudcrate(sortable, exclude(update))]
    pub last_enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Who made the last edit, from the request that made it.
    #[crudcrate(exclude(update), on_update = crate::common::actor::current().unwrap_or_default())]
    pub updated_by: Option<String>,
    #[crudcrate(sortable, exclude(update), on_update = chrono::Utc::now())]
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[crudcrate(sortable, exclude(update))]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Whether a job of this name is in flight. Per request, never stored.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), default = false)]
    pub running: bool,
    /// What this job accepts under `tunables`, from its own declaration, as the form reads it.
    /// `[]` means none. It is the job's own declaration, not a stored column.
    #[sea_orm(ignore)]
    #[crudcrate(non_db_attr = true, exclude(create, update), default = Vec::new())]
    pub tunables_schema: Vec<super::job::TunableSpec>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
