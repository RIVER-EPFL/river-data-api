//! The tracked-job machinery: the [`Job`] contract and its registry, the [`JobContext`] a claimed
//! run is handed, the cadence types the scheduler applies, the claim-based worker pool, and the
//! `CRUDOperations` for a job row and a schedule row.
//!
//! Every concrete job lives with the component whose tables it moves; what runs them is here.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crudcrate::{ApiError, CRUDOperations, CRUDResource};
use futures::FutureExt;
use sea_orm::sea_query::{Expr, ExprTrait as _, LockBehavior, LockType, OnConflict};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, FromQueryResult,
    QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait, TryInsertResult,
};
use uuid::Uuid;

use super::models::job::{ActiveModel, Column, Entity, ReprocessingJob};
use super::models::schedule;

// --- Job policy tables ---
//
// Single source of truth for per-`trigger_type` metadata: the `category` used for UI grouping and
// retention tiering, and the `rerunnable`/`cancellable` policies. A job row carries what these say
// about it, so a client renders its buttons from the server's answer rather than from a copy.

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
    "meteoswiss_recent",
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

/// What one kind needs before a person can run it off-cadence.
///
/// Three answers, and every kind gives one (I94): a kind reached only from its own action route
/// is `NotOffered` and is not listed; one whose `run` reads nothing from `params` is a button;
/// one that reads a target or a window declares each input, and both the refusal and the form are
/// built from that declaration. There is no free JSON field anywhere (Evan, Q165).
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "offer")]
pub enum ManualRun {
    /// Its inputs are not a person's to supply: staged rows it consumes, or a replay of persisted
    /// timestamps. Reached from its own route.
    NotOffered,
    /// Runs as it stands.
    NoParameters,
    /// Runs once these are given.
    Declared { params: Vec<ParamSpec> },
}

/// One input a manual run supplies, as the form renders it and the refusal names it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct ParamSpec {
    pub name: &'static str,
    pub kind: ParamKind,
    pub required: bool,
    /// What the control is labelled, and what the 400 calls the input.
    pub label: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// One row of the entity the label names.
    Uuid,
    /// An RFC 3339 instant.
    Instant,
    Text,
    Bool,
    Number,
    /// An array of UUIDs; an empty one is "every one of them".
    UuidList,
    /// An array of `[site_id, parameter_id]` pairs, or of `{site_id, timestamps}` objects.
    PairList,
}

const fn spec(
    name: &'static str,
    kind: ParamKind,
    required: bool,
    label: &'static str,
) -> ParamSpec {
    ParamSpec {
        name,
        kind,
        required,
        label,
    }
}

/// One kind a person may run off-cadence, as the page lists it: what it needs, and the cadence it
/// also runs on where it has one.
#[derive(Debug, Clone, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub struct RunnableJob {
    pub job_name: String,
    pub manual_run: ManualRun,
    /// Absent where the kind has no schedule at all, which the page reads as on demand.
    pub interval_seconds: Option<i64>,
    /// Whether the cadence is switched on, where it has one.
    pub enabled: Option<bool>,
}

/// The kinds a person may run, by name. A `NotOffered` kind is left out rather than listed and
/// refused: its inputs come from the route that enqueues it, so there is no form to offer.
pub fn runnable_jobs<'a>(
    names: impl Iterator<Item = &'a str>,
    cadence: &HashMap<String, (Option<i64>, bool)>,
) -> Vec<RunnableJob> {
    let mut runnable: Vec<RunnableJob> = names
        .filter_map(|name| {
            let manual_run = manual_run_for(name);
            if manual_run == ManualRun::NotOffered {
                return None;
            }
            let scheduled = cadence.get(name);
            Some(RunnableJob {
                job_name: name.to_string(),
                manual_run,
                interval_seconds: scheduled.and_then(|(interval, _)| *interval),
                enabled: scheduled.map(|(_, enabled)| *enabled),
            })
        })
        .collect();
    runnable.sort_by(|a, b| a.job_name.cmp(&b.job_name));
    runnable
}

/// What a manual run of `trigger_type` needs. One table, so the route's refusal, the page's form
/// and this answer cannot disagree; the kinds' own `required_uuid` guards stay the last defence.
#[must_use]
pub fn manual_run_for(trigger_type: &str) -> ManualRun {
    use ParamKind::{Bool, Instant, Number, PairList, Text, Uuid, UuidList};
    let declared = |params: Vec<ParamSpec>| ManualRun::Declared { params };
    match trigger_type {
        // Reads nothing but the scheduler's own snapshot.
        "janitor_service"
        | "alarm_sweep"
        | "sync_event_sweep"
        | "sync_ledger_retention"
        | "identity_reconcile"
        | "notify_health"
        | "dispatch_notifications"
        | "meteoswiss_sync"
        | "reprocess_all"
        | "event_audit" => ManualRun::NoParameters,

        // Each of these reads its scope from `params`, so a run minted with none either fails at
        // the first missing key or moves nothing and reports zero (B262). What they read is what
        // they declare.
        "derived_recompute" => declared(vec![
            spec("derived_definition_id", Uuid, false, "Calculation"),
            spec("site_ids", UuidList, false, "Sites"),
            spec("parameter_ids", UuidList, false, "Parameters"),
            spec("start", Instant, false, "Window start"),
            spec("end", Instant, false, "Window end"),
        ]),
        "backfill_calibrations" => declared(vec![spec("sensors", UuidList, true, "Instruments")]),
        "backfill_attribution" => declared(vec![spec("slots", PairList, true, "Slots")]),

        "refresh_aggregates" => declared(vec![
            spec("from", Instant, false, "Window start"),
            spec("until", Instant, false, "Window end"),
            spec("since", Instant, false, "Everything after"),
        ]),
        "manual_reprocess"
        | "calibration_create"
        | "calibration_update"
        | "calibration_delete"
        | "calibration_retire"
        | "calibration_unretire"
        | "calibration_recalculate" => declared(vec![spec("sensor_id", Uuid, true, "Instrument")]),
        "sensor_swap" | "pairing_backfill" | "manual_adopt" | "attribution_pin" => declared(vec![
            spec("site_id", Uuid, true, "Site"),
            spec("parameter_id", Uuid, true, "Parameter"),
            spec("sensor_id", Uuid, false, "Instrument"),
        ]),
        "deployment_create" | "deployment_update" | "deployment_delete" => declared(vec![
            spec("sensor_id", Uuid, true, "Instrument"),
            spec("site_id", Uuid, true, "Site"),
            spec("parameter_id", Uuid, false, "Parameter"),
        ]),
        "derived_assignment" => declared(vec![
            spec("derived_definition_id", Uuid, true, "Calculation"),
            spec("site_id", Uuid, true, "Site"),
        ]),
        "merge_parameters" => declared(vec![
            spec("source_parameter_id", Uuid, true, "Absorbed parameter"),
            spec("target_parameter_id", Uuid, true, "Surviving parameter"),
        ]),
        "merge_site_parameters" => declared(vec![
            spec("source_site_parameter_id", Uuid, true, "Absorbed slot"),
            spec("target_site_parameter_id", Uuid, true, "Surviving slot"),
        ]),
        "plan_apply" | "plan_revert" => declared(vec![spec("plan_id", Uuid, true, "Pairing plan")]),
        "alarm_backfill" => declared(vec![
            spec("slots", PairList, false, "Slots"),
            spec("start", Instant, false, "Window start"),
            spec("end", Instant, false, "Window end"),
        ]),
        "replicate_reconciliation" | "replicate_reconciliation_delete" | "sync_full_reassert" => {
            declared(vec![
                spec("source_system", Text, true, "Source system"),
                spec("tolerance", Number, false, "Relative tolerance"),
                spec("dry_run", Bool, false, "Report only"),
            ])
        }
        "measurement_retag" => declared(vec![
            spec("target", Text, true, "Measurement type"),
            spec("sensor_ids", UuidList, false, "Instruments"),
            spec("stream_ids", UuidList, false, "Streams"),
            spec("source_system", Text, false, "Source system"),
        ]),
        "sd_estimator_retag" => declared(vec![
            spec("estimator", Text, true, "Estimator"),
            spec("site_parameter_ids", UuidList, false, "Slots"),
            spec("stream_ids", UuidList, false, "Streams"),
            spec("start", Instant, false, "Window start"),
            spec("end", Instant, false, "Window end"),
            spec(
                "override_instants",
                Bool,
                false,
                "Override declared instants",
            ),
        ]),

        // Its inputs are not a person's: staged rows the run deletes, or persisted timestamps.
        _ => ManualRun::NotOffered,
    }
}

/// The inputs a declared kind is missing from what a caller supplied, in declaration order.
#[must_use]
pub fn missing_params(offer: &ManualRun, supplied: &serde_json::Value) -> Vec<&'static str> {
    let ManualRun::Declared { params } = offer else {
        return Vec::new();
    };
    params
        .iter()
        .filter(|p| p.required && supplied.get(p.name).is_none_or(serde_json::Value::is_null))
        .map(|p| p.label)
        .collect()
}

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
    "meteoswiss_recent",
];

