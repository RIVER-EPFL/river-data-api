//! The three tables the job machinery owns: a tracked job row, its timeline, and the cadence row
//! that enqueues one.
//!
//! One file, three modules, because each is a SeaORM entity and an entity owns the names `Model`,
//! `Entity` and `Column`.
//!
//! `QueuedJobResponse` is the shape every route that enqueues a job answers with, wherever that
//! route lives.

use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

/// What a route that enqueues one job answers with: the row it enqueued, and the state that row is
/// in when the response is written.
#[derive(Debug, Serialize, ToSchema)]
pub struct QueuedJobResponse {
    /// The row enqueued, or null where an identical job was already queued under the same dedupe
    /// key and this request added none.
    #[schema(required)]
    pub job_id: Option<Uuid>,
    /// `queued`, always.
    pub status: String,
}

impl QueuedJobResponse {
    #[must_use]
    pub fn queued(job_id: Option<Uuid>) -> Self {
        Self {
            job_id,
            status: "queued".to_string(),
        }
    }
}

pub mod job {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    use super::super::service::ReprocessingJobOperations;

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "reprocessing_jobs")]
    #[crudcrate(
        api_struct = "ReprocessingJob",
        name_singular = "reprocessing_job",
        name_plural = "reprocessing_jobs",
        generate_router,
        routes(read),
        operations = ReprocessingJobOperations
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
        /// What the run was asked to do, the object `worker::enqueue` stored and a rerun replays.
        #[crudcrate(exclude(update, create))]
        pub params: serde_json::Value,
        #[crudcrate(sortable, exclude(update, create))]
        pub created_at: DateTimeWithTimeZone,
        #[crudcrate(sortable, exclude(update, create))]
        pub completed_at: Option<DateTimeWithTimeZone>,
        /// The worker replica holding the lease, while one does.
        #[crudcrate(exclude(update, create))]
        pub owner: Option<String>,
        /// When the current lease lapses, after which a reaper arm may claim the row again.
        #[crudcrate(sortable, exclude(update, create))]
        pub lease_expires_at: Option<DateTimeWithTimeZone>,
        /// Bumped on every claim, so a lease granted before a takeover cannot write after it.
        #[crudcrate(exclude(update, create))]
        pub lease_epoch: i64,
        /// Set by `POST /reprocessing_jobs/{id}/cancel`; the run stops at its next checkpoint.
        #[crudcrate(filterable, exclude(update, create))]
        pub cancel_requested: bool,
        /// When the row becomes claimable, which a retry pushes out by the backoff.
        #[crudcrate(sortable, exclude(update, create))]
        pub next_attempt_at: DateTimeWithTimeZone,
        /// Coalescing key while a job waits: an enqueue naming one that is already queued creates
        /// nothing. The claim clears it, so the next enqueue queues a fresh run.
        #[crudcrate(filterable, exclude(update, create))]
        pub dedupe_key: Option<String>,
        /// Whether `POST /reprocessing_jobs/{id}/rerun` accepts this row, from the registry's policy.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(update, create))]
        pub rerunnable: bool,
        /// Whether `POST /reprocessing_jobs/{id}/cancel` accepts this row, from the registry's policy.
        #[sea_orm(ignore)]
        #[crudcrate(non_db_attr = true, exclude(update, create))]
        pub cancellable: bool,
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
}

/// A schedule row as an entity.
///
/// `schedules` is a projection of the job registry: the scheduler inserts one row per registered
/// job at boot and nothing else creates one, so the entity mounts read and update only
/// (`routes(read, update)`). A create would make a schedule for a name the registry does not know,
/// which would never fire, and a delete would be back at the next boot.
///
/// `job_name` is the key the API is addressed by, so it is the declared primary key even though the
/// table's own is `id`; the column is `UNIQUE`, so the two agree on every row.
pub mod schedule {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    use super::super::service::ScheduleOperations;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
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
        pub tunables_schema: Vec<super::super::service::TunableSpec>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// A timeline entry as an entity.
///
/// `reprocessing_job_logs` is append-only and written only by a running job, so reads are the whole
/// surface (`routes(read)`). Its key is `(job_id, seq)`: the job the line belongs to, and its
/// position in that job's ordered timeline.
pub mod job_log {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;

    #[derive(
        Clone,
        Debug,
        PartialEq,
        DeriveEntityModel,
        serde::Serialize,
        serde::Deserialize,
        EntityToModels,
    )]
    #[sea_orm(table_name = "reprocessing_job_logs")]
    #[crudcrate(
        api_struct = "ReprocessingJobLog",
        name_singular = "reprocessing_job_log",
        name_plural = "reprocessing_job_logs",
        generate_router,
        routes(read)
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, filterable, sortable, exclude(create, update))]
        pub job_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, filterable, sortable, exclude(create, update))]
        pub seq: i64,
        #[crudcrate(filterable, sortable, exclude(create, update))]
        pub ts: DateTimeWithTimeZone,
        #[crudcrate(filterable, sortable, exclude(create, update))]
        pub level: String,
        #[crudcrate(exclude(create, update))]
        pub message: String,
        #[crudcrate(exclude(create, update))]
        pub context: serde_json::Value,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::job::Entity",
            from = "Column::JobId",
            to = "super::job::Column::Id"
        )]
        Job,
    }

    impl Related<super::job::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Job.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}
