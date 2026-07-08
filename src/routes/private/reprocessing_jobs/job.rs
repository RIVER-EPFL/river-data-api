//! The uniform `Job` abstraction (ADR 0001).
//!
//! Every tracked-job kind implements [`Job`], and a name-keyed [`JobRegistry`] lets the worker pool
//! and the scheduler dispatch a claimed row (or a due schedule) to its handler by `trigger_type` —
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
    /// Stable identifier — must equal the `trigger_type` persisted on the row and any
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

    /// Validate operator-supplied tunables before they are persisted onto a schedule or job.
    /// Default accepts anything; jobs with tunables override to reject bad values.
    fn validate(&self, _tunables: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }

    /// Execute one run. Inputs come from the job row via `ctx`; the returned count is recorded as
    /// `readings_updated` on completion.
    async fn run(&self, ctx: JobContext) -> Result<i64, DbErr>;
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

    /// Register a job kind. Panics on a duplicate `name()` — a programming error caught at startup
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

    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

/// Build the registry of every on-demand worker-run job kind. Called once at startup; jobs register
/// here as they are ported off the inline lifecycle. The recurring Services (which carry a
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
        "calibration_recalculate",
    ] {
        registry.register(Arc::new(super::jobs::ReprocessSensor::new(trigger)));
    }
    registry.register(Arc::new(super::jobs::RefreshAggregates::incremental()));
    registry.register(Arc::new(super::jobs::RefreshAggregates::full()));
    for trigger in ["sensor_swap", "pairing_backfill", "manual_adopt"] {
        registry.register(Arc::new(super::jobs::ReprocessSlot::new(trigger)));
    }
    for trigger in ["deployment_create", "deployment_update", "deployment_delete"] {
        registry.register(Arc::new(super::jobs::ReprocessDeployment::new(trigger)));
    }
    registry.register(Arc::new(super::jobs::DerivedRecompute));
    registry.register(Arc::new(super::jobs::DerivedAssignment));
    registry.register(Arc::new(super::jobs::IngestDerived));
    for trigger in ["compute_derived", "batch_derived"] {
        registry.register(Arc::new(super::jobs::SiteTimestampsDerived::new(trigger)));
    }
    registry.register(Arc::new(super::jobs::ReprocessAll));
    registry.register(Arc::new(super::jobs::CsvImport));
    registry.register(Arc::new(super::jobs::AlarmBackfill));
    registry.register(Arc::new(super::jobs::BackfillAttribution));
    registry.register(Arc::new(super::jobs::BackfillCalibrations));
    registry.register(Arc::new(super::jobs::MergeSiteParameters));
    registry.register(Arc::new(super::jobs::MergeParameters));
    registry.register(Arc::new(super::jobs::PlanApply));
    registry.register(Arc::new(super::jobs::PlanRevert));
    registry
}

/// Register the recurring Services (the former `main.rs` background loops) onto an existing registry,
/// each carrying its cadence from `Config`. Kept separate from [`build_registry`] so the cadence
/// dependency lives only on the `main.rs` startup path; the scheduler then seeds `schedules` rows
/// from `registry.default_schedules()`.
pub fn register_scheduled_services(registry: &mut JobRegistry, config: &crate::config::Config) {
    registry.register(Arc::new(super::jobs::JanitorRun::from_config(config)));
    registry.register(Arc::new(super::jobs::AlarmSweep::from_config(config)));
    registry.register(Arc::new(super::jobs::IdentityReconcile::from_config(config)));
    registry.register(Arc::new(super::jobs::NotifyHealth::from_config(config)));
    registry.register(Arc::new(super::jobs::DispatchNotifications::from_config(config)));
}

#[cfg(test)]
mod tests {
    use super::super::lifecycle::JobContext;
    use super::super::registry;
    use super::super::schedule::Schedule;
    use super::{Job, JobRegistry};
    use async_trait::async_trait;
    use std::sync::Arc;

    /// A handler used only to exercise registry mechanics — `run` is never called in these tests
    /// (it would need a live `JobContext`), so its body is a placeholder.
    struct Dummy {
        name: &'static str,
        schedule_secs: Option<i64>,
    }

    #[async_trait]
    impl Job for Dummy {
        fn name(&self) -> &'static str {
            self.name
        }
        fn default_schedule(&self) -> Option<Schedule> {
            self.schedule_secs.map(Schedule::every_secs)
        }
        async fn run(&self, _ctx: JobContext) -> Result<i64, sea_orm::DbErr> {
            Ok(0)
        }
    }

    fn dummy(name: &'static str, schedule_secs: Option<i64>) -> Arc<dyn Job> {
        Arc::new(Dummy { name, schedule_secs })
    }

    #[test]
    fn register_and_lookup() {
        let mut r = JobRegistry::new();
        r.register(dummy("manual_reprocess", None));
        r.register(dummy("janitor_run", Some(3600)));
        assert_eq!(r.len(), 2);
        assert!(r.get("manual_reprocess").is_some());
        assert!(r.get("unknown").is_none());
    }

    #[test]
    fn category_delegates_to_registry_table() {
        assert_eq!(dummy("janitor_run", None).category(), registry::CATEGORY_MAINTENANCE);
        assert_eq!(dummy("manual_reprocess", None).category(), registry::CATEGORY_OPERATOR);
        assert_eq!(dummy("calibration_create", None).category(), registry::CATEGORY_METADATA);
    }

    #[test]
    fn default_schedules_lists_only_recurring_services() {
        let mut r = JobRegistry::new();
        r.register(dummy("manual_reprocess", None));
        r.register(dummy("janitor_run", Some(3600)));
        let scheds: Vec<_> = r.default_schedules().collect();
        assert_eq!(scheds.len(), 1);
        assert_eq!(scheds[0].0, "janitor_run");
        assert_eq!(scheds[0].1.interval, chrono::Duration::seconds(3600));
    }

    #[test]
    #[should_panic(expected = "duplicate Job registration")]
    fn duplicate_registration_panics() {
        let mut r = JobRegistry::new();
        r.register(dummy("x", None));
        r.register(dummy("x", None));
    }
}
