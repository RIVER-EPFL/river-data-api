use super::SlotOutcome;
use sea_orm::DbErr;

fn slot(n: u32) -> serde_json::Value {
    serde_json::json!({ "site_id": n })
}

#[test]
fn a_failed_slot_is_named_and_the_rest_still_count() {
    let outcome = SlotOutcome::from(vec![
        (slot(1), Ok(4)),
        (slot(2), Err(DbErr::Custom("lock timeout".into()))),
        (slot(3), Ok(6)),
    ]);

    assert_eq!(outcome.succeeded, 2);
    assert_eq!(outcome.readings, 10);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, slot(2));
    assert!(outcome.failed[0].1.contains("lock timeout"));
    assert!(!outcome.all_failed());
}

#[test]
fn every_slot_failing_is_a_failed_run() {
    let outcome = SlotOutcome::from(vec![
        (slot(1), Err(DbErr::Custom("a".into()))),
        (slot(2), Err(DbErr::Custom("b".into()))),
    ]);

    assert_eq!(outcome.readings, 0);
    assert!(outcome.all_failed());
    assert!(outcome.error().to_string().contains('2'));
}

#[test]
fn an_empty_slot_set_is_not_a_failure() {
    let outcome = SlotOutcome::from(Vec::new());

    assert_eq!(outcome.succeeded, 0);
    assert_eq!(outcome.readings, 0);
    assert!(!outcome.all_failed());
}
