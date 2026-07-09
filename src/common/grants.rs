//! Per-user project grants: the set of projects a non-admin Keycloak member may see and act in
//! (`user_project_grants`). Loaded on every non-admin request through a short-TTL cache keyed by the
//! user's `sub`. Administrators are exempt (they are unrestricted and never queried). Grant
//! mutations bust the cache for the affected user so a revoke takes effect within one request.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use uuid::Uuid;

/// Cache of `sub → granted project ids`. Cheap to clone (an `Arc<HashSet>`), so callers hold the
/// `Arc` directly on the `AuthContext`.
pub type GrantsCache = Cache<String, Arc<HashSet<Uuid>>>;

#[must_use]
pub fn new_grants_cache(ttl_seconds: u64) -> GrantsCache {
    Cache::builder()
        .max_capacity(10_000)
        .time_to_live(Duration::from_secs(ttl_seconds.max(1)))
        .build()
}

/// The projects granted to `sub`, from cache or a single indexed query. Returns an empty set (which
/// fails closed — the member sees nothing) on any DB error rather than erroring the request; the
/// access gate has already confirmed the member holds a river role, so an empty portal is the safe
/// degradation.
pub async fn load_grants(
    db: &DatabaseConnection,
    cache: &GrantsCache,
    sub: &str,
) -> Arc<HashSet<Uuid>> {
    if let Some(hit) = cache.get(sub).await {
        return hit;
    }
    let grants = query_grants(db, sub).await.unwrap_or_default();
    let grants = Arc::new(grants);
    cache.insert(sub.to_string(), grants.clone()).await;
    grants
}

async fn query_grants(db: &DatabaseConnection, sub: &str) -> Result<HashSet<Uuid>, sea_orm::DbErr> {
    let rows = db
        .query_all(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT project_id FROM user_project_grants WHERE user_sub = $1",
            [sub.into()],
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<Uuid>("", "project_id").ok())
        .collect())
}