/// The recurring services [`register_scheduled_services`] adds. They carry a cadence
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
    "meteoswiss_recent",
];

// --- The Job contract and its registry ---
//
// The uniform `Job` abstraction (ADR 0001).
//
// Every tracked-job kind implements [`Job`], and a name-keyed [`JobRegistry`] lets the worker pool
// and the scheduler dispatch a claimed row (or a due schedule) to its handler by `trigger_type`,
// so first-run and rerun share one code path. Implementations are **stateless handlers**: every
// per-run input arrives via the job row's `params` (read through [`JobContext`]), so any replica
// can run any job after claiming it.
//
// This is the migration-independent contract. The worker claim loop, the enqueue-flip, and reading
// persisted `params` land with the worker-pool migration and build on this trait.

/// A kind of tracked job, keyed by its stable [`name`](Job::name) (the `trigger_type` written to
/// `reprocessing_jobs` and referenced by `schedules.job_name`).
#[async_trait]
pub trait Job: Send + Sync {
    /// Stable identifier, must equal the `trigger_type` persisted on the row and any
    /// `schedules.job_name` that enqueues this job.
    fn name(&self) -> &'static str;

    /// UI/retention classification. Delegates to the shared [`category_for`] mapping so
    /// the trait and the string table can never disagree.
    fn category(&self) -> &'static str {
        category_for(self.name())
    }

    /// Whether a finished job of this kind can be re-run by replaying its persisted inputs.
    fn rerunnable(&self) -> bool {
        is_rerunnable(self.name())
    }

    /// Whether a running job of this kind observes [`JobContext::is_cancelled`] at batch
    /// checkpoints (single-statement jobs report 409 on a cancel attempt).
    fn cancellable(&self) -> bool {
        is_cancellable(self.name())
    }

    /// The default cadence when this kind is a recurring Service. `None` (the common case) means
    /// on-demand only; recurring services (janitor, alarm sweeper, …) return `Some(_)` and the
    /// scheduler seeds a `schedules` row from it on first start.
    fn default_schedule(&self) -> Option<Schedule> {
        None
    }

    /// What a manual off-cadence run of this kind needs. Delegates to the shared
    /// [`manual_run_for`] table so the trait and the route's refusal cannot disagree.
    fn manual_run(&self) -> ManualRun {
        manual_run_for(self.name())
    }

    /// The operator-settable inputs this job reads. The default is none, which is what makes the
    /// default [`Job::validate`] refuse every key.
    fn tunables(&self) -> Vec<TunableSpec> {
        Vec::new()
    }

    /// Validate operator-supplied tunables before they are persisted onto a schedule or job. A job
    /// that declares no tunables reads none, so the default refuses every key rather than saving a
    /// misspelling nothing will ever act on.
    fn validate(&self, tunables: &serde_json::Value) -> Result<(), String> {
        validate_against_specs(tunables, &self.tunables())
    }

    /// Execute one run. Inputs come from the job row via `ctx`; the returned count is recorded as
    /// `readings_updated` on completion.
    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr>;
}

/// What one operator-settable input a job accepts looks like: enough for a form to build a field
/// and for the server to refuse a value the job would not read.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TunableSpec {
    pub key: String,
    pub kind: TunableKind,
    /// Inclusive bounds for an integer or a duration in seconds.
    #[schema(required)]
    pub min: Option<i64>,
    #[schema(required)]
    pub max: Option<i64>,
    pub default: serde_json::Value,
    pub help: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TunableKind {
    Integer,
    Boolean,
    /// A count of seconds, rendered as a duration.
    Duration,
    Enum {
        options: Vec<String>,
    },
}

/// Whether a value satisfies its spec. The message names the key and what it takes.
fn check_tunable(spec: &TunableSpec, value: &serde_json::Value) -> Result<(), String> {
    let key = &spec.key;
    match &spec.kind {
        TunableKind::Integer | TunableKind::Duration => {
            let Some(n) = value.as_i64() else {
                return Err(format!("{key} must be an integer"));
            };
            if spec.min.is_some_and(|min| n < min) || spec.max.is_some_and(|max| n > max) {
                return Err(match (spec.min, spec.max) {
                    (Some(min), Some(max)) => format!("{key} must be between {min} and {max}"),
                    (Some(min), None) => format!("{key} must be at least {min}"),
                    (None, Some(max)) => format!("{key} must be at most {max}"),
                    (None, None) => unreachable!("a bound was exceeded, so one is set"),
                });
            }
            Ok(())
        }
        TunableKind::Boolean => value
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| format!("{key} must be true or false")),
        TunableKind::Enum { options } => {
            let Some(s) = value.as_str() else {
                return Err(format!("{key} must be one of: {}", options.join(", ")));
            };
            if options.iter().any(|option| option == s) {
                Ok(())
            } else {
                Err(format!("{key} must be one of: {}", options.join(", ")))
            }
        }
    }
}

/// Validate an operator's tunables object against a job's declared specs: unknown keys are refused
/// naming them, and every value present must satisfy its spec. A null or empty object is no
/// tunables at all, which every job accepts.
pub fn validate_against_specs(
    tunables: &serde_json::Value,
    specs: &[TunableSpec],
) -> Result<(), String> {
    let known: Vec<&str> = specs.iter().map(|s| s.key.as_str()).collect();
    reject_unknown_tunables(tunables, &known)?;
    let Some(obj) = tunables.as_object() else {
        return Ok(());
    };
    for spec in specs {
        if let Some(value) = obj.get(&spec.key) {
            check_tunable(spec, value)?;
        }
    }
    Ok(())
}

/// A job accepts exactly the keys it reads. Anything else is a misspelling that would be saved,
/// audited and never acted on, so it is refused naming both what arrived and what is accepted.
///
/// A null or empty object is no tunables at all, which every job accepts.
pub fn reject_unknown_tunables(tunables: &serde_json::Value, known: &[&str]) -> Result<(), String> {
    if tunables.is_null() {
        return Ok(());
    }
    let Some(obj) = tunables.as_object() else {
        return Err("tunables must be a JSON object".to_string());
    };
    let unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    if known.is_empty() {
        Err(format!(
            "this job takes no tunables, but got: {}",
            unknown.join(", ")
        ))
    } else {
        Err(format!(
            "unknown tunable(s): {}; this job takes: {}",
            unknown.join(", "),
            known.join(", ")
        ))
    }
}

/// Name-keyed set of all known job kinds, built once at startup and shared read-only with the
/// worker pool and scheduler. Lookups are by `trigger_type`.
#[derive(Default, Clone)]
pub struct JobRegistry {
    jobs: HashMap<&'static str, Arc<dyn Job>>,
}

