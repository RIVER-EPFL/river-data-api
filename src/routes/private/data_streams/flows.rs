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
//! the slot reprocess every caller enqueues on the pairing's own transaction.

use async_trait::async_trait;
use sea_orm::sea_query::{Alias, Expr, Func, PostgresQueryBuilder, Query, UpdateStatement};
use sea_orm::{Condition, ConnectionTrait, DbErr, EntityTrait, Statement, TransactionTrait};
use uuid::Uuid;

use super::models::{Backfilled, SlotScope};
use super::service::{
    lock_released_streams, referenced_event_pairs, release_slot_rows, resolve_retire_target,
};
use crate::common::bulk_write::{self, TouchedRange};
use crate::error::{AppError, AppResult};
use crate::routes::private::collection_events::flows::{events_from_pairs, touched_events};
use crate::routes::private::collection_events::service::{EventSource, attach_collection_events};
use crate::routes::private::data_streams::models as data_streams;
use crate::routes::private::readings::models as readings;
use crate::routes::private::readings::service::materialise_samples;
use crate::routes::private::readings::status_events::models as status_events;
use crate::routes::private::reprocessing_jobs::flows::required_uuid;
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport};
use crate::routes::private::site_parameters::models as site_parameters;
use crate::routes::private::site_parameters::service::queue_moved_rollups;
use crate::routes::private::sync::service::{
    HoldScope, has_plan_attribution, queue_plan_attribution, repoint_holds,
};

/// The scope as a predicate over `data_streams ds`, which is the one table all four statements
/// and the three shared helpers join.
pub(crate) fn predicate(scope: HoldScope) -> Condition {
    use sea_orm::sea_query::ExprTrait;

    // The shared helpers join the streams table as `ds`.
    let ds = Alias::new("ds");
    Condition::all().add(match scope {
        HoldScope::Stream(id) => Expr::col((ds, data_streams::Column::Id)).eq(id),
        HoldScope::Plan(id) => Expr::col((ds, data_streams::Column::PairingPlanId)).eq(id),
    })
}

/// The same scope over the streams table itself, for the statements that name it rather than
/// joining it.
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

/// Drop the newly paired streams' handshake digest. The stored digest claims the server already
/// applied that content, which pairing makes untrue: annotations the source sent while the stream
/// was unpaired were refused, so the next pass has to carry them again.
fn forget_window_digests(scope: HoldScope) -> UpdateStatement {
    Query::update()
        .table(data_streams::Entity)
        .value(
            data_streams::Column::LastWindowDigest,
            Expr::val(Option::<String>::None),
        )
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
    let scoped = predicate(scope);

    // The backfill reaches chunks the compression policy has already closed.
    bulk_write::lift_decompression_cap(conn).await?;

    let readings =
        bulk_write::mutation_rows(conn, attribute_readings(scope, deployment_id)).await?;

    // Replicate groups on the newly paired streams (2+ spot readings sharing a slot and timestamp,
    // e.g. migrated NOMIS A/B/C rows) form samples. The row-level triggers populate the statistics.
    materialise_samples(conn, scoped.clone()).await?;

    // Attribution arriving is what makes these spot readings addressable as visits: attach their
    // collection events now, deriving the source from where each stream came from.
    attach_collection_events(conn, scoped.clone(), EventSource::ByStreamOrigin).await?;
    let touched = touched_events(conn, scoped).await?;

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

    let (sql, values) = forget_window_digests(scope).build(PostgresQueryBuilder);
    conn.execute_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await?;

    Ok(Backfilled {
        readings,
        touched_events: touched,
    })
}

