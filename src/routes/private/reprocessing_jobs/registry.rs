//! Single source of truth for per-`trigger_type` metadata. Today: the `category` used for UI
//! grouping/filtering and retention tiering. Phases 3-4 will add `rerunnable`/`cancellable` here so
//! the policy for every job type lives in one place.

/// Job categories. `operator` = a person (or an operator action) triggered it; `metadata` = a
/// config change (calibration/deployment/derived assignment) triggered it; `maintenance` = routine
/// automatic plumbing (janitor, ingest-time derived, aggregate refresh, alarm backfill).
pub const CATEGORY_OPERATOR: &str = "operator";
pub const CATEGORY_METADATA: &str = "metadata";
pub const CATEGORY_MAINTENANCE: &str = "maintenance";

/// Classify a job by its `trigger_type`. Unknown types default to `operator` (always visible) so a
/// newly added job type is never silently hidden behind the maintenance filter.
#[must_use]
pub fn category_for(trigger_type: &str) -> &'static str {
    match trigger_type {
        "janitor_run"
        | "janitor_service"
        | "alarm_sweep"
        | "sync_event_sweep"
        | "identity_reconcile"
        | "notify_health"
        | "dispatch_notifications"
        | "ingest_derived"
        | "batch_derived"
        | "refresh_aggregates"
        | "refresh_aggregates_full"
        | "alarm_backfill" => CATEGORY_MAINTENANCE,
        "calibration_create" | "calibration_update" | "calibration_delete"
        | "deployment_create" | "deployment_update" | "deployment_delete" | "deployment_edit"
        | "derived_assignment" => CATEGORY_METADATA,
        _ => CATEGORY_OPERATOR,
    }
}

/// Whether a finished job of this `trigger_type` can be re-run by replaying it from the ids stored
/// on its row (sensor reprocess, aggregate refresh, derived recompute). Keyed on trigger_type, not
/// status, `failed`/`completed`/`cancelled` jobs are all rerunnable if the type is.
///
/// Excluded for now: the timestamp-driven derived jobs (`ingest_derived`/`batch_derived`/
/// `compute_derived`), faithful replay needs their persisted timestamps (a follow-up); the janitor
/// already backfills any missed derived values. `csv_import` can never replay (source expires).
#[must_use]
pub fn is_rerunnable(trigger_type: &str) -> bool {
    matches!(
        trigger_type,
        "manual_reprocess"
            | "calibration_create"
            | "calibration_update"
            | "calibration_delete"
            | "calibration_recalculate"
            | "deployment_create"
            | "deployment_update"
            | "deployment_delete"
            | "deployment_edit"
            | "manual_adopt"
            | "sensor_swap"
            | "refresh_aggregates"
            | "refresh_aggregates_full"
            | "derived_recompute"
            | "measurement_retag"
    )
}

/// Whether a running job of this `trigger_type` can be cooperatively cancelled, i.e. it iterates a
/// loop and checks `JobContext::is_cancelled` at its batch checkpoints. Single-statement jobs
/// (aggregate refresh, pairing backfill) have no checkpoint and report 409 on a cancel attempt.
#[must_use]
pub fn is_cancellable(trigger_type: &str) -> bool {
    matches!(
        trigger_type,
        "ingest_derived"
            | "batch_derived"
            | "derived_recompute"
            | "csv_import"
            | "janitor_run"
            | "replicate_reconciliation"
            | "replicate_reconciliation_delete"
            | "replicate_reindex_repair"
    )
}
