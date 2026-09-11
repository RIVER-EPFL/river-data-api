use sea_orm::sea_query::OnConflict;
use sea_orm::{ConnectionTrait, EntityTrait};

use super::models::{ActiveModel, Column, Entity};

const BATCH_SIZE: usize = 1000;

/// Insert status events in chunks, leaving a row that already holds the `(stream, time)` slot as it
/// is. Returns the number of rows that landed.
pub async fn insert_ignoring_duplicates<C: ConnectionTrait>(
    db: &C,
    models: Vec<ActiveModel>,
) -> Result<usize, sea_orm::DbErr> {
    let mut inserted = 0usize;
    for chunk in models.chunks(BATCH_SIZE) {
        let rows = Entity::insert_many(chunk.to_vec())
            .on_conflict(
                OnConflict::columns([Column::StreamId, Column::Time])
                    .do_nothing()
                    .to_owned(),
            )
            .exec_without_returning(db)
            .await?;
        inserted += usize::try_from(rows).unwrap_or(usize::MAX);
    }
    Ok(inserted)
}
