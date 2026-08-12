use crudcrate::{CRUDResource, EntityToModels};
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// Read-only CrudCrate view over the `api_token_audit_log` forensic table (written fire-and-forget on
/// every API-token request, see `api_tokens::service::record_token_use`). All fields are
/// `exclude(create, update)`, the table is append-only and the generated mutation routes are unused.
/// Mounted behind `require_admin` (no API token can read the audit trail); the UI surfaces it in the
/// System → Logs hub with filtering/sorting/pagination for free.
#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "api_token_audit_log")]
#[crudcrate(
    api_struct = "ApiTokenAuditLog",
    name_singular = "api_token_audit_log",
    name_plural = "api_token_audit_logs",
    generate_router
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub token_id: Uuid,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub method: String,
    #[crudcrate(filterable, exclude(update, create))]
    pub path: String,
    #[crudcrate(filterable, sortable, exclude(update, create))]
    pub status_code: i32,
    #[crudcrate(filterable, exclude(update, create))]
    pub project_scope: Option<Uuid>,
    #[crudcrate(sortable, exclude(update, create))]
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
