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

/// Expected behaviour: the closing line a walk writes names what moved and over how many, and
/// mentions failures only when there were some, so a clean run reads as a clean run.
#[test]
fn test_the_closing_line_names_the_failures_only_when_there_are_some() {
    let clean = SlotOutcome::from([
        (serde_json::json!({ "slot": 1 }), Ok(12_000)),
        (serde_json::json!({ "slot": 2 }), Ok(3)),
    ]);
    assert_eq!(clean.line("slots"), "Moved 12003 readings across 2 slots");

    let partial = SlotOutcome::from([
        (serde_json::json!({ "slot": 1 }), Ok(5)),
        (
            serde_json::json!({ "slot": 2 }),
            Err(sea_orm::DbErr::Custom("no".to_string())),
        ),
    ]);
    assert_eq!(
        partial.line("instruments"),
        "Moved 5 readings across 1 instruments, 1 failed"
    );
}
