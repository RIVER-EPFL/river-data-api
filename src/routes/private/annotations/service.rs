//! The two lookups `/annotations/register` makes before it upserts: the slot behind each stream,
//! and the rows the pass re-asserts.

use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::HashMap;
use uuid::Uuid;

use super::models::{Column, Entity, Model};
use crate::error::AppResult;
use crate::routes::private::{data_streams, site_parameters as site_parameters};

/// `(site_id, parameter_id)` per paired stream. A stream with no pairing is absent, which is what
/// the caller reports as `unpaired`.
pub async fn slots_by_stream<C: ConnectionTrait>(
    db: &C,
    stream_ids: &[Uuid],
) -> AppResult<HashMap<Uuid, (Uuid, Uuid)>> {
    let mut stream_ids = stream_ids.to_vec();
    stream_ids.sort_unstable();
    stream_ids.dedup();
    let streams = data_streams::Entity::find()
        .filter(data_streams::Column::Id.is_in(stream_ids))
        .all(db)
        .await?;
    let mut sp_ids: Vec<Uuid> = streams.iter().filter_map(|s| s.site_parameter_id).collect();
    sp_ids.sort_unstable();
    sp_ids.dedup();
    let slots = site_parameters::Entity::find()
        .filter(site_parameters::Column::Id.is_in(sp_ids))
        .all(db)
        .await?;
    let slot_by_id: HashMap<Uuid, (Uuid, Uuid)> = slots
        .iter()
        .map(|sp| (sp.id, (sp.site_id, sp.parameter_id)))
        .collect();
    Ok(streams
        .iter()
        .filter_map(|s| {
            s.site_parameter_id
                .and_then(|sp| slot_by_id.get(&sp))
                .map(|slot| (s.id, *slot))
        })
        .collect())
}

/// The stored annotations of this source under the given keys, keyed by source key. A row that
/// already names a curve is frozen, and which half the source moved is what the outcome reports.
pub async fn stored_by_source_key<C: ConnectionTrait>(
    db: &C,
    source_system: &str,
    keys: &[String],
) -> AppResult<HashMap<String, Model>> {
    let mut keys = keys.to_vec();
    keys.sort_unstable();
    keys.dedup();
    Ok(Entity::find()
        .filter(Column::SourceSystem.eq(source_system))
        .filter(Column::SourceKey.is_in(keys))
        .all(db)
        .await?
        .into_iter()
        .filter_map(|a| a.source_key.clone().map(|key| (key, a)))
        .collect())
}
