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

use sea_orm::sea_query::{Alias, Expr, Func, PostgresQueryBuilder, Query, UpdateStatement};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use uuid::Uuid;

use super::models::{Backfilled, SlotScope};
use super::service::{release_slot_rows, resolve_retire_target};
use crate::common::bulk_write::{self, TouchedRange};
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::flows::{rows_matching, touched_events};
use crate::routes::private::collection_events::service::{EventSource, attach_collection_events};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::service::materialise_samples;
use crate::routes::private::readings::status_events::model as status_events;
use crate::routes::private::sites::parameters::models as site_parameters;
use crate::routes::private::sync::service::{HoldScope, repoint_holds};

/// The scope as a predicate over `data_streams ds`, which is the one table all four statements
/// and the three shared helpers join.
fn predicate(scope: HoldScope) -> (&'static str, Vec<sea_orm::Value>) {
    match scope {
        HoldScope::Stream(id) => ("ds.id = $1", vec![id.into()]),
        HoldScope::Plan(id) => ("ds.pairing_plan_id = $1", vec![id.into()]),
    }
}

/// The same scope, built. The text form above stays until the three shared helpers this file calls
/// take a condition instead of a string.
fn scope_condition(scope: HoldScope) -> Expr {
    use sea_orm::sea_query::ExprTrait;

    // An UPDATE takes no alias, so the streams table is named rather than aliased.
    let ds = Alias::new("data_streams");
    match scope {
        HoldScope::Stream(id) => Expr::col((ds, data_streams::Column::Id)).eq(id),
        HoldScope::Plan(id) => Expr::col((ds, data_streams::Column::PairingPlanId)).eq(id),
    }
}

/// Attribute a newly paired stream's readings from the slot it now serves. A reading already
/// attributed keeps what it has; the instrument and cadence fall back to the stream's own.
fn attribute_readings(scope: HoldScope, deployment_id: Option<Uuid>) -> UpdateStatement {
    use sea_orm::sea_query::ExprTrait;

    let r = Alias::new("readings");
    let ds = Alias::new("data_streams");
    let sp = Alias::new("site_parameters");
    Query::update()
        .table(readings::Entity)
        .value(
            readings::Column::SiteId,
            Expr::col((sp.clone(), site_parameters::Column::SiteId)),
        )
        .value(
            readings::Column::ParameterId,
            Expr::col((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .value(
            readings::Column::SensorId,
            Func::coalesce([
                Expr::col((ds.clone(), data_streams::Column::SensorId)),
                Expr::col((r.clone(), readings::Column::SensorId)),
            ]),
        )
        .value(
            readings::Column::DeploymentId,
            Func::coalesce([
                Expr::val(deployment_id).cast_as(Alias::new("uuid")),
                Expr::col((r.clone(), readings::Column::DeploymentId)),
            ]),
        )
        .value(
            readings::Column::MeasurementType,
            Func::coalesce([
                Expr::col((r.clone(), readings::Column::MeasurementType)),
                Expr::col((ds.clone(), data_streams::Column::MeasurementType)),
            ]),
        )
        .from(data_streams::Entity)
        .from(site_parameters::Entity)
        .and_where(
            Expr::col((ds.clone(), data_streams::Column::SiteParameterId))
                .equals((sp, site_parameters::Column::Id)),
        )
        .and_where(
            Expr::col((r.clone(), readings::Column::StreamId))
                .equals((ds, data_streams::Column::Id)),
        )
        .and_where(Expr::col((r, readings::Column::SiteId)).is_null())
        .and_where(scope_condition(scope))
        .to_owned()
}

/// The same attribution for the non-numeric series, which carry no value to correct.
fn attribute_status_events(scope: HoldScope) -> UpdateStatement {
    use sea_orm::sea_query::ExprTrait;

    let se = Alias::new("status_events");
    let ds = Alias::new("data_streams");
    let sp = Alias::new("site_parameters");
    Query::update()
        .table(status_events::Entity)
        .value(
            status_events::Column::SiteId,
            Expr::col((sp.clone(), site_parameters::Column::SiteId)),
        )
        .value(
            status_events::Column::ParameterId,
            Expr::col((sp.clone(), site_parameters::Column::ParameterId)),
        )
        .value(
            status_events::Column::SensorId,
            Func::coalesce([
                Expr::col((ds.clone(), data_streams::Column::SensorId)),
                Expr::col((se.clone(), status_events::Column::SensorId)),
            ]),
        )
        .from(data_streams::Entity)
        .from(site_parameters::Entity)
        .and_where(
            Expr::col((ds.clone(), data_streams::Column::SiteParameterId))
                .equals((sp, site_parameters::Column::Id)),
        )
        .and_where(
            Expr::col((se.clone(), status_events::Column::StreamId))
                .equals((ds, data_streams::Column::Id)),
        )
        .and_where(Expr::col((se, status_events::Column::SiteId)).is_null())
        .and_where(scope_condition(scope))
        .to_owned()
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

    let readings = bulk_write::mutation(conn, attribute_readings(scope, deployment_id))
        .await?
        .rows;

    // Replicate groups on the newly paired streams (2+ spot readings sharing a slot and timestamp,
    // e.g. migrated NOMIS A/B/C rows) form samples. The row-level triggers populate the statistics.
    materialise_samples(conn, scope_sql, binds.clone()).await?;

    // Attribution arriving is what makes these spot readings addressable as visits: attach their
    // collection events now, deriving the source from where each stream came from.
    attach_collection_events(conn, scope_sql, binds.clone(), EventSource::ByStreamOrigin).await?;
    let touched = touched_events(conn, rows_matching(scope_sql, binds.clone())).await?;

    let (sql, values) = attribute_status_events(scope).build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
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

#[cfg(test)]
#[path = "tests/backfill.rs"]
mod backfill_tests;
