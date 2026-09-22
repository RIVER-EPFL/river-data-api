//! Sync orchestration: the jobs that drive the ledger sweeps.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sea_orm::sea_query::extension::postgres::PgBinOper;
use sea_orm::sea_query::{Alias, Condition, Expr, ExprTrait as _, Query as SeaQuery};
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};
use uuid::Uuid;

use crate::config::Config;
use crate::routes::private::data_streams::models::receipts;
use crate::routes::private::readings::models as readings;
use crate::routes::private::reprocessing_jobs::service::{Job, JobContext, JobReport, Schedule};

use super::models::*;

/// Backdate every `(site, parameter)` slot that has a deployment, re-deriving its readings from the
/// current deployment + calibration timelines. The slot set is recomputed inside the job from
/// `sensor_deployments`, so a rerun always reflects the current deployment topology. Backs the
/// The instant `days` days before now, as a retention prune's cutoff.
pub(super) fn days_ago(days: u32) -> sea_orm::prelude::DateTimeWithTimeZone {
    (chrono::Utc::now() - chrono::Duration::days(i64::from(days))).into()
}

/// What the sweeper writes into a closed row's `errors`, so the sweep and anything reading the
/// reason cannot drift apart.
pub(super) const SWEPT_REASON: &str = "Closed by sweeper: service stopped reporting";

/// A cycle still reporting `running` whose start is older than the threshold. The cut-off is
/// computed here rather than left to the statement, so the window is a typed instant.
pub(super) fn stale_running(cutoff: DateTime<Utc>) -> Condition {
    Condition::all()
        .add(events::Column::Status.eq("running"))
        .add(events::Column::StartedAt.lt(cutoff))
}

/// Close 'running' sync_events older than the staleness threshold; returns the row count.
pub async fn sweep_stale_sync_events(
    db: &sea_orm::DatabaseConnection,
    stale_after_seconds: u64,
) -> Result<u64, DbErr> {
    let cutoff =
        Utc::now() - Duration::seconds(i64::try_from(stale_after_seconds).unwrap_or(i64::MAX));
    let appended = Expr::col(events::Column::Errors)
        .if_null(Expr::val(serde_json::json!([])))
        .binary(
            PgBinOper::Concatenate,
            Expr::val(serde_json::json!([SWEPT_REASON])),
        );
    let res = events::Entity::update_many()
        .col_expr(events::Column::Status, Expr::value("failed"))
        .col_expr(events::Column::CompletedAt, Expr::current_timestamp())
        .col_expr(events::Column::Errors, appended)
        .filter(stale_running(cutoff))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// Close sync_events rows left 'running' past a staleness threshold. A sync service killed
/// mid-cycle (SIGKILL, node loss) can never terminate its own event; without this sweep the
/// row reads as "sync in progress" forever.
pub struct SyncEventSweep {
    interval_seconds: u64,
    stale_after_seconds: u64,
}

impl SyncEventSweep {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            interval_seconds: config.sync_event_sweep_interval_seconds,
            stale_after_seconds: config.sync_event_stale_after_seconds,
        }
    }
}

#[async_trait]
impl Job for SyncEventSweep {
    fn name(&self) -> &'static str {
        "sync_event_sweep"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(
            Ord::max(self.interval_seconds, 1) as i64
        ))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let closed = sweep_stale_sync_events(ctx.db(), self.stale_after_seconds).await?;
        ctx.report(
            JobReport::new()
                .scope("stale_after_seconds", self.stale_after_seconds)
                .count("sync_events_closed", closed),
        )
        .await;
        Ok(closed as i64)
    }
}

/// Age-based retention for the sync ledgers. sync_events accretes one row per cycle and
/// ingest_receipts one per windowed pass; without pruning both grow forever. Running
/// sync_events rows are never touched (the staleness sweep owns those). A receipt is the record
/// of how a stored value arrived, so age alone does not release one: a receipt whose window still
/// covers a stored reading is kept whatever its age, and only receipts nothing resolves to are
/// pruned.
pub struct SyncLedgerRetention {
    sync_event_retention_days: u32,
    ingest_receipt_retention_days: u32,
}

impl SyncLedgerRetention {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let retention = crate::common::retention::Retention::from_config(config);
        Self {
            sync_event_retention_days: retention.sync_events.horizon_days().unwrap_or(0),
            ingest_receipt_retention_days: retention.ingest_receipts.horizon_days().unwrap_or(0),
        }
    }
}

#[async_trait]
impl Job for SyncLedgerRetention {
    fn name(&self) -> &'static str {
        "sync_ledger_retention"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(86_400))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let db = ctx.db();
        let mut events_pruned = 0u64;
        if self.sync_event_retention_days > 0 {
            events_pruned = events::Entity::delete_many()
                .filter(events::Column::Status.ne("running"))
                .filter(events::Column::StartedAt.lt(days_ago(self.sync_event_retention_days)))
                .exec(db)
                .await?
                .rows_affected;
        }
        let mut receipts_pruned = 0u64;
        if self.ingest_receipt_retention_days > 0 {
            // The readings the pass wrote are what a receipt explains, so one whose window still
            // holds rows is kept however old it is.
            use sea_orm::sea_query::ExprTrait as _;
            let explains_a_stored_reading = SeaQuery::select()
                .expr(Expr::value(1))
                .from_as(readings::Entity, Alias::new("r"))
                .and_where(
                    Expr::col((Alias::new("r"), readings::Column::StreamId))
                        .equals((receipts::Entity, receipts::Column::StreamId)),
                )
                .and_where(
                    Expr::col((Alias::new("r"), readings::Column::Time))
                        .gte(Expr::col((receipts::Entity, receipts::Column::WindowFrom))),
                )
                .and_where(
                    Expr::col((Alias::new("r"), readings::Column::Time))
                        .lt(Expr::col((receipts::Entity, receipts::Column::WindowTo))),
                )
                .take();
            receipts_pruned = receipts::Entity::delete_many()
                .filter(receipts::Column::At.lt(days_ago(self.ingest_receipt_retention_days)))
                .filter(Expr::exists(explains_a_stored_reading).not())
                .exec(db)
                .await?
                .rows_affected;
        }
        ctx.report(
            JobReport::new()
                .scope("sync_event_retention_days", self.sync_event_retention_days)
                .scope(
                    "ingest_receipt_retention_days",
                    self.ingest_receipt_retention_days,
                )
                .count("sync_events_pruned", events_pruned)
                .count("ingest_receipts_pruned", receipts_pruned),
        )
        .await;
        Ok((events_pruned + receipts_pruned) as i64)
    }
}