/// Pair an unpaired entry channel to the slot it was opened for, now that the slot exists, with
/// the backfill every pairing owes and the recompute of every manual visit the backfill attributed
/// readings at, queued under `actor` in the pairing's own transaction. A channel already paired
/// keeps its pairing, and one whose slot does not exist stays unpaired.
///
/// No deployment is opened: an entry channel's instrument is not stationed at the site, which is
/// how the channel is paired when it is created with its slot already in place.
pub async fn pair_entry_channel(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
    slot: Option<Uuid>,
    actor: &str,
) -> AppResult<()> {
    let stream = data_streams::Entity::find_by_id(stream_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Stream not found".to_string()))?;
    let (None, Some(site_parameter_id)) = (stream.site_parameter_id, slot) else {
        return Ok(());
    };
    let sp = site_parameters::Entity::find_by_id(site_parameter_id)
        .one(db)
        .await?
        .ok_or_else(|| AppError::NotFound("Site parameter not found".to_string()))?;
    bulk_write::guarded(db, async |txn| {
        let claimed =
            super::service::claim_stream(stream_id, site_parameter_id, chrono::Utc::now().into())
                .exec(txn)
                .await?
                .rows_affected;
        if claimed == 0 {
            return Ok(());
        }
        let done = backfill(txn, HoldScope::Stream(stream_id), None).await?;
        enqueue_slot_reprocess(txn, stream_id, (sp.site_id, sp.parameter_id), done.readings)
            .await?;
        crate::routes::private::collection_events::flows::enqueue_for(
            txn,
            &done.touched_events,
            actor,
            crate::routes::private::collection_events::flows::Writer::Person,
        )
        .await?;
        Ok(())
    })
    .await
}

/// Queue the window reprocess a newly paired stream's slot owes, on the pairing's own transaction:
/// each reading is re-attributed to the deployment covering its own time and corrected by the
/// curve covering it, and the rollups and derived values follow.
///
/// Gated on the stream holding readings at all, not on the backfill having moved rows: a stream
/// re-paired after an unpair, or one whose readings arrived already attributed, backfills nothing
/// and still needs its window resolved against the slot it now feeds.
pub async fn enqueue_slot_reprocess<C: ConnectionTrait>(
    db: &C,
    stream_id: Uuid,
    (site_id, parameter_id): (Uuid, Uuid),
    backfilled: u64,
) -> AppResult<()> {
    use sea_orm::{ColumnTrait, QueryFilter};

    let has_readings = backfilled > 0
        || readings::Entity::find()
            .filter(readings::Column::StreamId.eq(stream_id))
            .one(db)
            .await?
            .is_some();
    if !has_readings {
        return Ok(());
    }
    crate::routes::private::reprocessing_jobs::service::enqueue(
        db,
        "pairing_backfill",
        None,
        Some(stream_id),
        &serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
        None,
    )
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(())
}

/// Release everything a slot owns, in one transaction with the decompression cap lifted, and queue
/// the rollup rebuild that has to follow it. Returns the reading span removed from the rollups.
///
/// This is the whole teardown, in the order that keeps it recoverable: the samples a scope
/// references are collected before the readings lose their `sample_id`, the readings and status
/// events are unattributed rather than deleted (the measurement outlives the slot), the samples
/// nothing references any more are deleted, and a slot that is going away releases the streams
/// pointing at it last. Unpair, and a `site_parameters` delete, are the same operation over
/// different scopes. The recompute of every manual visit that lost an input is queued in the same
/// transaction, under `actor`, so a release never commits without it.
///
/// The rollups are refreshed over what the teardown touched by a tracked `refresh_aggregates`
/// job, queued in the same transaction, rather than inline: a teardown can span a stream's whole
/// history, and a refresh that fails then belongs in `/jobs`, where it is visible and rerunnable,
/// not as a 500 on an operation that already committed.
///
/// A slot the scope cannot resolve reports an empty range rather than an error, so retiring a row
/// that is already gone is not a failure.
pub async fn retire_slot<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    scope: SlotScope,
    actor: &str,
) -> AppResult<TouchedRange> {
    let trigger_id = match scope {
        SlotScope::Stream(id) | SlotScope::SiteParameter(id) => id,
    };
    bulk_write::guarded(db, async |txn| {
        lock_released_streams(txn, scope.streams()).await?;
        let Some(target) = resolve_retire_target(txn, scope).await? else {
            return Ok(TouchedRange::default());
        };
        let pairs = referenced_event_pairs(txn, &target).await?;
        let touched_events = events_from_pairs(txn, &pairs).await?;
        let touched = release_slot_rows(txn, &target).await?;
        crate::routes::private::collection_events::flows::enqueue_for(
            txn,
            &touched_events,
            actor,
            crate::routes::private::collection_events::flows::Writer::Person,
        )
        .await?;
        queue_moved_rollups(txn, &touched, trigger_id).await?;
        Ok(touched)
    })
    .await
}

