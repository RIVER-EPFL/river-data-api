//! The site lookup `/notes/register` makes before it upserts, and the hook the CRUD routes run.

use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use sea_orm::sea_query::{Expr, ExprTrait, Func};
use sea_orm::{ConnectionTrait, EntityTrait, QueryFilter, TransactionTrait};
use std::collections::HashMap;
use uuid::Uuid;

use super::models::Note;
use crate::error::AppResult;
use crate::routes::private::sites::models as sites;
use crate::routes::private::sites::source_links;

/// The sites a source's station names resolve to, keyed by lowercased name: through the source's
/// own links first, so a renamed site keeps its notes, then by site name case-insensitively. A
/// station river-data has never seen is absent, which is what the caller reports as `unresolved`.
pub async fn sites_by_station<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    names: &[String],
) -> AppResult<HashMap<String, Uuid>> {
    let mut names: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let links = source_links::for_source(db, source_system).await?;
    let lowered: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
    let rows = sites::Entity::find()
        .filter(Expr::expr(Func::lower(Expr::col(sites::Column::Name))).is_in(lowered))
        .all(db)
        .await?;
    let mut resolved: HashMap<String, Uuid> = rows
        .into_iter()
        .map(|row| (row.name.to_lowercase(), row.id))
        .collect();
    for name in &names {
        if let Some(id) = links.get(name) {
            resolved.insert(name.to_lowercase(), *id);
        }
    }
    Ok(resolved)
}

pub struct NoteOperations;

impl CRUDOperations for NoteOperations {
    type Resource = Note;

    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        _id: Uuid,
        data: &<Note as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        crate::common::actor::refuse_reattribution(data.created_by.is_some())
    }
}
