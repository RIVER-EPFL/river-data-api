//! Single source of truth for per-`trigger_type` metadata: the `category` used for UI grouping and
//! retention tiering, and the `rerunnable`/`cancellable` policies. A job row carries what these say
//! about it, so a client renders its buttons from the server's answer rather than from a copy.

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
    if MAINTENANCE.contains(&trigger_type) {
        CATEGORY_MAINTENANCE
    } else if METADATA.contains(&trigger_type) {
        CATEGORY_METADATA
    } else {
        CATEGORY_OPERATOR
    }
}

/// Routine automatic plumbing. Ordered as registered, not alphabetically.
pub const MAINTENANCE: &[&str] = &[
    "janitor_service",
    "alarm_sweep",
    "sync_event_sweep",
    "sync_ledger_retention",
    "sync_full_reassert",
    "identity_reconcile",
    "notify_health",
    "dispatch_notifications",
    "ingest_derived",
    "batch_derived",
    "refresh_aggregates",
    "alarm_backfill",
    "meteoswiss_sync",
];

/// Triggered by a configuration change rather than by a person.
pub const METADATA: &[&str] = &[
    "calibration_create",
    "calibration_update",
    "calibration_delete",
    "calibration_retire",
    "calibration_unretire",
    "deployment_create",
    "deployment_update",
    "deployment_delete",
    "derived_assignment",
];

/// Whether a finished job of this `trigger_type` can be re-run by replaying the `params` stored on
/// its row. Keyed on trigger_type, not status, `failed`/`completed`/`cancelled` jobs are all
/// rerunnable if the type is.
///
/// Excluded: `csv_import`, whose staged source rows are deleted on completion; the timestamp-driven
/// derived jobs (`ingest_derived`/`batch_derived`/`compute_derived`), whose faithful replay needs
/// their persisted timestamps and whose values the janitor backfills anyway; and the merges and
/// plan jobs, which are guarded no-ops on a second run rather than a replay worth offering.
#[must_use]
pub fn is_rerunnable(trigger_type: &str) -> bool {
    RERUNNABLE.contains(&trigger_type)
}

pub const RERUNNABLE: &[&str] = &[
    "manual_reprocess",
    "calibration_create",
    "calibration_update",
    "calibration_delete",
    "calibration_retire",
    "calibration_unretire",
    "calibration_recalculate",
    "deployment_create",
    "deployment_update",
    "deployment_delete",
    "manual_adopt",
    "attribution_pin",
    "sensor_swap",
    "refresh_aggregates",
    "derived_recompute",
    "measurement_retag",
    "sd_estimator_retag",
    "event_recompute",
    "event_audit",
    "reprocess_all",
    "pairing_backfill",
];

/// Whether a running job of this `trigger_type` can be cooperatively cancelled, i.e. it iterates a
/// loop and checks `JobContext::is_cancelled` at its batch checkpoints. Single-statement jobs
/// (aggregate refresh, pairing backfill) have no checkpoint and report 409 on a cancel attempt.
#[must_use]
pub fn is_cancellable(trigger_type: &str) -> bool {
    CANCELLABLE.contains(&trigger_type)
}

pub const CANCELLABLE: &[&str] = &[
    "ingest_derived",
    "batch_derived",
    "derived_recompute",
    "csv_import",
    "janitor_service",
    "replicate_reconciliation",
    "replicate_reconciliation_delete",
    "event_audit",
    "event_recompute",
    "meteoswiss_sync",
];

/// The recurring services [`super::job::register_scheduled_services`] adds. They carry a cadence
/// from `Config`, so a unit test cannot build them; `register_scheduled_services` asserts at
/// startup that it registered exactly these, which is what keeps the list honest.
pub const SCHEDULED_SERVICE_NAMES: &[&str] = &[
    "janitor_service",
    "alarm_sweep",
    "sync_event_sweep",
    "sync_ledger_retention",
    "sync_full_reassert",
    "identity_reconcile",
    "notify_health",
    "dispatch_notifications",
    "meteoswiss_sync",
];
