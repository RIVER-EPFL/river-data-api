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

/// Every finding standing against one calculation slot: raised by the audit or the chain, so it
/// carries no stream and is keyed on the site, the parameter and the instant instead.
///
/// The probe and the two supersedes in the chain executor share this predicate, so what "the same
/// slot" means is stated once. A status or a kind is bound as a value through the entity's own
/// columns rather than pasted into the statement, which is what makes a renamed variant a compile
/// error instead of a supersede that matches no row.
#[must_use]
pub fn slot(
    site_id: Uuid,
    parameter_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> sea_orm::Condition {
    sea_orm::Condition::all()
        .add(Column::StreamId.is_null())
        .add(Column::SiteId.eq(site_id))
        .add(Column::ParameterId.eq(parameter_id))
        .add(Column::GroupTime.eq(DateTimeWithTimeZone::from(at)))
}

/// Every finding standing against one slot whatever the instant it names.
///
/// A continuous derived refusal names the first instant the formula stopped computing (Q172), so
/// what closes it is the slot computing again anywhere in the run, not at that one instant.
#[must_use]
pub fn slot_at_any_instant(site_id: Uuid, parameter_id: Uuid) -> sea_orm::Condition {
    sea_orm::Condition::all()
        .add(Column::StreamId.is_null())
        .add(Column::SiteId.eq(site_id))
        .add(Column::ParameterId.eq(parameter_id))
}

/// Narrow a slot predicate to the findings in one status.
#[must_use]
pub fn in_status(
    condition: sea_orm::Condition,
    status: super::models::HoldStatus,
) -> sea_orm::Condition {
    condition.add(Column::Status.eq(status.as_str()))
}

/// Narrow a slot predicate to a set of kinds.
#[must_use]
pub fn of_kinds(
    condition: sea_orm::Condition,
    kinds: &[super::models::HoldKind],
) -> sea_orm::Condition {
    let names: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
    condition.add(Column::Kind.is_in(names))
}

#[cfg(test)]
#[path = "tests/hold_slot.rs"]
mod tests;