impl JobRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
        }
    }

    /// Register a job kind. Panics on a duplicate `name()`, a programming error caught at startup
    /// rather than a silent dispatch ambiguity at runtime.
    pub fn register(&mut self, job: Arc<dyn Job>) {
        let name = job.name();
        assert!(
            self.jobs.insert(name, job).is_none(),
            "duplicate Job registration for trigger_type {name:?}"
        );
    }

    /// The handler for a `trigger_type`, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Job>> {
        self.jobs.get(name).cloned()
    }

    /// Every registered job that defines a default schedule (the recurring Services), for seeding
    /// `schedules` rows at startup.
    pub fn default_schedules(&self) -> impl Iterator<Item = (&'static str, Schedule)> + '_ {
        self.jobs
            .values()
            .filter_map(|job| job.default_schedule().map(|sched| (job.name(), sched)))
    }

    /// Every registered `trigger_type`. The policy tables in [`registry`] are keyed by these, so a
    /// name only they know is a name nothing answers to.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.jobs.keys().copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

/// Build the registry of every on-demand worker-run job kind. Called once at startup. The
/// recurring Services (which carry a
/// `default_schedule` derived from `Config`) are added separately via
/// [`register_scheduled_services`] so this stays zero-arg for the worker-pool tests.
#[must_use]
pub fn build_registry() -> JobRegistry {
    let mut registry = JobRegistry::new();
    for trigger in [
        "manual_reprocess",
        "calibration_create",
        "calibration_update",
        "calibration_delete",
        "calibration_retire",
        "calibration_unretire",
        "calibration_recalculate",
    ] {
        registry.register(Arc::new(
            crate::routes::private::sensors::flows::ReprocessSensor::new(trigger),
        ));
    }
    registry.register(Arc::new(super::flows::RefreshAggregates));
    for trigger in [
        "sensor_swap",
        "pairing_backfill",
        "manual_adopt",
        "attribution_pin",
    ] {
        registry.register(Arc::new(
            crate::routes::private::sensors::flows::ReprocessSlot::new(trigger),
        ));
    }
    for trigger in [
        "deployment_create",
        "deployment_update",
        "deployment_delete",
    ] {
        registry.register(Arc::new(
            crate::routes::private::sensor_deployments::flows::ReprocessDeployment::new(trigger),
        ));
    }
    registry.register(Arc::new(
        crate::routes::private::derived_parameters::flows::DerivedRecompute,
    ));
    registry.register(Arc::new(
        crate::routes::private::derived_parameters::flows::DerivedAssignment,
    ));
    registry.register(Arc::new(
        crate::routes::private::derived_parameters::flows::IngestDerived,
    ));
    for trigger in ["compute_derived", "batch_derived"] {
        registry.register(Arc::new(
            crate::routes::private::derived_parameters::flows::SiteTimestampsDerived::new(trigger),
        ));
    }
    registry.register(Arc::new(
        crate::routes::private::sensors::flows::ReprocessAll,
    ));
    registry.register(Arc::new(
        crate::routes::private::readings::flows::MeasurementRetag,
    ));
    registry.register(Arc::new(
        crate::routes::private::site_parameters::flows::SdEstimatorRetag,
    ));
    registry.register(Arc::new(crate::routes::private::readings::flows::CsvImport));
    registry.register(Arc::new(
        crate::routes::private::alarms::flows::AlarmBackfill,
    ));
    registry.register(Arc::new(
        crate::routes::private::sensor_deployments::flows::BackfillAttribution,
    ));
    registry.register(Arc::new(
        crate::routes::private::sensors::flows::BackfillCalibrations,
    ));
    registry.register(Arc::new(
        crate::routes::private::site_parameters::flows::MergeSiteParameters,
    ));
    registry.register(Arc::new(
        crate::routes::private::parameters::flows::MergeParameters,
    ));
    registry.register(Arc::new(
        crate::routes::private::data_streams::flows::PlanApply,
    ));
    registry.register(Arc::new(
        crate::routes::private::data_streams::flows::PlanRevert,
    ));
    registry.register(Arc::new(
        crate::routes::private::sync::flows::ReplicateReconciliation,
    ));
    registry.register(Arc::new(
        crate::routes::private::sync::flows::ReplicateReconciliationDelete,
    ));
    registry.register(Arc::new(
        crate::routes::private::tools::models::EventRecompute,
    ));
    registry.register(Arc::new(crate::routes::private::tools::models::EventAudit));
    registry
}

/// Register the recurring Services (the former `main.rs` background loops) onto an existing registry,
/// each carrying its cadence from `Config`. Kept separate from [`build_registry`] so the cadence
/// dependency lives only on the `main.rs` startup path; the scheduler then seeds `schedules` rows
/// from `registry.default_schedules()`.
pub fn register_scheduled_services(registry: &mut JobRegistry, config: &crate::config::Config) {
    registry.register(Arc::new(super::flows::JanitorRun::from_config(config)));
    registry.register(Arc::new(
        crate::routes::private::alarms::flows::AlarmSweep::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::sync::flows::SyncEventSweep::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::sync::flows::SyncLedgerRetention::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::sync::flows::SyncFullReassert::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::notifications::flows::PushSubscriptionReconcile::from_config(
            config,
        ),
    ));
    registry.register(Arc::new(
        crate::routes::private::notifications::flows::NotifyHealth::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::notifications::flows::DispatchNotifications::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::meteoswiss::flows::MeteoswissSync::from_config(config),
    ));
    registry.register(Arc::new(
        crate::routes::private::meteoswiss::flows::MeteoswissRecent::from_config(config),
    ));

    // The policy tables in `registry` are keyed by trigger_type and cannot construct these, so the
    // name list they check against is verified here instead of drifting quietly.
    for name in SCHEDULED_SERVICE_NAMES {
        assert!(
            registry.get(name).is_some(),
            "registry::SCHEDULED_SERVICE_NAMES lists {name:?}, which no service registered under"
        );
    }
}

// --- What a claimed job runs with ---
//
// What a claimed job runs with: the [`JobContext`] handed to `Job::run` (progress, structured
// `detail`, the timeline in `reprocessing_job_logs`, the cancel flag) and the process-wide
// [`RetryPolicy`] the worker pool reschedules a failed run under. Rows are created by
// `worker::enqueue` and driven by the worker pool; nothing here spawns work.

/// Retry policy the worker pool reschedules a failed run under. Set once at startup from `Config`;
/// code paths that don't run `main.rs` (integration tests) see the default, no retries.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub backoff_base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            backoff_base: Duration::from_secs(60),
        }
    }
}

static JOB_RETRY_POLICY: OnceLock<RetryPolicy> = OnceLock::new();

/// Initialise the global tracked-job retry policy (call once at startup, from `main.rs`).
pub fn set_job_retry_policy(policy: RetryPolicy) {
    let _ = JOB_RETRY_POLICY.set(policy);
}

pub(crate) fn job_retry_policy() -> RetryPolicy {
    JOB_RETRY_POLICY.get().copied().unwrap_or_default()
}

/// Handle passed to `Job::run`. Owns a DB connection, the job id, and the event sender so work can
/// report incremental progress, structured `detail`, and a timeline of log lines that the UI sees
/// live. Cheap to clone; the log `seq` counter is shared across clones so ordering stays monotonic.
#[derive(Clone)]
pub struct JobContext {
    db: DatabaseConnection,
    job_id: Uuid,
    events: crate::common::EventSender,
    seq: Arc<AtomicI64>,
    cancel: Arc<AtomicBool>,
    params: serde_json::Value,
}

impl JobContext {
    /// Build a context for a job claimed by the worker pool. Returns the context plus the in-process
    /// cancel flag the worker's heartbeat flips when it sees `cancel_requested` on the row (or when
    /// the lease is lost), so a cooperatively-cancellable job stops at its next checkpoint.
    pub(crate) fn for_worker(
        db: DatabaseConnection,
        job_id: Uuid,
        events: crate::common::EventSender,
        params: serde_json::Value,
    ) -> (Self, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = Self {
            db,
            job_id,
            events,
            seq: Arc::new(AtomicI64::new(0)),
            cancel: cancel.clone(),
            params,
        };
        (ctx, cancel)
    }

    /// The job's persisted inputs, what a worker-run job reads to do its work.
    #[must_use]
    pub fn params(&self) -> &serde_json::Value {
        &self.params
    }

    /// The DB connection the job should use.
    #[must_use]
    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    /// The SSE event sender, for jobs that emit domain events (e.g. `DataIngested`) beyond the
    /// lifecycle's own progress/completion events.
    #[must_use]
    pub fn events(&self) -> &crate::common::EventSender {
        &self.events
    }

    /// This job's id.
    #[must_use]
    pub fn job_id(&self) -> Uuid {
        self.job_id
    }

    /// Whether cancellation has been requested. Loop-based work checks this at its batch
    /// checkpoints and returns early; the lifecycle then records the job as `cancelled`.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Append a line to the job's timeline (`reprocessing_job_logs`). `warn`/`error` lines are also
    /// streamed over SSE as `JobLog`; the full ordered timeline is fetched on demand from
    /// `GET /api/jobs/{id}/logs`. Best-effort, a logging failure must never fail the job.
    pub async fn log(&self, level: &str, message: &str, context: serde_json::Value) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let line = super::models::job_log::ActiveModel {
            job_id: Set(self.job_id),
            seq: Set(seq),
            level: Set(level.to_string()),
            message: Set(message.to_string()),
            context: Set(context.clone()),
            ..Default::default()
        };
        if let Err(e) = super::models::job_log::Entity::insert(line)
            .exec(&self.db)
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to append job log line");
        }
        if level == "warn" || level == "error" {
            let _ = self.events.send(crate::common::AppEvent::JobLog {
                job_id: self.job_id,
                seq,
                level: level.into(),
                message: message.into(),
                context,
            });
        }
    }

    /// Convenience: an `info` timeline line with no structured context.
    pub async fn info(&self, message: &str) {
        self.log("info", message, serde_json::json!({})).await;
    }

    /// Set one column on this run's row.
    async fn update_row(
        &self,
        column: super::models::job::Column,
        value: Expr,
    ) -> Result<sea_orm::UpdateResult, sea_orm::DbErr> {
        super::models::job::Entity::update_many()
            .col_expr(column, value)
            .filter(super::models::job::Column::Id.eq(self.job_id))
            .exec(&self.db)
            .await
    }

    /// Replace the job's structured `detail` with what this run reports. Best-effort.
    pub async fn report(&self, report: JobReport) {
        if let Err(e) = self
            .update_row(
                super::models::job::Column::Detail,
                Expr::value(report.to_value()),
            )
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to set job detail");
        }
    }

    /// Set the job's `site_id` scope column (promoted from `detail` for list filtering).
    pub async fn set_site(&self, site_id: Uuid) {
        if let Err(e) = self
            .update_row(super::models::job::Column::SiteId, Expr::value(site_id))
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to set job site_id");
        }
    }

    /// Atomically persist `progress` (and `total` when provided) onto the row **and** emit the
    /// matching `JobProgress` event, so the stored row and the live SSE stream never disagree and a
    /// crash leaves a truthful last-known checkpoint. Best-effort: a failed write is logged, never
    /// fatal to the job.
    pub async fn set_progress(&self, progress: i32, total: Option<i32>) {
        let mut update = super::models::job::Entity::update_many()
            .col_expr(super::models::job::Column::Progress, Expr::value(progress));
        if let Some(t) = total {
            update = update.col_expr(super::models::job::Column::Total, Expr::value(t));
        }
        if let Err(e) = update
            .filter(super::models::job::Column::Id.eq(self.job_id))
            .exec(&self.db)
            .await
        {
            tracing::warn!(error = %e, job_id = %self.job_id, "Failed to update job progress");
        }
        let _ = self.events.send(crate::common::AppEvent::JobProgress {
            job_id: self.job_id,
            status: "running".into(),
            progress: Some(progress),
            total,
        });
    }
}

