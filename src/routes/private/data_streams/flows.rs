//! What a pairing and a retirement owe their slot, once and for every path that reaches them.
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

use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use uuid::Uuid;

use super::models::{Backfilled, SlotScope};
use super::service::{release_slot_rows, resolve_retire_target};
use crate::common::bulk_write::{self, TouchedRange};
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::flows::touched_events;
use crate::routes::private::collection_events::service::{EventSource, attach_collection_events};
use crate::routes::private::readings::sample_groups::materialise_samples;
use crate::routes::private::sync::service::{HoldScope, repoint_holds};

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
    attach_collection_events(conn, scope_sql, binds.clone(), EventSource::ByStreamOrigin).await?;
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

/// Release everything a slot owns, in one transaction with the decompression cap lifted, and queue
/// the rollup rebuild that has to follow it.
///
/// This is the whole teardown, in the order that keeps it recoverable: the samples a scope
/// references are collected before the readings lose their `sample_id`, the readings and status
/// events are unattributed rather than deleted (the measurement outlives the slot), the samples
/// nothing references any more are deleted, and a slot that is going away releases the streams
/// pointing at it last. Unpair, and a `site_parameters` delete, are the same operation over
/// different scopes.
///
/// The rollups are rebuilt by a tracked `refresh_aggregates_full` job rather than inline: a
/// teardown can span a stream's whole history, and a refresh that fails then belongs in `/jobs`,
/// where it is visible and rerunnable, not as a 500 on an operation that already committed.
///
/// A slot the scope cannot resolve reports an empty range rather than an error, so retiring a row
/// that is already gone is not a failure.
pub async fn retire_slot<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    scope: SlotScope,
) -> AppResult<TouchedRange> {
    let touched = bulk_write::guarded(db, async |txn| {
        let Some(target) = resolve_retire_target(txn, scope).await? else {
            return Ok(TouchedRange::default());
        };
        release_slot_rows(txn, &target).await
    })
    .await?;

    if !touched.is_empty() {
        let trigger_id = match scope {
            SlotScope::Stream(id) | SlotScope::SiteParameter(id) => id,
        };
        crate::routes::private::reprocessing_jobs::worker::enqueue(
            db,
            "refresh_aggregates_full",
            None,
            Some(trigger_id),
            &serde_json::json!({ "full": true }),
            None,
        )
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    }
    Ok(touched)
}

#[cfg(test)]
#[path = "tests/flows.rs"]
mod tests;
