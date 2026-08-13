//! `GET /api/me` returns the UI's single source of truth for the caller's identity, access level, and
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
    let AuthContext::Keycloak {
        roles,
        sub,
        email,
        grants,
        ..
    } = &auth
    else {
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

#[derive(Serialize)]
pub struct NavigatorSite {
    pub id: Uuid,
    pub name: String,
}

#[derive(Serialize)]
pub struct NavigatorSubproject {
    /// `None` for sites without a subproject (defensive, the sites trigger normally assigns the
    /// project's default subproject).
    pub id: Option<Uuid>,
    pub name: String,
    pub sites: Vec<NavigatorSite>,
}

#[derive(Serialize)]
pub struct NavigatorProject {
    pub project_id: Uuid,
    pub name: String,
    pub subprojects: Vec<NavigatorSubproject>,
}

/// `GET /api/me/sites`, the caller's visible sites as a project → subproject → site tree, for the
/// sidebar site navigator. Same visibility rule the CRUD scope filter enforces on `/api/sites`:
/// administrators see every project, other members see exactly their grant set. Keycloak-only,
/// like `/api/me`. Projects and subprojects without sites are omitted.
pub async fn get_my_sites(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
) -> AppResult<Json<Vec<NavigatorProject>>> {
    if !matches!(auth, AuthContext::Keycloak { .. }) {
        return Err(AppError::Forbidden("me_requires_keycloak".to_string()));
    }
    let project_ids = auth.access_scope().project_ids();

    const TREE_SQL: &str = "SELECT p.id AS project_id, p.name AS project_name, \
            sp.id AS subproject_id, sp.name AS subproject_name, \
            s.id AS site_id, s.name AS site_name \
         FROM sites s \
         JOIN projects p ON p.id = s.project_id \
         LEFT JOIN subprojects sp ON sp.id = s.subproject_id";
    const TREE_ORDER: &str =
        " ORDER BY LOWER(p.name), p.id, LOWER(COALESCE(sp.name, '')), sp.id, LOWER(s.name), s.id";
    let stmt = match project_ids.as_deref() {
        None => Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            format!("{TREE_SQL}{TREE_ORDER}"),
            [],
        ),
        Some([]) => return Ok(Json(Vec::new())),
        Some(ids) => Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            format!("{TREE_SQL} WHERE p.id = ANY($1){TREE_ORDER}"),
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

    // Rows arrive grouped by the ORDER BY; fold them into the tree in one pass.
    let mut projects: Vec<NavigatorProject> = Vec::new();
    for row in &rows {
        let (Ok(project_id), Ok(site_id), Ok(site_name)) = (
            row.try_get::<Uuid>("", "project_id"),
            row.try_get::<Uuid>("", "site_id"),
            row.try_get::<String>("", "site_name"),
        ) else {
            continue;
        };
        let subproject_id = row
            .try_get::<Option<Uuid>>("", "subproject_id")
            .unwrap_or(None);

        if projects.last().is_none_or(|p| p.project_id != project_id) {
            projects.push(NavigatorProject {
                project_id,
                name: row.try_get("", "project_name").unwrap_or_default(),
                subprojects: Vec::new(),
            });
        }
        let project = projects.last_mut().expect("just pushed");
        if project
            .subprojects
            .last()
            .is_none_or(|sp| sp.id != subproject_id)
        {
            project.subprojects.push(NavigatorSubproject {
                id: subproject_id,
                name: row
                    .try_get::<Option<String>>("", "subproject_name")
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                sites: Vec::new(),
            });
        }
        let subproject = project.subprojects.last_mut().expect("just pushed");
        subproject.sites.push(NavigatorSite {
            id: site_id,
            name: site_name,
        });
    }
    Ok(Json(projects))
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