/// Queue a trigger_full_sync for every live, unpaused service with `full_reassert_enabled`. The
/// digest handshake stops a service re-sending unchanged content, which also means routine passes
/// can no longer repair server-side drift (rows changed outside the sync path); the full pass
/// ignores digests and re-asserts everything the source holds.
///
/// What that repairs depends on the source. A reconciled backend declares the window it
/// re-asserts, so its diff applies the corrections. An append-only one (Vaisala, NOMIS) sends no
/// window and the driver ingests with `overwrite` false, so the pass inserts rows missing here and
/// leaves every stored value as it is; correcting those is `resync_streams`, not this.
///
/// Delivery is the normal heartbeat pickup; a service already holding a pending command is not
/// queued twice.
pub struct SyncFullReassert {
    command_expiry_secs: u64,
}

impl SyncFullReassert {
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            command_expiry_secs: config.sync_command_expiry_secs,
        }
    }
}

#[async_trait]
impl Job for SyncFullReassert {
    fn name(&self) -> &'static str {
        "sync_full_reassert"
    }

    fn default_schedule(&self) -> Option<Schedule> {
        Some(Schedule::every_secs(604_800))
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let rows = ctx
            .db()
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "INSERT INTO sync_commands
                     (id, service_id, command, status, created_at, expires_at)
                 SELECT gen_random_uuid(), s.id, 'trigger_full_sync', 'pending', NOW(),
                        NOW() + ($1 || ' seconds')::interval
                 FROM sync_services s
                 WHERE s.paused IS NOT TRUE
                   AND s.full_reassert_enabled
                   AND s.last_heartbeat > NOW() - INTERVAL '1 hour'
                   AND NOT EXISTS (
                       SELECT 1 FROM sync_commands c
                       WHERE c.service_id = s.id
                         AND c.command = 'trigger_full_sync'
                         AND c.status = 'pending'
                         AND c.expires_at > NOW()
                   )
                 RETURNING service_id",
                [self.command_expiry_secs.to_string().into()],
            ))
            .await?;
        let services: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<Uuid>("", "service_id"))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        let queued = services.len();
        ctx.report(
            JobReport::new()
                .scope("service_ids", services)
                .count("commands_queued", queued),
        )
        .await;
        Ok(i64::try_from(queued).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
#[path = "tests/flows.rs"]
mod tests;

/// The re-derivation a pairing plan's apply hands on: every slot the plan paired that holds a
/// reading, re-derived by the deployment and calibration windows. One tracked job under the apply,
/// in `ReprocessAll`'s shape, rather than one job per slot: a failed slot is logged and the run
/// continues, and the panel shows this row's progress instead of thousands of anonymous ones
/// (B418).
pub struct AttributePlanSlots;

#[async_trait]
impl Job for AttributePlanSlots {
    fn name(&self) -> &'static str {
        "plan_attribution"
    }

    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr> {
        let plan_id = crate::routes::private::reprocessing_jobs::flows::required_uuid(
            ctx.params(),
            "plan_id",
        )?;
        let slots: Vec<(Uuid, Uuid)> = super::service::plan_slots_holding_readings(plan_id)
            .into_tuple()
            .all(ctx.db())
            .await?;
        let total = i32::try_from(slots.len()).unwrap_or(i32::MAX);
        ctx.info(&format!("Attributing {} slot(s)", slots.len())).await;

        let mut results = Vec::with_capacity(slots.len());
        for (done, (site_id, parameter_id)) in slots.into_iter().enumerate() {
            let moved = crate::routes::private::sensor_calibrations::service::reprocess_site_parameter_readings(
                ctx.db(),
                site_id,
                parameter_id,
                Some(ctx.job_id()),
            )
            .await
            .map(|n| n as i64);
            results.push((
                serde_json::json!({ "site_id": site_id, "parameter_id": parameter_id }),
                moved,
            ));
            ctx.set_progress(i32::try_from(done + 1).unwrap_or(i32::MAX), Some(total))
                .await;
        }
        let outcome = crate::routes::private::reprocessing_jobs::flows::SlotOutcome::from(results);
        let readings_updated = outcome.readings;
        let report = outcome
            .record(
                &ctx,
                JobReport::new()
                    .scope("plan_id", plan_id.to_string())
                    .count("slots", usize::try_from(total).unwrap_or(0))
                    .count("readings_updated", readings_updated),
            )
            .await;
        ctx.report(report).await;
        if outcome.all_failed() {
            return Err(outcome.error());
        }
        Ok(readings_updated)
    }
}
