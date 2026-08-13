//! The one rule deciding when readings sharing an instant form a `samples` row, and the one SQL
//! materialiser the backfill paths use to apply it.
//!
//! The database triggers on `readings` answer a different question: given a `sample_id`, what are
//! that sample's statistics. They never create or delete a `samples` row, so the rule below is the
//! only place the existence of a sample is decided.

use sea_orm::{ConnectionTrait, Statement};

use crate::common::bulk_write;
use crate::error::AppResult;

/// Grabs are spot measurements: a bottle, not a logger cadence.
const SPOT: &str = "spot";

/// A `(site, parameter, instant)` group forms a `samples` row when the writer declared it a
/// collection event, or when it carries two or more spot readings on a paired slot.
///
/// Replicate count alone cannot decide. A grab measured once is still a collection event and has to
/// reach every view that reads `samples`, while two logger points sharing a timestamp are a
/// malformed file rather than a sampling event, which is why the backfill paths also require the
/// reading to be classified `spot`.
#[must_use]
pub const fn forms_sample(declared_collection: bool, replicates: usize) -> bool {
    declared_collection || replicates >= 2
}

/// Find-or-create the `samples` rows for the undeclared groups a backfill selects, then stamp
/// `sample_id` onto the readings in those groups.
///
/// `row_predicate` is SQL over the aliases `r` (`readings`) and `ds` (`data_streams`) taking one
/// bind value, eg. `r.stream_id = $1`. Grouping is always by `(site_id, parameter_id, time)`, which
/// is the `samples` unique key, so the find-or-create and the stamping cannot disagree about what a
/// group is.
pub async fn materialise_backfilled_samples<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    bind: sea_orm::Value,
) -> AppResult<()> {
    let group_select = format!(
        "SELECT r.site_id, r.parameter_id, r.time
         FROM readings r
         JOIN data_streams ds ON r.stream_id = ds.id
         WHERE {row_predicate}
           AND r.sample_id IS NULL
           AND r.site_id IS NOT NULL
           AND r.measurement_type = '{SPOT}'
         GROUP BY r.site_id, r.parameter_id, r.time
         HAVING COUNT(*) >= 2"
    );

    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO samples (site_id, parameter_id, collected_at)
             {group_select}
             ON CONFLICT (site_id, parameter_id, collected_at) DO NOTHING"
        ),
        [bind.clone()],
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE readings
             SET sample_id = s.id
             FROM ({group_select}) g
             JOIN samples s
               ON s.site_id = g.site_id
              AND s.parameter_id = g.parameter_id
              AND s.collected_at = g.time
             WHERE readings.site_id = g.site_id
               AND readings.parameter_id = g.parameter_id
               AND readings.time = g.time
               AND readings.sample_id IS NULL
               AND readings.measurement_type = '{SPOT}'"
        ),
        [bind],
    ))
    .await?;

    Ok(())
}
