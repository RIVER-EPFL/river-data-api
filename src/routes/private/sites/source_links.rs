//! The names a source knows a site by. A renamed site keeps its links, so every later stream and
//! record the source sends under its own name still resolves to it.

use sea_orm::entity::prelude::*;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ConnectionTrait, Set};
use std::collections::HashMap;

use crate::error::AppResult;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "site_source_links")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_system: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub source_key: String,
    pub site_id: Uuid,
    pub created_at: DateTimeWithTimeZone,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Every link one source system holds, keyed by source name.
pub async fn for_source<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
) -> AppResult<HashMap<String, Uuid>> {
    let rows = Entity::find()
        .filter(Column::SourceSystem.eq(source_system))
        .all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.source_key, row.site_id))
        .collect())
}

/// The site a source name is linked to.
pub async fn find<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    source_key: &str,
) -> AppResult<Option<Uuid>> {
    Ok(
        Entity::find_by_id((source_system.to_string(), source_key.to_string()))
            .one(db)
            .await?
            .map(|row| row.site_id),
    )
}

/// Point a source name at a site, replacing whatever it named before.
pub async fn link<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    source_key: &str,
    site_id: Uuid,
) -> AppResult<()> {
    Entity::insert(ActiveModel {
        source_system: Set(source_system.to_string()),
        source_key: Set(source_key.to_string()),
        site_id: Set(site_id),
        created_at: Set(chrono::Utc::now().into()),
    })
    .on_conflict(
        OnConflict::columns([Column::SourceSystem, Column::SourceKey])
            .update_column(Column::SiteId)
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}
