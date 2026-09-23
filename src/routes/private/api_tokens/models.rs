//! The token row and its audit row, plus the Keycloak directory shapes the user routes answer
//! with. Only the first two are tables.

use crudcrate::EntityToModels;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::service::ApiTokenOperations;

#[derive(
    Clone, Debug, PartialEq, DeriveEntityModel, serde::Serialize, serde::Deserialize, EntityToModels,
)]
#[sea_orm(table_name = "api_tokens")]
#[crudcrate(
    api_struct = "ApiToken",
    name_singular = "api_token",
    name_plural = "api_tokens",
    generate_router,
    operations = ApiTokenOperations
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    #[crudcrate(primary_key, exclude(update, create), on_create = Uuid::new_v4())]
    pub id: Uuid,
    #[crudcrate(filterable, fulltext, sortable)]
    pub name: String,
    /// Per-key allocation label, which external client/logger this key was issued to.
    #[crudcrate(filterable, fulltext)]
    pub description: Option<String>,
    /// Argon2id PHC hash of the token secret. Set on mint, and served by no response.
    #[sea_orm(unique)]
    #[crudcrate(exclude(create, update, list, one))]
    pub token_hash: String,
    /// Non-secret indexed lookup key (`rvd_<token_prefix>_<secret>`); set on mint.
    #[crudcrate(exclude(create, update), sortable)]
    pub token_prefix: String,
    #[crudcrate(filterable)]
    pub project_scope: Option<Uuid>,
    #[sea_orm(column_type = "JsonBinary")]
    pub permissions: serde_json::Value,
    #[crudcrate(filterable, on_create = true)]
    pub is_active: bool,
    /// Optional per-token request ceiling (requests/second). NULL = unlimited.
    pub rate_limit_per_second: Option<i32>,
    #[crudcrate(exclude(create, update), sortable)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(sortable)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[crudcrate(exclude(create, update), sortable)]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_by: Option<String>,
    /// The one-time secret, populated only in the create/rotate response. Never stored.
    #[sea_orm(ignore)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[crudcrate(non_db_attr = true, exclude(create, update))]
    pub token: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::routes::private::projects::Entity",
        from = "Column::ProjectScope",
        to = "crate::routes::private::projects::Column::Id"
    )]
    Project,
}

impl Related<crate::routes::private::projects::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Project.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub mod audit_log {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    /// Read-only CrudCrate view over the `api_token_audit_log` forensic table (appended off the
    /// request path by `api_tokens::service::record_token_use`). The table is append-only, so only
    /// the read routes are mounted. Mounted behind `require_admin` (no API token can read the audit
    /// trail); the UI surfaces it in the System → Logs hub with filtering/sorting/pagination.
    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "api_token_audit_log")]
    #[crudcrate(
        api_struct = "ApiTokenAuditLog",
        name_singular = "api_token_audit_log",
        name_plural = "api_token_audit_logs",
        generate_router,
        routes(read)
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
}

/// A project a Keycloak member may see and act in. The key is the pair itself: a member holds a
/// project once or not at all, and nothing addresses one grant by a URL, so no router is mounted
/// (Q153). Read on every non-admin request through `common::grants`, written whole per user by
/// `set_user_grants`.
pub mod grant {
    use crudcrate::EntityToModels;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(
        Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize, EntityToModels,
    )]
    #[sea_orm(table_name = "user_project_grants")]
    #[crudcrate(
        api_struct = "UserProjectGrant",
        name_singular = "user_project_grant",
        name_plural = "user_project_grants"
    )]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update), filterable)]
        pub user_sub: String,
        #[sea_orm(primary_key, auto_increment = false)]
        #[crudcrate(primary_key, exclude(update), filterable)]
        pub project_id: Uuid,
        #[crudcrate(exclude(update))]
        pub granted_by: Option<String>,
        #[crudcrate(exclude(create, update), sortable)]
        pub created_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "crate::routes::private::projects::Entity",
            from = "Column::ProjectId",
            to = "crate::routes::private::projects::Column::Id"
        )]
        Project,
    }

    impl Related<crate::routes::private::projects::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Project.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

// --- The Keycloak directory's wire shapes ---

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ListQuery {
    /// React-admin style range, e.g. "[0,9]"
    pub range: Option<String>,
    /// React-admin style filter, e.g. {"q":"john"}
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize, ToSchema)]
pub struct KeycloakRole {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AssignRolesRequest {
    pub roles: Vec<String>,
}

/// A realm user as this API reports one: the fields the dashboard renders, plus the realm roles
/// that decide what they may do. The names are Keycloak's own, so a caller reading the directory
/// and a caller reading this see one shape.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct KeycloakUser {
    #[schema(required)]
    pub id: Option<String>,
    #[schema(required)]
    pub username: Option<String>,
    #[schema(required)]
    pub email: Option<String>,
    #[serde(rename = "firstName")]
    #[schema(required)]
    pub first_name: Option<String>,
    #[serde(rename = "lastName")]
    #[schema(required)]
    pub last_name: Option<String>,
    #[schema(required)]
    pub enabled: Option<bool>,
    #[serde(rename = "createdTimestamp")]
    #[schema(required)]
    pub created_timestamp: Option<i64>,
    pub roles: Vec<String>,
}

/// The id of a user this request removed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DeletedUser {
    pub id: String,
}

/// A role assignment that took effect. The roles themselves are read back through the user.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RolesAssigned {
    pub success: bool,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct SearchQuery {
    /// Search string matched by Keycloak against username, email, first and last name.
    pub q: String,
}

/// Outcome of the startup realm check. `Missing` is authoritative (Keycloak answered, and the
/// roles are not there); `Unavailable` means the realm could not be asked, which is retryable.
#[derive(Debug)]
pub enum RealmRoleCheck {
    Satisfied,
    Missing(Vec<&'static str>),
    Unavailable(String),
}

/// Replace the project visibility grants for a user (`user_project_grants`). Body is the full new
/// set, this overwrites, not appends, mirroring `assign_roles`. Requires `require_admin`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetGrantsRequest {
    /// The complete set of project ids the user may see. An empty array revokes all access.
    pub project_ids: Vec<uuid::Uuid>,
}

/// Replace a user's project grants transactionally and bust their grants cache so the change takes
/// What a grant write left behind: the number of projects the user may now see.
#[derive(Debug, Serialize, ToSchema)]
pub struct SetGrantsResponse {
    pub success: bool,
    pub count: usize,
}
