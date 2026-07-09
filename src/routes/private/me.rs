//! `GET /api/me` — the UI's single source of truth for the caller's identity, access level, and
//! project visibility. Keycloak-only (an API token has no user identity or grant set). The response
//! drives the whole role-aware shell: `role`/`is_admin` gate the menu, `grants` names the projects
//! the portal may show. Administrators are unrestricted, so their `grants` lists every project.

use axum::{Extension, Json, extract::State};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde::Serialize;
use uuid::Uuid;

use crate::common::AppState;
use crate::common::authz::Role;
use crate::common::middleware::AuthContext;
use crate::error::{AppError, AppResult};

#[derive(Serialize)]
pub struct GrantedProject {
    pub project_id: Uuid,
    pub name: String,
}

#[derive(Serialize)]
pub struct Me {
    pub sub: String,
    pub email: Option<String>,
    pub is_admin: bool,
    /// The caller's highest access level as a bare token (`administrator`|`manager`|`river`|`intern`).
    pub role: String,
    /// Projects the caller may see. For administrators this is every project (they are unrestricted);
    /// for everyone else it is exactly their grant set.
    pub grants: Vec<GrantedProject>,
}

/// The highest-privilege role held, by level (ignores `Unknown`). The access gate guarantees at
/// least one river role, so the fallback is only defensive.
fn highest_role(roles: &[Role]) -> Role {
    roles
        .iter()
        .max_by_key(|r| r.level())
        .cloned()
        .unwrap_or(Role::Unknown(String::new()))
}

fn role_token(role: &Role) -> &'static str {
    match role {
        Role::Administrator => "administrator",
        Role::Manager => "manager",
        Role::River => "river",
        Role::Intern => "intern",
        Role::Unknown(_) => "none",
    }
}

pub async fn get_me(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Me>> {
    let AuthContext::Keycloak { roles, sub, email, grants, .. } = &auth else {
        return Err(AppError::Forbidden("me_requires_keycloak".to_string()));
    };
    let highest = highest_role(roles);
    let is_admin = matches!(highest, Role::Administrator);

    // Administrators see every project; other members see exactly their grant set. Both go through a
    // name lookup so the UI never has to stitch project names client-side.
    let granted = if is_admin {
        named_projects(&state, None).await?
    } else {
        let ids: Vec<Uuid> = grants.iter().copied().collect();
        named_projects(&state, Some(&ids)).await?
    };

    Ok(Json(Me {
        sub: sub.clone(),
        email: email.clone(),
        is_admin,
        role: role_token(&highest).to_string(),
        grants: granted,
    }))
}

/// Resolve `(id, name)` for a set of project ids, or every project when `ids` is `None` (admin).
/// An empty `ids` slice returns no rows (a member with no grants).
async fn named_projects(state: &AppState, ids: Option<&[Uuid]>) -> AppResult<Vec<GrantedProject>> {
    let stmt = match ids {
        None => Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id, name FROM projects ORDER BY name",
            [],
        ),
        Some([]) => return Ok(Vec::new()),
        Some(ids) => Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id, name FROM projects WHERE id = ANY($1) ORDER BY name",
            [sea_orm::Value::Array(
                sea_orm::sea_query::ArrayType::Uuid,
                Some(Box::new(ids.iter().map(|id| (*id).into()).collect())),
            )],
        ),
    };
    let rows = state
        .db
        .query_all(stmt)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(GrantedProject {
                project_id: r.try_get::<Uuid>("", "id").ok()?,
                name: r.try_get::<String>("", "name").ok()?,
            })
        })
        .collect())
}
