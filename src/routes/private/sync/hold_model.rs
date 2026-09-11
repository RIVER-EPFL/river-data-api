//! A review-queue hold as an entity.
//!
//! `replicate_audit_holds` is written only by the paths that raise or decide a finding: the audit
//! never writes, a raise is an upsert with its own conflict target, and each transition is a
//! guarded status change that also writes a note. So the entity mounts read only (`routes(read)`)
//! and the named routes under `/sync/replicate_audit_holds` keep the transitions.
//!
//! `kind` is [`super::models::HoldKind`] and `status` one of the values the table's CHECK allows;
//! both stay `String` on the row, so a value outside either vocabulary is readable rather than a
//! decode failure that hides the queue.

use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "replicate_audit_holds")]
#[crudcrate(
    api_struct = "ReplicateAuditHold",
    name_singular = "replicate_audit_hold",
    name_plural = "replicate_audit_holds",
    generate_router,
    routes(read)
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    /// The stream the finding is about, or NULL for one no stream produced.
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub stream_id: Option<Uuid>,
    /// The instant the finding is about.
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub group_time: DateTimeWithTimeZone,
    /// What the source said, as the raise recorded it.
    #[crudcrate(exclude(update, create))]
    pub expected: serde_json::Value,
    /// What river-data computes over what it stores.
    #[crudcrate(exclude(update, create))]
    pub computed: serde_json::Value,
    #[crudcrate(exclude(update, create))]
    pub delta: serde_json::Value,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub status: String,
    #[crudcrate(sortable, exclude(update, create))]
    pub created_at: DateTimeWithTimeZone,
    #[crudcrate(filterable, fulltext, exclude(update, create))]
    pub acknowledged_by: Option<String>,
    #[crudcrate(sortable, exclude(update, create))]
    pub acknowledged_at: Option<DateTimeWithTimeZone>,
    /// The value an operator entered in place of both, where one was.
    #[crudcrate(exclude(update, create))]
    pub manual_value: Option<f64>,
    /// What the decision was, as the transition recorded it.
    #[crudcrate(exclude(update, create))]
    pub resolution: Option<serde_json::Value>,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub kind: String,
    /// The slot half of the key, for a finding no stream produced.
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub site_id: Option<Uuid>,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub parameter_id: Option<Uuid>,
    /// The calculation a finding is about, where one produced it.
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub tool: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
