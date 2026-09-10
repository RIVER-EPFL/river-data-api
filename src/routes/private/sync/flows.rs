//! Sync orchestration: the reconciliation enqueue and the jobs that drive the ledger sweeps.

use async_trait::async_trait;
use axum::Json;
use sea_orm::{ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter};
use uuid::Uuid;

use crate::common::AppState;
use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::routes::private::reprocessing_jobs;
use crate::routes::private::reprocessing_jobs::job::Job;
use crate::routes::private::reprocessing_jobs::lifecycle::{JobContext, JobReport};
use crate::routes::private::reprocessing_jobs::schedule::Schedule;
use crate::routes::private::reprocessing_jobs::worker;

use super::models::*;

/// Backdate every `(site, parameter)` slot that has a deployment, re-deriving its readings from the
/// current deployment + calibration timelines. The slot set is recomputed inside the job from
/// `sensor_deployments`, so a rerun always reflects the current deployment topology. Backs the
/// The instant `days` days before now, as a retention prune's cutoff.
pub(super) fn days_ago(days: u32) -> sea_orm::prelude::DateTimeWithTimeZone {
    (chrono::Utc::now() - chrono::Duration::days(i64::from(days))).into()
}

/// Close 'running' sync_events older than the staleness threshold; returns the row count.
pub async fn sweep_stale_sync_events(
    db: &sea_orm::DatabaseConnection,
    stale_after_seconds: u64,
) -> Result<u64, DbErr> {
    let res = db
        .execute_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE sync_events
             SET status = 'failed',
                 completed_at = NOW(),
                 errors = COALESCE(errors, '[]'::jsonb) || '[\"Closed by sweeper: service stopped reporting\"]'::jsonb
             WHERE status = 'running' AND started_at < NOW() - ($1 || ' seconds')::interval",
            [stale_after_seconds.to_string().into()],
        ))
        .await?;
    Ok(res.rows_affected())
}

pub(super) async fn enqueue_reconciliation(
    state: &AppState,
    trigger_type: &str,
    payload: &StartReconciliationRequest,
) -> AppResult<Json<StartReconciliationResponse>> {
    if payload.source_system.trim().is_empty() {
        return Err(AppError::BadRequest(
            "source_system is required".to_string(),
        ));
    }
    // One live run per (job kind, source): a second concurrent migration over the same streams
    // would race the per-family claims for no benefit.
    // The live set for one job kind is a handful of rows, so the `params` match is made here
    // rather than as a jsonb predicate the typed column API cannot express.
    let active = reprocessing_jobs::model::Entity::find()
        .filter(reprocessing_jobs::model::Column::TriggerType.eq(trigger_type))
        .filter(reprocessing_jobs::model::Column::Status.is_in(["queued", "running", "retrying"]))
        .all(&state.db)
        .await?
        .into_iter()
        .find(|job| {
            job.params
                .get("source_system")
                .and_then(serde_json::Value::as_str)
                == Some(payload.source_system.as_str())
        })
        .map(|job| job.id);
    if let Some(id) = active {
        return Err(AppError::Conflict(format!(
            "{trigger_type} already running for {} (job {id})",
            payload.source_system
        )));
    }

    let job_id = worker::enqueue(
        &state.db,
        trigger_type,
        None,
        None,
        &serde_json::json!({
            "source_system": payload.source_system,
            "dry_run": payload.dry_run,
            "tolerance": payload.tolerance,
        }),
        None,
    )
    .await?
    .ok_or_else(|| AppError::Internal("job enqueue inserted nothing".to_string()))?;
    Ok(Json(StartReconciliationResponse { job_id }))
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
        Some(Schedule::every_secs(self.interval_seconds.max(1) as i64))
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
            receipts_pruned = db
                .execute_raw(sea_orm::Statement::from_sql_and_values(
                    sea_orm::DatabaseBackend::Postgres,
                    "DELETE FROM ingest_receipts
                     WHERE at < NOW() - ($1 || ' days')::interval
                       AND NOT EXISTS (
                             SELECT 1 FROM readings r
                              WHERE r.stream_id = ingest_receipts.stream_id
                                AND r.time >= ingest_receipts.window_from
                                AND r.time < ingest_receipts.window_to)",
                    [self.ingest_receipt_retention_days.to_string().into()],
                ))
                .await?
                .rows_affected();
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
