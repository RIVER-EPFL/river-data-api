//! The one place readings acquire a collection event.
//!
//! An attributed spot reading belongs to the visit that produced it, identified by
//! `(site_id, collected_at)` — the portal's `(station, staged timestamp)` row key. Every write
//! path that lands attributed spot readings calls through here after its insert, and the pairing
//! backfill calls through here when attribution arrives late, so the invariant is one helper
//! rather than five copies.

use sea_orm::{ConnectionTrait, Statement};

use crate::common::bulk_write;
use crate::error::AppResult;

/// How the event came to exist, decided by the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Manual,
    PortalSync,
    /// Derive per group from the streams feeding it: a stream this system did not create itself
    /// was registered by a sync service. Used by backfills that span sources.
    ByStreamOrigin,
}

/// `bool_or` SQL deciding whether any stream feeding a group was registered by a sync service:
/// a stream this system did not create itself. Shared with the sample backfill, which uses the
/// same distinction to decide whether a group's readings arrived as declared collections.
pub fn any_sync_origin_sql() -> String {
    "bool_or(ds.source_system NOT IN ('api', 'grab_sample', 'csv', 'csv_import'))".to_string()
}

impl EventSource {
    fn sql(self) -> String {
        match self {
            Self::Manual => "'manual'".to_string(),
            Self::PortalSync => "'portal_sync'".to_string(),
            Self::ByStreamOrigin => format!(
                "CASE WHEN {} THEN 'portal_sync' ELSE 'manual' END",
                any_sync_origin_sql()
            ),
        }
    }
}

/// Find-or-create the `collection_events` rows for the attributed spot readings a predicate
/// selects, then stamp `collection_event_id` onto them.
///
/// `row_predicate` is SQL over the aliases `r` (`readings`) and `ds` (`data_streams`) with the
/// given binds, exactly as `sample_groups::materialise_samples` takes it. The stamping UPDATE can
/// reach compressed chunks, so callers run inside a `bulk_write::guarded` transaction when the
/// window can be historical.
pub async fn attach_collection_events<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    binds: Vec<sea_orm::Value>,
    source: EventSource,
) -> AppResult<()> {
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO collection_events (site_id, collected_at, source)
             SELECT r.site_id, r.time, {source}
             FROM readings r
             JOIN data_streams ds ON r.stream_id = ds.id
             WHERE {row_predicate}
               AND r.collection_event_id IS NULL
               AND r.site_id IS NOT NULL
               AND r.measurement_type = 'spot'
             GROUP BY r.site_id, r.time
             ON CONFLICT (site_id, collected_at) DO NOTHING",
            source = source.sql(),
        ),
        binds.clone(),
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE readings r SET collection_event_id = ce.id
             FROM data_streams ds, collection_events ce
             WHERE r.stream_id = ds.id
               AND {row_predicate}
               AND r.collection_event_id IS NULL
               AND r.site_id IS NOT NULL
               AND r.measurement_type = 'spot'
               AND ce.site_id = r.site_id AND ce.collected_at = r.time"
        ),
        binds,
    ))
    .await?;

    Ok(())
}
