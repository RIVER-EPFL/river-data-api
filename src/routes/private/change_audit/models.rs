//! What was done to a thing, by whom, and when, as an entity.
//!
//! The table is append-only: every writer is a side effect of the change it records and runs in
//! that change's own transaction, so the entity mounts read only (`routes(read)`). The list route
//! answers the "what has been done here lately" question across every subject;
//! [`super::service::entries_for`] answers it for one.

use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels)]
#[sea_orm(table_name = "change_audit")]
#[crudcrate(
    api_struct = "ChangeAudit",
    name_singular = "change_audit_entry",
    name_plural = "change_audit_entries",
    generate_router,
    routes(read)
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    /// The thing the change was made to, as the writer keyed it: `schedule:{job_name}`,
    /// `parameter:{id}`, `site_parameter:{id}`, `parameter_group:{id}`, `push_subscriptions:{sub}`.
    #[crudcrate(filterable, fulltext, sortable, exclude(update, create))]
    pub subject: String,
    /// What happened, as the writer named it: `schedule_update`, `member_insert`, and so on.
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub change: String,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub changed_by: Option<String>,
    #[crudcrate(exclude(update, create))]
    pub old_value: Option<serde_json::Value>,
    #[crudcrate(exclude(update, create))]
    pub new_value: Option<serde_json::Value>,
    #[crudcrate(sortable, exclude(update, create))]
    pub changed_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// One entry as the subject-keyed reader exposes it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChangeEntry {
    pub changed_at: chrono::DateTime<chrono::Utc>,
    #[schema(required)]
    pub changed_by: Option<String>,
    /// What happened, as the writer named it: `schedule_update`, `member_insert`, and so on.
    pub change: String,
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    pub old_value: Option<serde_json::Value>,
    #[schema(value_type = Option<Object>)]
    #[schema(required)]
    pub new_value: Option<serde_json::Value>,
}

impl From<Model> for ChangeEntry {
    fn from(m: Model) -> Self {
        Self {
            changed_at: m.changed_at.into(),
            changed_by: m.changed_by,
            change: m.change,
            old_value: m.old_value,
            new_value: m.new_value,
        }
    }
}
