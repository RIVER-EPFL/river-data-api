//! The uniform `Job` abstraction (ADR 0001).
//!
//! Every tracked-job kind implements [`Job`], and a name-keyed [`JobRegistry`] lets the worker pool
//! and the scheduler dispatch a claimed row (or a due schedule) to its handler by `trigger_type`,
//! so first-run and rerun share one code path. Implementations are **stateless handlers**: every
//! per-run input arrives via the job row's `params` (read through [`JobContext`]), so any replica
//! can run any job after claiming it.
//!
//! This is the migration-independent contract. The worker claim loop, the enqueue-flip, and reading
//! persisted `params` land with the worker-pool migration and build on this trait.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::DbErr;

use super::lifecycle::JobContext;
use super::registry;
use super::schedule::Schedule;

/// A kind of tracked job, keyed by its stable [`name`](Job::name) (the `trigger_type` written to
/// `reprocessing_jobs` and referenced by `schedules.job_name`).
#[async_trait]
pub trait Job: Send + Sync {
    /// Stable identifier, must equal the `trigger_type` persisted on the row and any
    /// `schedules.job_name` that enqueues this job.
    fn name(&self) -> &'static str;

    /// UI/retention classification. Delegates to the shared [`registry::category_for`] mapping so
    /// the trait and the string table can never disagree.
    fn category(&self) -> &'static str {
        registry::category_for(self.name())
    }

    /// Whether a finished job of this kind can be re-run by replaying its persisted inputs.
    fn rerunnable(&self) -> bool {
        registry::is_rerunnable(self.name())
    }

    /// Whether a running job of this kind observes [`JobContext::is_cancelled`] at batch
    /// checkpoints (single-statement jobs report 409 on a cancel attempt).
    fn cancellable(&self) -> bool {
        registry::is_cancellable(self.name())
    }

    /// The default cadence when this kind is a recurring Service. `None` (the common case) means
    /// on-demand only; recurring services (janitor, alarm sweeper, …) return `Some(_)` and the
    /// scheduler seeds a `schedules` row from it on first start.
    fn default_schedule(&self) -> Option<Schedule> {
        None
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
        registry.register(Arc::new(super::jobs::ReprocessSensor::new(trigger)));
    }
    registry.register(Arc::new(super::jobs::RefreshAggregates));
    for trigger in [
        "sensor_swap",
        "pairing_backfill",
        "manual_adopt",
        "attribution_pin",
    ] {
        registry.register(Arc::new(super::jobs::ReprocessSlot::new(trigger)));
    }
    for trigger in [
        "deployment_create",
        "deployment_update",
        "deployment_delete",
    ] {
        registry.register(Arc::new(super::jobs::ReprocessDeployment::new(trigger)));
    }
    registry.register(Arc::new(super::jobs::DerivedRecompute));
    registry.register(Arc::new(super::jobs::DerivedAssignment));
    registry.register(Arc::new(super::jobs::IngestDerived));
    for trigger in ["compute_derived", "batch_derived"] {
        registry.register(Arc::new(super::jobs::SiteTimestampsDerived::new(trigger)));
    }
    registry.register(Arc::new(super::jobs::ReprocessAll));
    registry.register(Arc::new(super::jobs::MeasurementRetag));
    registry.register(Arc::new(
        crate::routes::private::site_parameters::flows::SdEstimatorRetag,
    ));
    registry.register(Arc::new(crate::routes::private::readings::flows::CsvImport));
    registry.register(Arc::new(
        crate::routes::private::alarms::flows::AlarmBackfill,
    ));
    registry.register(Arc::new(super::jobs::BackfillAttribution));
    registry.register(Arc::new(super::jobs::BackfillCalibrations));
    registry.register(Arc::new(super::jobs::MergeSiteParameters));
    registry.register(Arc::new(super::jobs::MergeParameters));
    registry.register(Arc::new(super::jobs::PlanApply));
    registry.register(Arc::new(super::jobs::PlanRevert));
    registry.register(Arc::new(super::reconcile::ReplicateReconciliation));
    registry.register(Arc::new(super::reconcile::ReplicateReconciliationDelete));
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
    registry.register(Arc::new(super::jobs::JanitorRun::from_config(config)));
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

    // The policy tables in `registry` are keyed by trigger_type and cannot construct these, so the
    // name list they check against is verified here instead of drifting quietly.
    for name in super::registry::SCHEDULED_SERVICE_NAMES {
        assert!(
            registry.get(name).is_some(),
            "registry::SCHEDULED_SERVICE_NAMES lists {name:?}, which no service registered under"
        );
    }
}

#[cfg(test)]
#[path = "tests/job.rs"]
mod tests;
