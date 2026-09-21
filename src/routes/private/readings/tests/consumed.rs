use chrono::{TimeZone, Utc};
use uuid::Uuid;

use super::super::models::{ConsumedInput, ConsumedReading};
use super::{CHANGED, Standing, UNCHANGED, UNKNOWN, combine, mark, subject_ids};

#[test]
fn test_a_source_at_the_consumed_revision_is_unchanged() {
    assert_eq!(mark(Some(7), Standing::At(Some(7))), UNCHANGED);
    // The arrival state on both sides: a row no decision had touched, and still none has.
    assert_eq!(mark(None, Standing::At(None)), UNCHANGED);
}

#[test]
fn test_a_moved_revision_is_changed() {
    assert_eq!(mark(Some(7), Standing::At(Some(8))), CHANGED);
    // A row consumed at its arrival state that has since been decided on.
    assert_eq!(mark(None, Standing::At(Some(1))), CHANGED);
    // A rollback restores the value and still advances the revision.
    assert_eq!(mark(Some(9), Standing::At(Some(11))), CHANGED);
}

#[test]
fn test_a_source_that_no_longer_answers_is_changed() {
    assert_eq!(mark(Some(7), Standing::Gone), CHANGED);
    assert_eq!(mark(None, Standing::Gone), CHANGED);
}

#[test]
fn test_an_input_with_no_source_is_unknown_rather_than_assumed_to_have_held_still() {
    assert_eq!(mark(None, Standing::Unsourced), UNKNOWN);
    assert_eq!(mark(Some(7), Standing::Unsourced), UNKNOWN);
}

#[test]
fn test_one_moved_member_changes_the_input() {
    assert_eq!(combine(&[UNCHANGED, CHANGED, UNCHANGED]), CHANGED);
    assert_eq!(combine(&[UNCHANGED, UNCHANGED]), UNCHANGED);
    // Changed outranks unknown: something is known to have moved.
    assert_eq!(combine(&[UNKNOWN, CHANGED]), CHANGED);
    assert_eq!(combine(&[UNCHANGED, UNKNOWN]), UNKNOWN);
}

#[test]
fn test_an_input_behind_nothing_at_all_is_unknown() {
    assert_eq!(combine(&[]), UNKNOWN);
}

#[test]
fn test_a_subject_is_read_by_its_own_prefix_only() {
    let constant = Uuid::new_v4();
    let subjects = vec![
        format!("constant:{constant}"),
        format!("site:{}", Uuid::new_v4()),
        "constant:not-a-uuid".to_string(),
    ];
    assert_eq!(subject_ids(&subjects, "constant:"), vec![constant]);
    assert_eq!(subject_ids(&subjects, "standard_curve:").len(), 0);
}

#[test]
fn test_a_capture_round_trips_through_the_stored_json() {
    let input = ConsumedInput {
        variable: "WTW_Temp_degC_1".to_string(),
        kind: "mean".to_string(),
        subject: None,
        property: None,
        revision: None,
        members: vec![ConsumedReading {
            stream_id: Uuid::new_v4(),
            time: Utc.with_ymd_and_hms(2026, 8, 2, 10, 0, 0).unwrap(),
            replicate_index: 1,
            revision: Some(4),
            value: Some(7.6),
        }],
        value: serde_json::json!(7.6),
    };
    let stored = serde_json::to_value(&input).unwrap();
    let back: ConsumedInput = serde_json::from_value(stored).unwrap();
    assert_eq!(back, input);
}