/// What a run reports, in one shape across every job: `scope` says what the run covered, `counts`
/// how much of each kind it moved. A count is a number, so an aggregate over `detail->'counts'`
/// cannot meet a boolean or a string; anything else about the run belongs in `scope`.
#[derive(Debug, Default, Clone)]
pub struct JobReport {
    scope: serde_json::Map<String, serde_json::Value>,
    counts: std::collections::BTreeMap<String, i64>,
}

impl JobReport {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What the run covered: an id, a window, a flag, a target name.
    #[must_use]
    pub fn scope(mut self, key: &str, value: impl Into<serde_json::Value>) -> Self {
        self.scope.insert(key.to_string(), value.into());
        self
    }

    /// Skip a scope entry whose value is absent, so an unscoped run reports no key rather than a
    /// null.
    #[must_use]
    pub fn scope_opt(self, key: &str, value: Option<impl Into<serde_json::Value>>) -> Self {
        match value {
            Some(v) => self.scope(key, v),
            None => self,
        }
    }

    /// How much of one kind the run moved. A width that does not fit saturates at [`i64::MAX`]
    /// rather than refusing the report.
    #[must_use]
    pub fn count(mut self, key: &str, value: impl TryInto<i64>) -> Self {
        self.counts
            .insert(key.to_string(), value.try_into().unwrap_or(i64::MAX));
        self
    }

    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({ "scope": self.scope, "counts": self.counts })
    }
}

// --- Cadence types and the pure next-run math ---
//
// Recurring-Service scheduling types and the pure cadence math (ADR 0001).
//
// Migration-independent on purpose: the `schedules` table, the per-replica scheduler tick that
// reads it, and the SeaORM model land with the worker-pool migration. This module defines the
// policy types and the drift-free next-run / catch-up decisions those pieces apply, so the tricky
// timing logic is unit-tested in isolation (no DB) before it drives anything.

/// What to do when a schedule is due but the previous run of the same job is still active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlapPolicy {
    /// Don't enqueue while a run is still in flight (the safe default for janitor/sweeper).
    #[default]
    SkipIfRunning,
    /// Always enqueue, even if one is running, only for genuinely concurrency-safe jobs.
    AllowConcurrent,
}

impl OverlapPolicy {
    /// The stable string persisted to `schedules.overlap_policy`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SkipIfRunning => "skip_if_running",
            Self::AllowConcurrent => "allow_concurrent",
        }
    }

    /// Parse the persisted string, falling back to the default for an unknown/missing value so a
    /// hand-edited row can never make the scheduler panic.
    #[must_use]
    pub fn from_str_or_default(s: Option<&str>) -> Self {
        match s {
            Some("allow_concurrent") => Self::AllowConcurrent,
            _ => Self::SkipIfRunning,
        }
    }
}

/// What to do about runs missed while the scheduler was down (a deploy/restart gap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatchupPolicy {
    /// Fire a single run now to resync, then drop the missed backlog. Never replay every tick.
    #[default]
    RunOnce,
    /// Skip entirely; wait for the next scheduled slot.
    Skip,
}

impl CatchupPolicy {
    /// The stable string persisted to `schedules.catchup_policy`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunOnce => "run_once",
            Self::Skip => "skip",
        }
    }

    /// Parse the persisted string, falling back to the default for an unknown/missing value.
    #[must_use]
    pub fn from_str_or_default(s: Option<&str>) -> Self {
        match s {
            Some("skip") => Self::Skip,
            _ => Self::RunOnce,
        }
    }
}

/// A recurring Service's cadence + policies. The persisted form (a `schedules` row) carries these
/// plus identity / enabled / tunables; this is the in-memory shape the scheduler reasons over and
/// that `Job::default_schedule` returns when seeding a schedule on first start.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub interval: chrono::Duration,
    pub overlap: OverlapPolicy,
    pub catchup: CatchupPolicy,
}

