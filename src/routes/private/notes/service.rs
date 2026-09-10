//! The site lookup `/notes/register` makes before it upserts.

use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::AppResult;
use crate::routes::private::sites::models as sites;

/// Sites matching the given names case-insensitively, keyed by lowercased name. A station
/// river-data has never seen is absent, which is what the caller reports as `unresolved`.
pub async fn sites_by_name<C: ConnectionTrait>(
    db: &C,
    names: &[String],
) -> AppResult<HashMap<String, Uuid>> {
    let mut names: Vec<String> = names.iter().map(|n| n.trim().to_lowercase()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sites::Entity::find()
        .filter(Expr::expr(Func::lower(Expr::col(sites::Column::Name))).is_in(names))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.name.to_lowercase(), row.id))
        .collect())
}
