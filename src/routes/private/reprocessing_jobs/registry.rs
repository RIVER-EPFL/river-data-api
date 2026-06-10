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
        | "ingest_derived"
        | "batch_derived"
        | "refresh_aggregates"
        | "refresh_aggregates_full"
        | "alarm_backfill" => CATEGORY_MAINTENANCE,
        "calibration_create"
        | "calibration_update"
        | "calibration_delete"
        | "deployment_create"
        | "deployment_update"
        | "deployment_delete"
        | "deployment_edit"
        | "derived_assignment" => CATEGORY_METADATA,
        _ => CATEGORY_OPERATOR,
    }
}
