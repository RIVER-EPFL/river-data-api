use super::*;
use async_trait::async_trait;
use std::sync::Arc;

/// A handler used only to exercise registry mechanics, `run` is never called in these tests
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
    Arc::new(Dummy {
        name,
        schedule_secs,
    })
}

#[test]
fn register_and_lookup() {
    let mut r = JobRegistry::new();
    r.register(dummy("manual_reprocess", None));
    r.register(dummy("janitor_service", Some(3600)));
    assert_eq!(r.len(), 2);
    assert!(r.get("manual_reprocess").is_some());
    assert!(r.get("unknown").is_none());
}

#[test]
fn category_delegates_to_registry_table() {
    assert_eq!(
        dummy("janitor_service", None).category(),
        CATEGORY_MAINTENANCE
    );
    assert_eq!(
        dummy("manual_reprocess", None).category(),
        CATEGORY_OPERATOR
    );
    assert_eq!(
        dummy("calibration_create", None).category(),
        CATEGORY_METADATA
    );
}

#[test]
fn default_schedules_lists_only_recurring_services() {
    let mut r = JobRegistry::new();
    r.register(dummy("manual_reprocess", None));
    r.register(dummy("janitor_service", Some(3600)));
    let scheds: Vec<_> = r.default_schedules().collect();
    assert_eq!(scheds.len(), 1);
    assert_eq!(scheds[0].0, "janitor_service");
    assert_eq!(scheds[0].1.interval, chrono::Duration::seconds(3600));
}

/// Scenario: a policy table names a `trigger_type` that no handler registers under.
///
/// Expected behaviour: none do. `is_cancellable` and `is_rerunnable` are keyed on the name a
/// job answers to, so a stale spelling silently withdraws the policy from the job it was
/// written for, and nothing else reports it.
#[test]
fn every_policy_name_is_a_registered_job() {
    let r = build_registry();
    // The recurring services need a Config to build; `register_scheduled_services` asserts it
    // registered exactly the names below, so taking them from the const is not taking them on
    // trust.
    let registered: std::collections::HashSet<&str> = r
        .names()
        .chain(SCHEDULED_SERVICE_NAMES.iter().copied())
        .collect();

    let policy_names = MAINTENANCE
        .iter()
        .chain(METADATA)
        .chain(RERUNNABLE)
        .chain(CANCELLABLE);
    for name in policy_names {
        assert!(
            registered.contains(name),
            "policy table names {name:?}, which no job registers under"
        );
    }
}

#[test]
#[should_panic(expected = "duplicate Job registration")]
fn duplicate_registration_panics() {
    let mut r = JobRegistry::new();
    r.register(dummy("x", None));
    r.register(dummy("x", None));
}
