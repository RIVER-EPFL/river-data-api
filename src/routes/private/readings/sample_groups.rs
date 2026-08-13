//! The one rule deciding when readings sharing an instant form a `samples` row, and the one SQL
//! materialiser every path that applies the rule after the readings are written goes through.
//!
//! The database triggers on `readings` answer a different question: given a `sample_id`, what are
//! that sample's statistics, and does any reading still point at it. They never create a `samples`
//! row, so the rule below is the only place a sample's existence begins. A sample nothing references
//! any more is reaped by the trigger, which is garbage collection rather than a judgement about what
//! a group is.

use sea_orm::{ConnectionTrait, Statement};

use crate::common::bulk_write;
use crate::error::AppResult;

/// Grabs are spot measurements: a bottle, not a logger cadence.
pub const SPOT: &str = "spot";

/// How many readings a group needs before it is a sample. A writer that declared a collection event
/// has said so from the first measurement; an undeclared group has only its replicate count to
/// argue with.
const fn min_replicates(declared_collection: bool) -> usize {
    if declared_collection { 1 } else { 2 }
}

/// A `(site, parameter, instant)` group forms a `samples` row when the writer declared it a
/// collection event, or when it carries two or more spot readings on a paired slot.
///
/// Replicate count alone cannot decide. A grab measured once is still a collection event and has to
/// reach every view that reads `samples`, while two logger points sharing a timestamp are a
/// malformed file rather than a sampling event, which is why every caller also requires the reading
/// to be classified `spot`.
#[must_use]
pub const fn forms_sample(declared_collection: bool, replicates: usize) -> bool {
    replicates >= min_replicates(declared_collection)
}

/// Find-or-create the `samples` rows for the groups a selection of readings forms, then stamp
/// `sample_id` onto the readings of those groups.
///
/// `row_predicate` is SQL over the aliases `r` (`readings`) and `ds` (`data_streams`), taking the
/// bind values given, eg. `r.stream_id = $1`. It selects which readings are in scope and is applied
/// to the grouping and to the stamping alike, so the stamping cannot reach an unrelated stream's
/// reading that happens to sit on the same slot at the same instant. Grouping is always by
/// `(site_id, parameter_id, time)`, which is the `samples` unique key, so the find-or-create and
/// the stamping cannot disagree about what a group is.
///
/// `declared_collection` is the writer stating that these readings are a collection event, which is
/// what lets a single row form a sample. Backfills, which have no such statement, pass `false`.
pub async fn materialise_samples<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    binds: Vec<sea_orm::Value>,
    declared_collection: bool,
) -> AppResult<()> {
    let minimum = min_replicates(declared_collection);
    let group_select = format!(
        "SELECT r.site_id, r.parameter_id, r.time
         FROM readings r
         JOIN data_streams ds ON r.stream_id = ds.id
         WHERE {row_predicate}
           AND r.sample_id IS NULL
           AND r.site_id IS NOT NULL
           AND r.parameter_id IS NOT NULL
           AND r.measurement_type = '{SPOT}'
         GROUP BY r.site_id, r.parameter_id, r.time
         HAVING COUNT(*) >= {minimum}"
    );

    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO samples (site_id, parameter_id, collected_at)
             {group_select}
             ON CONFLICT (site_id, parameter_id, collected_at) DO NOTHING"
        ),
        binds.clone(),
    ))
    .await?;

    // The stamping UPDATE can reach chunks the compression policy already closed.
    bulk_write::lift_decompression_cap(conn).await?;
    conn.execute(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "UPDATE readings r
             SET sample_id = s.id
             FROM data_streams ds, ({group_select}) g
             JOIN samples s
               ON s.site_id = g.site_id
              AND s.parameter_id = g.parameter_id
              AND s.collected_at = g.time
             WHERE r.stream_id = ds.id
               AND {row_predicate}
               AND r.site_id = g.site_id
               AND r.parameter_id = g.parameter_id
               AND r.time = g.time
               AND r.sample_id IS NULL
               AND r.measurement_type = '{SPOT}'"
        ),
        binds,
    ))
    .await?;

    Ok(())
}

/// The backfill spelling of [`materialise_samples`]: a pairing or plan-apply backfill knows nothing
/// about the writer's intent, so only a group of two or more spot readings forms a sample.
pub async fn materialise_backfilled_samples<C: ConnectionTrait>(
    conn: &C,
    row_predicate: &str,
    bind: sea_orm::Value,
) -> AppResult<()> {
    materialise_samples(conn, row_predicate, vec![bind], false).await
}
