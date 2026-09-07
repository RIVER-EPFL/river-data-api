//! The work a pairing owes its slot, once and for every path that pairs.
//!
//! Attribution arriving is what makes a stream's history serveable: the readings and status
//! events gain their site, parameter and instrument, replicate groups form samples, spot instants
//! become visits, and the holds recorded while the stream was unpaired join the review queue.
//! Pairing one stream by hand and pairing the same stream through a plan differ only in the scope
//! they cover, so the scope is the argument and the rest is this function.
//!
//! No curve is stamped and no value computed. The instrument's newest calibration is not in
//! general the one covering a given reading's time, so which curve corrects a reading is left to
//! the slot reprocess every caller enqueues post-commit.

use sea_orm::{ConnectionTrait, Statement};
use uuid::Uuid;

use crate::common::bulk_write;
use crate::error::AppResult;
use crate::routes::private::collection_events::attach::{EventSource, attach_collection_events};
use crate::routes::private::collection_events::recompute::{TouchedEvent, touched_events};
use crate::routes::private::readings::sample_groups::materialise_samples;
use crate::routes::private::sync::replicate_audit::{HoldScope, repoint_holds};

/// What one backfill moved, and the visits whose calculations it owes a run.
pub struct Backfilled {
    pub readings: u64,
    pub touched_events: Vec<TouchedEvent>,
}

/// The scope as a predicate over `data_streams ds`, which is the one table all four statements
/// and the three shared helpers join.
fn predicate(scope: HoldScope) -> (&'static str, Vec<sea_orm::Value>) {
    match scope {
        HoldScope::Stream(id) => ("ds.id = $1", vec![id.into()]),
        HoldScope::Plan(id) => ("ds.pairing_plan_id = $1", vec![id.into()]),
    }
}

/// Attribute everything the newly paired streams already hold.
///
/// Runs inside the caller's transaction: the readings must not be half attributed if a later step
/// fails, and the reprocess the caller enqueues afterwards is what resolves each reading's curve
/// from its own time.
pub async fn backfill<C: ConnectionTrait>(
    conn: &C,
    scope: HoldScope,
    deployment_id: Option<Uuid>,
) -> AppResult<Backfilled> {
    let (scope_sql, binds) = predicate(scope);

    // The backfill reaches chunks the compression policy has already closed.
    bulk_write::lift_decompression_cap(conn).await?;

    let mut reading_binds = binds.clone();
    reading_binds.push(deployment_id.into());
    let readings = bulk_write::mutation(
        conn,
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                r"UPDATE readings r
                  SET site_id = sp.site_id, parameter_id = sp.parameter_id,
                      sensor_id = COALESCE(ds.sensor_id, r.sensor_id),
                      deployment_id = COALESCE($2::uuid, r.deployment_id),
                      measurement_type = COALESCE(r.measurement_type, ds.measurement_type)
                  FROM data_streams ds
                  JOIN site_parameters sp ON ds.site_parameter_id = sp.id
                  WHERE r.stream_id = ds.id AND r.site_id IS NULL AND {scope_sql}"
            ),
            reading_binds,
        ),
    )
    .await?
    .rows;

    // Replicate groups on the newly paired streams (2+ spot readings sharing a slot and timestamp,
    // e.g. migrated NOMIS A/B/C rows) form samples. The row-level triggers populate the statistics.
    materialise_samples(conn, scope_sql, binds.clone()).await?;

    // Attribution arriving is what makes these spot readings addressable as visits: attach their
    // collection events now, deriving the source from where each stream came from.
    attach_collection_events(
        conn,
        scope_sql,
        binds.clone(),
        EventSource::ByStreamOrigin,
    )
    .await?;
    let touched = touched_events(conn, scope_sql, binds.clone()).await?;

    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            r"UPDATE status_events se
              SET site_id = sp.site_id, parameter_id = sp.parameter_id,
                  sensor_id = COALESCE(ds.sensor_id, se.sensor_id)
              FROM data_streams ds
              JOIN site_parameters sp ON ds.site_parameter_id = sp.id
              WHERE se.stream_id = ds.id AND se.site_id IS NULL AND {scope_sql}"
        ),
        binds.clone(),
    ))
    .await?;

    // Audit mismatches recorded while the streams were unpaired become reviewable now that the
    // data serves a slot.
    repoint_holds(conn, scope, true).await?;

    Ok(Backfilled {
        readings,
        touched_events: touched,
    })
}

#[cfg(test)]
mod tests {
    use super::{HoldScope, predicate};
    use uuid::Uuid;

    #[test]
    fn each_scope_names_the_column_it_is_keyed_on() {
        let (sql, binds) = predicate(HoldScope::Stream(Uuid::nil()));
        assert_eq!(sql, "ds.id = $1");
        assert_eq!(binds.len(), 1);
        let (sql, binds) = predicate(HoldScope::Plan(Uuid::nil()));
        assert_eq!(sql, "ds.pairing_plan_id = $1");
        assert_eq!(binds.len(), 1);
    }
}