/// The status a guarded plan job's work leaves behind, read before it runs.
///
/// A lease lost after the run committed is reclaimed by the reaper and the job runs again. The
/// guard inside `apply_plan`/`revert_plan` then refuses the plan for being past its starting
/// status, which the worker records as a failure over work that in fact succeeded, so the replay
/// is recognised here and reported instead.
async fn plan_status<C: ConnectionTrait>(db: &C, plan_id: Uuid) -> Result<Option<String>, DbErr> {
    Ok(
        crate::routes::private::data_streams::pairing_plans::Entity::find_by_id(plan_id)
            .one(db)
            .await?
            .map(|p| p.status),
    )
}

/// Apply a pairing plan: resolve entities, execute pairings, backfill readings, mark the plan
/// `applied`. The status transition is guarded (only a `draft` plan applies), and a re-execution
/// after a lost lease finds the plan already applied and reports a replay, queueing the plan's
/// attribution if no job for it exists; not offered as a rerun. Backs the `apply_pairing_plan`
/// operator action.
pub struct PlanApply;

#[async_trait]
impl Job for PlanApply {
    fn name(&self) -> &'static str {
        "plan_apply"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = required_uuid(ctx.params(), "plan_id")?;
        if plan_status(ctx.db(), plan_id).await?.as_deref() == Some("applied") {
            let mut report = JobReport::new().scope("plan_id", plan_id.to_string());
            if has_plan_attribution(ctx.db(), plan_id).await? {
                ctx.info("Plan is already applied; this run is a replay and changed nothing")
                    .await;
            } else {
                queue_plan_attribution(ctx.db(), plan_id, Some(ctx.job_id())).await?;
                report = report.count("attribution_queued", 1);
                ctx.info("Plan is already applied but its attribution was never queued; queued it")
                    .await;
            }
            ctx.report(report).await;
            return Ok(0);
        }
        let result =
            crate::routes::private::sync::service::apply_plan(ctx.db(), plan_id, Some(&ctx))
                .await
                .map_err(|e| DbErr::Custom(e.to_string()))?;
        // Every counter the apply produced, so a reader of the run knows what it created as well
        // as what it paired; the plan's own `apply_result` records the same nine numbers.
        ctx.report(
            JobReport::new()
                .scope("plan_id", plan_id.to_string())
                .count("projects_created", result.projects_created)
                .count("sites_created", result.sites_created)
                .count("parameters_created", result.parameters_created)
                .count("site_parameters_created", result.site_parameters_created)
                .count("streams_paired", result.streams_paired)
                .count("streams_skipped", result.streams_skipped)
                .count("instruments_created", result.instruments_created)
                .count("curves_assigned", result.curves_assigned)
                .count("readings_backfilled", result.readings_backfilled),
        )
        .await;
        ctx.info(&format!(
            "Applied plan: {} streams paired, {} readings backfilled",
            result.streams_paired, result.readings_backfilled
        ))
        .await;
        Ok(i64::try_from(result.readings_backfilled).unwrap_or(i64::MAX))
    }
}

/// Revert an applied pairing plan: unpair every stream it touched, restoring the prior state, and
/// mark the plan `reverted`. The status transition is guarded (only an `applied` plan reverts), and
/// a re-execution after a lost lease finds the plan already reverted and reports a replay; not
/// offered as a rerun. Backs the
/// `revert_pairing_plan` operator action.
pub struct PlanRevert;

#[async_trait]
impl Job for PlanRevert {
    fn name(&self) -> &'static str {
        "plan_revert"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = required_uuid(ctx.params(), "plan_id")?;
        if plan_status(ctx.db(), plan_id).await?.as_deref() == Some("reverted") {
            ctx.report(JobReport::new().scope("plan_id", plan_id.to_string()))
                .await;
            ctx.info("Plan is already reverted; this run is a replay and changed nothing")
                .await;
            return Ok(0);
        }
        let reverted = crate::routes::private::sync::service::revert_plan(
            ctx.db(),
            plan_id,
            Some(ctx.events()),
        )
        .await
        .map_err(|e| DbErr::Custom(e.to_string()))?;
        ctx.report(
            JobReport::new()
                .scope("plan_id", plan_id.to_string())
                .count("reverted", reverted),
        )
        .await;
        ctx.info(&format!("Reverted plan: {reverted} streams unpaired"))
            .await;
        Ok(i64::from(reverted))
    }
}

#[cfg(test)]
#[path = "tests/flows.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/backfill.rs"]
mod backfill_tests;
