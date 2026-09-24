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
    // The services built from Config cannot be constructed here; `register_scheduled_services`
    // asserts it registered exactly the names below, so taking them from the const is not taking
    // them on trust.
    let registered: std::collections::HashSet<&str> = r
        .names()
        .chain(CONFIG_BUILT_SERVICE_NAMES.iter().copied())
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

#[test]
fn test_is_retry_budget_spent_at_and_past_the_limit() {
    assert!(!is_retry_budget_spent(0, 3));
    assert!(!is_retry_budget_spent(2, 3));
    assert!(is_retry_budget_spent(3, 3));
    assert!(is_retry_budget_spent(7, 3));
    // No retries configured: the first orphaning is final
    assert!(is_retry_budget_spent(0, 0));
}

/// Expected behaviour: a walk's position and its length reach the columns as they are, and a
/// collection longer than the columns can hold still reports a truthful "at least this far"
/// rather than wrapping into a negative.
#[test]
fn a_walk_is_counted_into_the_progress_columns_and_never_wraps() {
    assert_eq!(as_progress(0), 0);
    assert_eq!(as_progress(8), 8);
    assert_eq!(as_progress(usize::MAX), i32::MAX);
}

/// Expected behaviour: the length reaches the row on the first item and the bar ends full, and a
/// long walk reports a hundred times rather than once per row.
#[test]
fn a_walk_reports_its_ends_and_a_hundredth_of_what_is_between() {
    assert!(reports_step(1, 8), "the first item carries the length");
    assert!(reports_step(8, 8), "the last item fills the bar");
    for done in 2..8 {
        assert!(reports_step(done, 8), "a short walk reports every item");
    }

    // A hundred steps of a thousand, plus the first item, which carries the length.
    let reported = (1..=100_000).filter(|d| reports_step(*d, 100_000)).count();
    assert_eq!(
        reported, 101,
        "a long walk reports a hundred steps, not a hundred thousand"
    );
    assert!(reports_step(1, 100_000));
    assert!(reports_step(100_000, 100_000));
}

#[test]
fn test_site_of_reads_the_site_the_params_name() {
    let site = Uuid::from_u128(3);
    assert_eq!(site_of(&serde_json::json!({ "site_id": site })), Some(site));
    assert_eq!(site_of(&serde_json::json!({ "calculation": "doc" })), None);
    assert_eq!(site_of(&serde_json::json!({ "site_id": null })), None);
    assert_eq!(
        site_of(&serde_json::json!({ "site_id": "not a uuid" })),
        None
    );
}