impl Schedule {
    /// A schedule firing every `secs` seconds with the default skip-if-running / run-once policies.
    #[must_use]
    pub fn every_secs(secs: i64) -> Self {
        Self {
            interval: chrono::Duration::seconds(secs),
            overlap: OverlapPolicy::default(),
            catchup: CatchupPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_overlap(mut self, overlap: OverlapPolicy) -> Self {
        self.overlap = overlap;
        self
    }

    #[must_use]
    pub fn with_catchup(mut self, catchup: CatchupPolicy) -> Self {
        self.catchup = catchup;
        self
    }

    /// Next run strictly after `now`, on the grid anchored at `anchor` (the schedule's current
    /// `next_run_at`), so cadence never drifts by a run's own duration. After a downtime gap the
    /// grid snaps forward to the first future slot, discarding the missed backlog (whether to *also*
    /// fire once now is the scheduler's `CatchupPolicy` decision).
    #[must_use]
    pub fn next_run_after(&self, anchor: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
        next_run_after(anchor, self.interval, now)
    }

    /// New `next_run_at` to apply immediately when an operator edits the cadence, so a lowered
    /// interval takes effect now instead of waiting out the old one.
    #[must_use]
    pub fn next_after_edit(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now + self.interval
    }
}

/// Drift-free next grid point strictly after `now`, where the grid is `anchor + k * interval`
/// (`k >= 1`). A zero/negative interval is guarded to one second so a misconfigured schedule can't
/// busy-loop or stall.
#[must_use]
pub fn next_run_after(
    anchor: DateTime<Utc>,
    interval: chrono::Duration,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let interval_ms = interval.num_milliseconds();
    if interval_ms <= 0 {
        return now + chrono::Duration::seconds(1);
    }
    let delta_ms = (now - anchor).num_milliseconds();
    let steps = if delta_ms < 0 {
        0
    } else {
        delta_ms / interval_ms
    };
    let mut next = anchor + interval * ((steps + 1) as i32);
    // Guard against rounding leaving us on/under `now`.
    while next <= now {
        next += interval;
    }
    next
}

// --- The recurring-service scheduler ---
//
// DB-backed recurring-Service scheduler (ADR 0001, Wave 2).
//
// Every replica runs this tick loop, but each due Service fires exactly once per scheduled slot
// across the whole fleet. Two layers guarantee that at 2-3 k8s replicas:
//
//   1. **Row claim.** A tick selects due `schedules` rows with `FOR UPDATE SKIP LOCKED` inside one
//      transaction, advances each row's `next_run_at` off its *scheduled* time (drift-free, via
//      [`next_run_after`]), and commits. A peer ticking concurrently skips the locked row
//      and sees the advanced `next_run_at`, so it won't re-pick the same slot.
//   2. **Enqueue dedupe.** The job is enqueued with `dedupe_key = "{job_name}:{scheduled_epoch}"`;
//      [`enqueue`] is `ON CONFLICT (dedupe_key) DO NOTHING`, so even if two replicas raced
//      the same slot (clock skew, a missed lock), only one `queued` job is ever created.
//
// Overlap policy `SkipIfRunning` (the default) additionally skips the enqueue when a non-terminal
// job of that `job_name` already exists, so a slow run can't stack up behind itself. The schedule's
// `next_run_at` is still advanced regardless, so the cadence grid never drifts.

/// How often each replica scans `schedules` for due rows. Well below the shortest Service cadence
/// (the notification dispatcher's ~60s) so a due slot fires within a few seconds.
pub const TICK_SECONDS: u64 = 5;

/// Seed a `schedules` row for every recurring Service the registry knows about. Idempotent
/// (`ON CONFLICT (job_name) DO NOTHING`), so an operator's later cadence edits survive a restart and
/// only a genuinely new Service inserts a row. `next_run_at` starts one interval out from now.
pub async fn seed_default_schedules(
    db: &DatabaseConnection,
    registry: &JobRegistry,
) -> Result<(), sea_orm::DbErr> {
    let now = Utc::now();
    for (job_name, sched) in registry.default_schedules() {
        let interval_seconds = Ord::max(sched.interval.num_seconds(), 1);
        let next_run_at = now + sched.interval;
        let row = schedule::ActiveModel {
            job_name: Set(job_name.to_string()),
            enabled: Set(true),
            next_run_at: Set(Some(next_run_at)),
            interval_seconds: Set(Some(interval_seconds)),
            overlap_policy: Set(Some(sched.overlap.as_str().to_string())),
            catchup_policy: Set(Some(sched.catchup.as_str().to_string())),
            ..Default::default()
        };
        schedule::Entity::insert(row)
            .on_conflict_do_nothing()
            .exec(db)
            .await?;
    }
    Ok(())
}

/// A due schedule row claimed for this tick.
struct DueSchedule {
    job_name: String,
    /// The slot time this firing represents, the `next_run_at` that just came due. The dedupe key
    /// is built from this so a re-pick of the same slot collapses to one job.
    scheduled_at: DateTime<Utc>,
    interval_seconds: i64,
    overlap: OverlapPolicy,
    catchup: CatchupPolicy,
    /// Whether this slot is a backlog slot rather than the cadence coming round: its scheduled time
    /// is a full interval or more behind now, which only happens after a gap in the scheduler.
    missed: bool,
    /// The operator-edited tunables snapshot, carried onto the enqueued job's params. Jobs read it
    /// from `ctx.params()["tunables"]`; it is fixed at enqueue time, so a mid-run edit to the
    /// schedule does NOT affect a job already queued/running (intentional, a run uses one snapshot).
    tunables: serde_json::Value,
}

/// One scheduler pass: claim due rows, advance their grid, and enqueue each (honoring overlap).
/// Returns how many Services were enqueued this tick. Separated from [`run`] so a test can drive a
/// single deterministic tick. Each due row is claimed and advanced in its own short transaction so a
/// slow enqueue can't hold a lock across the whole batch.
pub async fn tick(
    db: &DatabaseConnection,
    registry: &JobRegistry,
) -> Result<usize, sea_orm::DbErr> {
    let mut enqueued = 0usize;
    loop {
        let Some(due) = claim_one_due(db).await? else {
            break;
        };
        if enqueue_due(db, registry, &due).await? {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// Claim one due, enabled schedule row (`FOR UPDATE SKIP LOCKED`), advance its `next_run_at` off the
/// scheduled time (drift-free), and stamp `last_enqueued_at`, all in one transaction so a peer can't
/// re-pick the same slot. Returns the claimed slot (whose job the caller then enqueues) or `None`
/// when nothing is due.
/// The schedule row a claim takes off the grid, decoded as one row rather than column by column.
#[derive(sea_orm::FromQueryResult)]
struct ClaimedSchedule {
    id: uuid::Uuid,
    job_name: String,
    next_run_at: DateTime<Utc>,
    interval_seconds: Option<i64>,
    overlap_policy: Option<String>,
    catchup_policy: Option<String>,
    tunables: Option<serde_json::Value>,
}

/// The enabled schedules whose slot has come due, soonest first, one row locked for this claim and
/// skipped by any peer already holding it.
fn due_schedules() -> sea_orm::Select<schedule::Entity> {
    schedule::Entity::find()
        .filter(schedule::Column::Enabled.eq(true))
        .filter(schedule::Column::NextRunAt.is_not_null())
        .filter(Expr::col(schedule::Column::NextRunAt).lte(Expr::current_timestamp()))
        .order_by_asc(schedule::Column::NextRunAt)
        .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
        .limit(1)
}

async fn claim_one_due(db: &DatabaseConnection) -> Result<Option<DueSchedule>, sea_orm::DbErr> {
    let txn = db.begin().await?;
    let Some(row) = due_schedules()
        .into_model::<ClaimedSchedule>()
        .one(&txn)
        .await?
    else {
        txn.commit().await?;
        return Ok(None);
    };

    let ClaimedSchedule {
        id,
        job_name,
        next_run_at: scheduled_at,
        interval_seconds,
        overlap_policy,
        catchup_policy,
        tunables,
    } = row;
    // A cadence of zero would fire in a tight loop, the two policies read their own vocabularies,
    // and `tunables` is `NOT NULL DEFAULT '{}'`, so a hand-edited null takes an empty object.
    let interval_seconds = Ord::max(interval_seconds.unwrap_or(0), 1);
    let overlap = OverlapPolicy::from_str_or_default(overlap_policy.as_deref());
    let catchup = CatchupPolicy::from_str_or_default(catchup_policy.as_deref());
    let tunables = tunables.unwrap_or_else(|| serde_json::json!({}));

    // Advance the grid off the SCHEDULED time, not `now()`, so cadence never drifts by a run's own
    // latency and a downtime gap snaps forward to the next future slot (discarding the backlog).
    let now = Utc::now();
    let interval = chrono::Duration::seconds(interval_seconds);
    let missed = scheduled_at + interval <= now;
    let next = next_run_after(scheduled_at, interval, now);
    super::models::schedule::Entity::update_many()
        .col_expr(
            super::models::schedule::Column::NextRunAt,
            sea_orm::sea_query::Expr::value(next),
        )
        .col_expr(
            super::models::schedule::Column::LastEnqueuedAt,
            sea_orm::sea_query::Expr::current_timestamp(),
        )
        .filter(super::models::schedule::Column::Id.eq(id))
        .exec(&txn)
        .await?;
    txn.commit().await?;

    Ok(Some(DueSchedule {
        job_name,
        scheduled_at,
        interval_seconds,
        overlap,
        catchup,
        missed,
        tunables,
    }))
}

/// Enqueue the job for a claimed slot, honoring its catchup and overlap policies. Returns whether a
/// job row was actually created (false when the slot is a skipped backlog slot, when skipped-if-
/// running, when the dedupe key collided, or when no handler exists).
async fn enqueue_due(
    db: &DatabaseConnection,
    registry: &JobRegistry,
    due: &DueSchedule,
) -> Result<bool, sea_orm::DbErr> {
    // A schedule row for a job the running binary doesn't know about (e.g. a removed Service whose
    // row lingers): the worker would just fail it "no handler registered", so skip the enqueue and
    // let the row keep advancing harmlessly.
    if registry.get(&due.job_name).is_none() {
        tracing::warn!(job_name = %due.job_name, "scheduler: no registered job for schedule; skipping");
        return Ok(false);
    }

    // The grid has already been advanced, so declining here waits for the next scheduled slot
    // rather than dropping the cadence.
    if matches!(due.catchup, CatchupPolicy::Skip) && due.missed {
        tracing::debug!(job_name = %due.job_name, "scheduler: missed slot not replayed (catchup=skip)");
        return Ok(false);
    }

    if matches!(due.overlap, OverlapPolicy::SkipIfRunning)
        && non_terminal_exists(db, &due.job_name).await?
    {
        tracing::debug!(job_name = %due.job_name, "scheduler: previous run still active; skipping (overlap=skip_if_running)");
        return Ok(false);
    }

    // Key the enqueue on (job, scheduled slot) so two replicas racing this slot collapse to one job.
    // Truncate to whole seconds so a sub-second clock difference between replicas can't split the key.
    let dedupe_key = format!("{}:{}", due.job_name, due.scheduled_at.timestamp());
    let created = enqueue(
        db,
        &due.job_name,
        None,
        None,
        &serde_json::json!({
            "scheduled_at": due.scheduled_at,
            "interval_seconds": due.interval_seconds,
            "tunables": due.tunables,
        }),
        Some(&dedupe_key),
    )
    .await?;
    Ok(created.is_some())
}

/// Whether a non-terminal job of this `job_name` already exists, the skip-if-running guard. Covers
/// every pre-completion state, including the `pending`/`retrying` statuses older rows carry.
async fn non_terminal_exists(
    db: &DatabaseConnection,
    job_name: &str,
) -> Result<bool, sea_orm::DbErr> {
    let row = super::Entity::find()
        .filter(super::Column::TriggerType.eq(job_name))
        .filter(super::Column::Status.is_in(["queued", "pending", "running", "retrying"]))
        .one(db)
        .await?;
    Ok(row.is_some())
}

/// This replica's scheduler loop: every [`TICK_SECONDS`], claim and enqueue due Services, then idle.
/// Runs until `shutdown` resolves. Mirrors the worker's biased `tokio::select!` so a shutdown is
/// observed promptly and no new work is enqueued during the drain window.
pub async fn run_scheduler(
    db: DatabaseConnection,
    registry: Arc<JobRegistry>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    tracing::info!(tick_secs = TICK_SECONDS, "schedule scheduler started");
    tokio::pin!(shutdown);
    let mut ticker = tokio::time::interval(Duration::from_secs(TICK_SECONDS));
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => {
                tracing::info!("scheduler stopping on shutdown");
                return;
            }
            _ = ticker.tick() => {
                match tick(&db, &registry).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(enqueued = n, "scheduler tick enqueued due services"),
                    Err(e) => tracing::warn!(error = %e, "scheduler tick failed"),
                }
            }
        }
    }
}

// --- The claim-based worker pool ---
//
// Claim-based multi-replica worker pool: each replica claims a `queued` (or reapable) job with
// `SELECT … FOR UPDATE SKIP LOCKED`, leases it, and commits ownership-guarded so a reaped stalled
// worker can't clobber the new owner. Idempotency makes the rare overlap harmless. See ADR 0001.

/// Lease lifetime before the reaper may reclaim a row. Sized well above a plausible GC / k8s
/// CPU-throttle stall so a slow-but-alive worker is not reaped mid-run.
pub const LEASE_SECONDS: i64 = 120;

/// When a lease taken now lapses. Server-side `now()`, so a worker whose clock has drifted cannot
/// hold a row past the reaper's reach or hand it over early.
/// The ownership guard every write a running worker makes carries: the row, this worker, and the
/// lease it was granted. A worker reaped out matches no row and its late write is a no-op.
fn owned_by(job_id: Uuid, worker_id: &str, lease_epoch: i64) -> Expr {
    Column::Id
        .eq(job_id)
        .and(Column::Owner.eq(worker_id))
        .and(Column::LeaseEpoch.eq(lease_epoch))
}

fn lease_expiry() -> Expr {
    Expr::cust_with_values(
        "now() + (interval '1 second' * $1)",
        [sea_orm::Value::from(LEASE_SECONDS)],
    )
}
/// Lease-renewal cadence, roughly one third of the lease.
pub const HEARTBEAT_SECONDS: u64 = 40;
/// Idle poll cadence when nothing is claimable.
pub const POLL_SECONDS: u64 = 2;

/// Identity for this replica's worker: pid + a short random suffix.
#[must_use]
pub fn worker_id() -> String {
    let uuid = Uuid::new_v4().to_string();
    format!("worker-{}-{}", std::process::id(), &uuid[..8])
}

/// Enqueue a `queued` job for the worker pool. A set `dedupe_key` makes the enqueue idempotent (a
/// duplicate inserts nothing and returns `None`), which keeps two replicas racing a scheduler tick
/// from double-firing one run.
///
/// Called from a CRUD hook, `db` is the transaction the write runs in: the row is invisible to the
/// pool until that commits, and a write that fails takes its job with it.
pub async fn enqueue<C: ConnectionTrait>(
    db: &C,
    trigger_type: &str,
    sensor_id: Option<Uuid>,
    trigger_id: Option<Uuid>,
    params: &serde_json::Value,
    dedupe_key: Option<&str>,
) -> Result<Option<Uuid>, sea_orm::DbErr> {
    let id = Uuid::new_v4();
    let category = category_for(trigger_type);
    // The conflict target carries the index's own predicate: the unique index on `dedupe_key` is
    // partial, so naming the column alone matches no index.
    let inserted = Entity::insert(ActiveModel {
        id: Set(id),
        trigger_type: Set(trigger_type.to_string()),
        sensor_id: Set(sensor_id),
        trigger_id: Set(trigger_id),
        status: Set("queued".to_string()),
        category: Set(category.to_string()),
        params: Set(params.clone()),
        dedupe_key: Set(dedupe_key.map(ToString::to_string)),
        next_attempt_at: Set(chrono::Utc::now().into()),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(Column::DedupeKey)
            .target_and_where(Expr::col(Column::DedupeKey).is_not_null())
            .do_nothing()
            .to_owned(),
    )
    .try_insert()
    .exec(db)
    .await?;
    Ok(match inserted {
        TryInsertResult::Inserted(_) => Some(id),
        TryInsertResult::Conflicted | TryInsertResult::Empty => None,
    })
}

/// A row claimed off the queue.
#[derive(FromQueryResult)]
struct Claimed {
    id: Uuid,
    trigger_type: String,
    lease_epoch: i64,
    params: serde_json::Value,
    /// Attempts already spent on this row, so the timeline says which try it is watching.
    retry_count: i32,
}

/// Claim one due `queued` row or one orphaned `running` row (the reaper arm), stamping this
/// worker's ownership and a fresh lease. A claimed row always carries a lease, so a `running` row
/// with none is an orphan too: rows stranded by the pre-worker-pool spawn path have no lease at all. `SKIP LOCKED` keeps two workers from taking the same row.
/// The claim releases `dedupe_key`: the key coalesces enqueues while a job waits, and a change
/// landing once the run has started needs a run of its own.
async fn claim_one(
    db: &DatabaseConnection,
    worker_id: &str,
) -> Result<Option<Claimed>, sea_orm::DbErr> {
    // The select and the update are one transaction because the row lock is what keeps two
    // workers off the same row: `SKIP LOCKED` holds it until this transaction commits, so a
    // second worker's select passes over it rather than waiting for it.
    let txn = db.begin().await?;
    let now = || Expr::current_timestamp();
    let due = Column::Status
        .eq("queued")
        .and(Expr::col(Column::NextAttemptAt).lte(now()));
    let orphaned = Column::Status.eq("running").and(
        Column::LeaseExpiresAt
            .is_null()
            .or(Expr::col(Column::LeaseExpiresAt).lt(now())),
    );
    let Some(row) = Entity::find()
        .filter(due.or(orphaned))
        .order_by_asc(Column::NextAttemptAt)
        .limit(1)
        .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
        .one(&txn)
        .await?
    else {
        txn.commit().await?;
        return Ok(None);
    };
    let lease_epoch = row.lease_epoch + 1;
    Entity::update_many()
        .col_expr(Column::Status, Expr::value("running"))
        .col_expr(Column::Owner, Expr::value(worker_id))
        .col_expr(Column::DedupeKey, Expr::value(Option::<String>::None))
        .col_expr(Column::LeaseEpoch, Expr::value(lease_epoch))
        .col_expr(Column::LeaseExpiresAt, lease_expiry())
        .filter(Column::Id.eq(row.id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(Some(Claimed {
        id: row.id,
        trigger_type: row.trigger_type,
        lease_epoch,
        params: row.params,
        retry_count: row.retry_count,
    }))
}

/// Renew the lease on a cadence while the job runs, and observe cross-replica cancellation: if
/// `cancel_requested` is set on the row, flip the in-process flag so the job's checkpoints stop; if
/// the ownership-guarded renewal matches no row (we were reaped), flip cancel and stop heartbeating.
async fn heartbeat(
    db: DatabaseConnection,
    job_id: Uuid,
    worker_id: String,
    lease_epoch: i64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(HEARTBEAT_SECONDS));
    tick.tick().await; // the immediate first tick, skip it, the claim just set the lease
    loop {
        tick.tick().await;
        // Renew and read the cancel flag as two statements: the renewal is what proves ownership,
        // and cancellation is advisory, re-read on the next tick if this read misses it.
        let renewed = Entity::update_many()
            .col_expr(Column::LeaseExpiresAt, lease_expiry())
            .filter(owned_by(job_id, &worker_id, lease_epoch))
            .exec(&db)
            .await;
        match renewed {
            Ok(res) if res.rows_affected > 0 => match Entity::find_by_id(job_id).one(&db).await {
                Ok(Some(row)) if row.cancel_requested => cancel.store(true, Ordering::Relaxed),
                Ok(_) => {}
                Err(e) => tracing::warn!(job_id = %job_id, error = %e, "cancel flag unreadable"),
            },
            // No row matched → we lost the lease (reclaimed). Stop the job and stop heartbeating.
            Ok(_) => {
                cancel.store(true, Ordering::Relaxed);
                break;
            }
            Err(e) => tracing::warn!(error = %e, job_id = %job_id, "job heartbeat failed"),
        }
    }
}

/// Mark a finished job terminal, **ownership-guarded** so a reaped-out worker's late write is a no-op.
/// Returns whether this worker still owned the row (i.e. whether the write took effect).
async fn commit_terminal(
    db: &DatabaseConnection,
    claimed: &Claimed,
    worker_id: &str,
    status: &str,
    readings_updated: Option<i32>,
    error_message: Option<&str>,
) -> Result<bool, sea_orm::DbErr> {
    let res = Entity::update_many()
        .col_expr(Column::Status, Expr::value(status))
        .col_expr(Column::ReadingsUpdated, Expr::value(readings_updated))
        .col_expr(Column::ErrorMessage, Expr::value(error_message))
        .col_expr(Column::CompletedAt, Expr::current_timestamp())
        .col_expr(Column::Owner, Expr::value(Option::<String>::None))
        .col_expr(
            Column::LeaseExpiresAt,
            Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
        )
        .filter(owned_by(claimed.id, worker_id, claimed.lease_epoch))
        .exec(db)
        .await?;
    Ok(res.rows_affected > 0)
}

/// On a retryable failure, durably reschedule (`status='queued'`, future `next_attempt_at` with
/// exponential backoff) until the retry budget is spent, then fail. Ownership-guarded. The backoff is
/// computed in SQL from the *current* `retry_count` so it survives restarts (no in-process timer).
/// Returns the status the row landed on, or `None` when another worker owned it.
async fn reschedule_or_fail(
    db: &DatabaseConnection,
    claimed: &Claimed,
    worker_id: &str,
    policy: RetryPolicy,
    error_message: &str,
) -> Result<Option<String>, sea_orm::DbErr> {
    let max_retries = i64::from(policy.max_retries);
    let backoff_base = policy.backoff_base.as_secs() as i64;
    // The backoff is read off the row's own `retry_count` rather than the claim's, so a restart
    // between the claim and the failure still doubles from where the row stands.
    let retrying = Expr::col(Column::RetryCount).lt(max_retries);
    let res = Entity::update_many()
        .col_expr(
            Column::Status,
            Expr::case(retrying.clone(), "queued")
                .finally("failed")
                .into(),
        )
        .col_expr(
            Column::RetryCount,
            Expr::col(Column::RetryCount).add(Expr::value(1)),
        )
        .col_expr(Column::ErrorMessage, Expr::value(error_message))
        .col_expr(
            Column::NextAttemptAt,
            Expr::case(
                retrying.clone(),
                Expr::cust_with_values(
                    "now() + (interval '1 second' * ($1 * power(2, retry_count)))",
                    [sea_orm::Value::from(backoff_base)],
                ),
            )
            .finally(Expr::col(Column::NextAttemptAt))
            .into(),
        )
        .col_expr(
            Column::CompletedAt,
            Expr::case(
                retrying,
                Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
            )
            .finally(Expr::current_timestamp())
            .into(),
        )
        .col_expr(Column::Owner, Expr::value(Option::<String>::None))
        .col_expr(
            Column::LeaseExpiresAt,
            Expr::value(Option::<sea_orm::prelude::DateTimeWithTimeZone>::None),
        )
        .filter(owned_by(claimed.id, worker_id, claimed.lease_epoch))
        .exec(db)
        .await?;
    if res.rows_affected == 0 {
        return Ok(None);
    }
    Ok(Some(
        if i64::from(claimed.retry_count) < max_retries {
            "queued"
        } else {
            "failed"
        }
        .to_string(),
    ))
}

/// Run a single claimed job to its terminal (or rescheduled) state. Separated from [`run`] so tests
/// can drive one cycle deterministically.
async fn execute(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
    policy: RetryPolicy,
    claimed: Claimed,
) -> Result<(), sea_orm::DbErr> {
    let Some(job) = registry.get(&claimed.trigger_type) else {
        // No handler, fail rather than let the reaper reclaim it forever.
        const NO_HANDLER: &str = "no handler registered for trigger_type";
        if commit_terminal(db, &claimed, worker_id, "failed", None, Some(NO_HANDLER)).await? {
            let _ = events.send(crate::common::AppEvent::JobCompleted {
                job_id: claimed.id,
                status: "failed".to_string(),
                readings_updated: None,
                error_message: Some(NO_HANDLER.to_string()),
            });
        }
        return Ok(());
    };

    let (ctx, cancel) = JobContext::for_worker(
        db.clone(),
        claimed.id,
        events.clone(),
        claimed.params.clone(),
    );
    // The two lines every run owes its timeline. A job body says what only it knows; that a run
    // started and how it ended is the worker's to say, so a silent job is impossible.
    let timeline = ctx.clone();
    timeline
        .log(
            "info",
            &format!("{} started", claimed.trigger_type),
            serde_json::json!({
                "trigger_type": claimed.trigger_type,
                "attempt": claimed.retry_count + 1,
            }),
        )
        .await;
    let hb = tokio::spawn(heartbeat(
        db.clone(),
        claimed.id,
        worker_id.to_string(),
        claimed.lease_epoch,
        cancel.clone(),
    ));

    // Catch a handler panic so it becomes a normal job failure instead of unwinding the worker task.
    // Without this, a panic skips `hb.abort()` below, the detached heartbeat then renews the lease
    // forever (the reaper never reclaims the row) while this replica is left with no worker.
    let run_result = std::panic::AssertUnwindSafe(job.run(ctx))
        .catch_unwind()
        .await;
    hb.abort();

    let outcome = match run_result {
        Ok(result) => result,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "job handler panicked".to_string());
            tracing::error!(job_id = %claimed.id, panic = %msg, "job handler panicked");
            Err(sea_orm::DbErr::Custom(format!(
                "job handler panicked: {msg}"
            )))
        }
    };

    match outcome {
        Ok(count) => {
            let status = if cancel.load(Ordering::Relaxed) {
                "cancelled"
            } else {
                "completed"
            };
            let readings = i32::try_from(count).unwrap_or(i32::MAX);
            timeline
                .log(
                    "info",
                    &format!("{} {status}", claimed.trigger_type),
                    serde_json::json!({ "status": status, "reported": count }),
                )
                .await;
            let owned =
                commit_terminal(db, &claimed, worker_id, status, Some(readings), None).await?;
            if owned {
                // Most tracked jobs rewrite reading values or attribution (recalibration, reprocess,
                // pairing/derived backfill, merge, adopt/swap), any of which can change breach
                // state. Reconcile unconditionally:
                // it is idempotent, O(active slots), spawns no jobs (no recursion), and is merely
                // redundant for jobs that don't touch values. Guarded on `owned` so only the winning
                // worker runs it.
                crate::routes::private::alarms::flows::reconcile_all_and_notify(db, events).await;
                let _ = events.send(crate::common::AppEvent::JobCompleted {
                    job_id: claimed.id,
                    status: status.to_string(),
                    readings_updated: Some(readings),
                    error_message: None,
                });
            }
        }
        Err(e) => {
            let message = e.to_string();
            let outcome = if policy.max_retries > u32::try_from(claimed.retry_count).unwrap_or(0) {
                "retrying"
            } else {
                "failed"
            };
            timeline
                .log(
                    if outcome == "failed" { "error" } else { "warn" },
                    &format!("{} {outcome}", claimed.trigger_type),
                    serde_json::json!({ "status": outcome, "error": message }),
                )
                .await;
            // Announce the failure the way the success arm announces completion: a watcher that only
            // sees `JobCompleted` learns nothing of a run that failed or is waiting out its backoff.
            match reschedule_or_fail(db, &claimed, worker_id, policy, &message)
                .await?
                .as_deref()
            {
                Some("failed") => {
                    let _ = events.send(crate::common::AppEvent::JobCompleted {
                        job_id: claimed.id,
                        status: "failed".to_string(),
                        readings_updated: None,
                        error_message: Some(message),
                    });
                }
                Some(_) => {
                    let _ = events.send(crate::common::AppEvent::JobProgress {
                        job_id: claimed.id,
                        status: "retrying".to_string(),
                        progress: None,
                        total: None,
                    });
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// Claim and execute at most one job under the process-wide retry policy. Returns `true` if a job
/// ran, `false` if the queue was empty. The unit of work tests drive directly.
pub async fn run_one(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
) -> Result<bool, sea_orm::DbErr> {
    run_one_with_policy(db, events, registry, worker_id, job_retry_policy()).await
}

/// [`run_one`] with an explicit retry policy, for a caller that must not depend on the process-wide
/// one.
pub async fn run_one_with_policy(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
    policy: RetryPolicy,
) -> Result<bool, sea_orm::DbErr> {
    match claim_one(db, worker_id).await? {
        Some(claimed) => {
            execute(db, events, registry, worker_id, policy, claimed).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Run claimable jobs until the queue drains. Lets a test pump the worker after enqueuing.
pub async fn drain(
    db: &DatabaseConnection,
    events: &crate::common::EventSender,
    registry: &JobRegistry,
    worker_id: &str,
) -> Result<(), sea_orm::DbErr> {
    while run_one(db, events, registry, worker_id).await? {}
    Ok(())
}

/// This replica's worker loop: drain claimable work, then idle-poll. `shutdown` stops the worker
/// claiming anything new; the job already claimed runs to completion first, so a rolled pod hands
/// back finished work rather than a part-written row holding its lease until the reaper takes it.
pub async fn run_workers(
    db: DatabaseConnection,
    events: crate::common::EventSender,
    registry: Arc<JobRegistry>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    let wid = worker_id();
    tracing::info!(worker_id = %wid, "job worker started");
    tokio::pin!(shutdown);
    loop {
        // Never race the shutdown against the work: `select!` drops the loser, which would abandon
        // a claimed job at whatever await point it had reached.
        let idle = match run_one(&db, &events, &registry, &wid).await {
            Ok(ran) => !ran,
            Err(e) => {
                tracing::warn!(error = %e, worker_id = %wid, "worker cycle failed");
                true
            }
        };
        if idle {
            tokio::select! {
                biased;
                () = &mut shutdown => {
                    tracing::info!(worker_id = %wid, "job worker stopping on shutdown");
                    return;
                }
                () = tokio::time::sleep(Duration::from_secs(POLL_SECONDS)) => {}
            }
        } else if (&mut shutdown).now_or_never().is_some() {
            tracing::info!(worker_id = %wid, "job worker stopping on shutdown, claimed job finished");
            return;
        }
    }
}

// --- CRUD operations ---
//
//

/// The rerun and cancel policies live in the policy tables above and are enforced there; a row carries
/// what they say about it so a client renders the two buttons from the server's answer rather than
/// from a copy of the lists.
pub struct ReprocessingJobOperations;

impl CRUDOperations for ReprocessingJobOperations {
    type Resource = ReprocessingJob;

    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        entity: &mut ReprocessingJob,
    ) -> Result<(), ApiError> {
        entity.rerunnable = is_rerunnable(&entity.trigger_type);
        entity.cancellable = is_cancellable(&entity.trigger_type);
        Ok(())
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        _db: &C,
        entities: &mut Vec<<ReprocessingJob as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        for entity in entities {
            entity.rerunnable = is_rerunnable(&entity.trigger_type);
            entity.cancellable = is_cancellable(&entity.trigger_type);
        }
        Ok(())
    }
}

// --- Schedule CRUD operations ---
//
// What the generated CRUD cannot state about a schedule: which edits are legal, what an edit does
// to the grid, and the trail it leaves.

pub struct ScheduleOperations;

/// The registry a schedule is validated against: the on-demand jobs plus the recurring Services
/// with their cadence. Stateless and cheap, rebuilt per call. The config comes from the process's
/// own `AppState`; only its config is read, never its pool.
fn full_registry() -> JobRegistry {
    let mut registry = build_registry();
    if let Some(state) = crate::common::global_app_state() {
        register_scheduled_services(&mut registry, &state.config);
    }
    registry
}

/// Whether a string round-trips through the policy enum unchanged, which is how an unknown value is
/// told from one the enum silently defaults.
fn known_overlap(s: &str) -> bool {
    OverlapPolicy::from_str_or_default(Some(s)).as_str() == s
}

fn known_catchup(s: &str) -> bool {
    CatchupPolicy::from_str_or_default(Some(s)).as_str() == s
}

#[derive(FromQueryResult)]
struct Stored {
    enabled: bool,
    interval_seconds: Option<i64>,
    overlap_policy: Option<String>,
    catchup_policy: Option<String>,
    tunables: serde_json::Value,
}

/// The five editable fields, as `change_audit` records them on both sides of an edit.
fn snapshot(
    enabled: bool,
    interval_seconds: Option<i64>,
    overlap_policy: Option<&String>,
    catchup_policy: Option<&String>,
    tunables: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "enabled": enabled,
        "interval_seconds": interval_seconds,
        "overlap_policy": overlap_policy,
        "catchup_policy": catchup_policy,
        "tunables": tunables,
    })
}

/// Whether a job of each name is in flight, for the names given. One statement for a page.
async fn running_names<C: ConnectionTrait>(
    db: &C,
    names: &[String],
) -> Result<std::collections::HashSet<String>, ApiError> {
    if names.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT DISTINCT trigger_type FROM reprocessing_jobs \
              WHERE trigger_type = ANY($1) \
                AND status IN ('queued', 'pending', 'running', 'retrying')",
            [names.to_vec().into()],
        ))
        .await
        .map_err(ApiError::database)?;
    rows.iter()
        .map(|r| {
            r.try_get::<String>("", "trigger_type")
                .map_err(ApiError::database)
        })
        .collect()
}

/// The tunables a job declares, as the form reads them. A row whose name the registry does not know
/// declares none.
fn tunables_schema(registry: &JobRegistry, job_name: &str) -> Vec<TunableSpec> {
    registry
        .get(job_name)
        .map(|handler| handler.tunables())
        .unwrap_or_default()
}

impl CRUDOperations for ScheduleOperations {
    type Resource = super::models::schedule::Schedule;

    /// `running` and `tunables_schema` are resolved per request: the first from the live queue, the
    /// second from the job's own declaration. Neither is a column.
    async fn after_get_one<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut super::models::schedule::Schedule,
    ) -> Result<(), ApiError> {
        let registry = full_registry();
        entity.tunables_schema = tunables_schema(&registry, &entity.job_name);
        entity.running = !running_names(db, std::slice::from_ref(&entity.job_name))
            .await?
            .is_empty();
        Ok(())
    }

    /// An edit answers with the same row a read would, computed fields included: the form that
    /// saved it renders the response.
    async fn after_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entity: &mut super::models::schedule::Schedule,
    ) -> Result<(), ApiError> {
        self.after_get_one(db, entity).await
    }

    async fn after_get_all<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        entities: &mut Vec<<super::models::schedule::Schedule as CRUDResource>::ListModel>,
    ) -> Result<(), ApiError> {
        let registry = full_registry();
        let names: Vec<String> = entities.iter().map(|e| e.job_name.clone()).collect();
        let running = running_names(db, &names).await?;
        for entity in entities.iter_mut() {
            entity.tunables_schema = tunables_schema(&registry, &entity.job_name);
            entity.running = running.contains(&entity.job_name);
        }
        Ok(())
    }

    /// The change-audit trail records who edited a schedule, and the writer is only known to the
    /// request.
    async fn after_begin<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
    ) -> Result<(), ApiError> {
        crate::common::actor::declare(db)
            .await
            .map_err(ApiError::database)
    }

    /// Refuse an edit the scheduler could not act on, bring the grid forward when the edit implies
    /// it, and record what moved. All of it on the transaction the update runs in, so a refused or
    /// failed edit leaves neither a moved slot nor a trail entry.
    async fn before_update<C: ConnectionTrait + TransactionTrait>(
        &self,
        db: &C,
        job_name: String,
        data: &<super::models::schedule::Schedule as CRUDResource>::UpdateModel,
    ) -> Result<(), ApiError> {
        let interval = data.interval_seconds.flatten();
        if let Some(seconds) = interval
            && seconds < 1
        {
            return Err(ApiError::bad_request("interval_seconds must be >= 1"));
        }
        if let Some(policy) = data.overlap_policy.clone().flatten()
            && !known_overlap(&policy)
        {
            return Err(ApiError::bad_request(format!(
                "unknown overlap_policy '{policy}' (expected skip_if_running|allow_concurrent)"
            )));
        }
        if let Some(policy) = data.catchup_policy.clone().flatten()
            && !known_catchup(&policy)
        {
            return Err(ApiError::bad_request(format!(
                "unknown catchup_policy '{policy}' (expected run_once|skip)"
            )));
        }

        let registry = full_registry();
        // Tunables are validated by the owning job. A row whose name the registry does not know has
        // nothing to validate against and is still editable.
        if let Some(tunables) = data.tunables.clone().flatten()
            && let Some(handler) = registry.get(&job_name)
        {
            handler.validate(&tunables).map_err(ApiError::bad_request)?;
        }

        let Some(before) = schedule::Entity::find()
            .select_only()
            .columns([
                schedule::Column::Enabled,
                schedule::Column::IntervalSeconds,
                schedule::Column::OverlapPolicy,
                schedule::Column::CatchupPolicy,
                schedule::Column::Tunables,
            ])
            .filter(schedule::Column::JobName.eq(job_name.clone()))
            .into_model::<Stored>()
            .one(db)
            .await
            .map_err(ApiError::database)?
        else {
            return Ok(());
        };

        let enabled = data.enabled.flatten().unwrap_or(before.enabled);
        let interval_seconds = interval.or(before.interval_seconds);
        let overlap_policy = data
            .overlap_policy
            .clone()
            .flatten()
            .or_else(|| before.overlap_policy.clone());
        let catchup_policy = data
            .catchup_policy
            .clone()
            .flatten()
            .or_else(|| before.catchup_policy.clone());
        let tunables = data
            .tunables
            .clone()
            .flatten()
            .unwrap_or_else(|| before.tunables.clone());

        // A lowered interval or a re-enable takes effect now rather than waiting out the slot the
        // old cadence left behind. `next_run_at` is not an editable field, so this is the only
        // writer of it here and the update that follows does not touch it.
        let interval_changed = interval.is_some_and(|n| Some(n) != before.interval_seconds);
        let being_enabled = enabled && !before.enabled;
        if interval_changed || being_enabled {
            let seconds = Ord::max(interval_seconds.unwrap_or(1), 1);
            schedule::Entity::update_many()
                .col_expr(
                    schedule::Column::NextRunAt,
                    Expr::current_timestamp().add(Expr::cust_with_values(
                        "interval '1 second' * $1",
                        [seconds],
                    )),
                )
                .filter(schedule::Column::JobName.eq(job_name.clone()))
                .exec(db)
                .await
                .map_err(ApiError::database)?;
        }

        crate::routes::private::change_audit::service::record(
            db,
            format!("schedule:{job_name}"),
            "schedule_update",
            crate::common::actor::current(),
            Some(snapshot(
                before.enabled,
                before.interval_seconds,
                before.overlap_policy.as_ref(),
                before.catchup_policy.as_ref(),
                &before.tunables,
            )),
            Some(snapshot(
                enabled,
                interval_seconds,
                overlap_policy.as_ref(),
                catchup_policy.as_ref(),
                &tunables,
            )),
        )
        .await
        .map_err(ApiError::database)?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests/manual_run.rs"]
mod manual_run_tests;

#[cfg(test)]
#[path = "tests/job.rs"]
mod job_tests;

#[cfg(test)]
#[path = "tests/lifecycle.rs"]
mod report_tests;

#[cfg(test)]
#[path = "tests/schedule.rs"]
mod schedule_tests;
