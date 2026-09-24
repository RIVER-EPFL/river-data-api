use chrono::{TimeZone, Utc};
use uuid::Uuid;

use super::super::models::{ConsumedInput, ConsumedReading};
use super::{CHANGED, Standing, UNCHANGED, UNKNOWN, as_first_read, combine, mark, subject_ids};

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

/// Expected behaviour: a record captured while a source could be held (Q230, withdrawn by Q252)
/// still reads back, the rule it named dropped.
#[test]
fn test_a_capture_naming_a_hold_still_reads() {
    let stored = serde_json::json!({
        "variable": "alkalinity",
        "alignment": "hold",
        "kind": "reading",
        "value": 7.0,
    });
    let back: ConsumedInput = serde_json::from_value(stored).unwrap();
    assert_eq!(back.variable, "alkalinity");
    assert!(
        serde_json::to_value(&back)
            .unwrap()
            .get("alignment")
            .is_none()
    );
}

fn captured(
    variable: &str,
    subject: Option<&str>,
    revision: Option<i64>,
    value: f64,
) -> ConsumedInput {
    ConsumedInput {
        variable: variable.to_string(),
        kind: "constant".to_string(),
        subject: subject.map(str::to_string),
        property: None,
        revision,
        members: Vec::new(),
        value: serde_json::json!(value),
    }
}

#[test]
fn test_a_recaptured_subject_keeps_what_the_first_computation_read() {
    let subject = format!("constant:{}", Uuid::new_v4());
    let newest = vec![captured("k", Some(&subject), Some(827), 3.0)];
    let first = vec![captured("k", Some(&subject), Some(826), 2.0)];
    let reported = as_first_read(newest, &first);
    assert_eq!(reported[0].revision, Some(826));
    assert_eq!(reported[0].value.as_f64(), Some(2.0));
}

#[test]
fn test_an_input_with_no_subject_follows_the_newest_capture() {
    let newest = vec![captured("Dissolved_O2", None, Some(9), 11.0)];
    let first = vec![captured("Dissolved_O2", None, Some(8), 10.0)];
    let reported = as_first_read(newest, &first);
    assert_eq!(reported[0].revision, Some(9));
    assert_eq!(reported[0].value.as_f64(), Some(11.0));
}

#[test]
fn test_a_subject_the_first_computation_did_not_read_stands_as_captured() {
    let subject = format!("constant:{}", Uuid::new_v4());
    let other = format!("constant:{}", Uuid::new_v4());
    let newest = vec![captured("k", Some(&subject), Some(827), 3.0)];
    // The formula changed: `k` now binds a different constant, and the same name under the other
    // subject is not the value this input was read from.
    let first = vec![captured("k", Some(&other), Some(400), 9.0)];
    let reported = as_first_read(newest, &first);
    assert_eq!(reported[0].revision, Some(827));
    assert_eq!(reported[0].value.as_f64(), Some(3.0));
}

#[test]
fn test_an_edited_step_follows_the_capture_the_recompute_made() {
    let subject = format!("calculation_formula:{}", Uuid::new_v4());
    let mut newest = captured("hs_k", Some(&subject), Some(674), 0.0);
    newest.kind = "step".to_string();
    newest.value = serde_json::json!("Dissolved_O2 * 3");
    let mut first = captured("hs_k", Some(&subject), Some(672), 0.0);
    first.kind = "step".to_string();
    first.value = serde_json::json!("Dissolved_O2 * 2");
    let reported = as_first_read(vec![newest], &[first]);
    assert_eq!(reported[0].revision, Some(674));
    assert_eq!(reported[0].value, serde_json::json!("Dissolved_O2 * 3"));
}

#[test]
fn test_a_key_computed_once_reports_that_computation() {
    let subject = format!("constant:{}", Uuid::new_v4());
    let only = vec![captured("k", Some(&subject), Some(826), 2.0)];
    let reported = as_first_read(only.clone(), &only);
    assert_eq!(reported[0].revision, Some(826));
    assert_eq!(reported[0].value.as_f64(), Some(2.0));
}

mod resolve {
    use std::collections::HashMap;

    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::super::super::models::{ConsumedInput, ConsumedReading};
    use super::super::{CHANGED, CurrentReading, UNCHANGED, UNKNOWN, mark_of, resolve_one};

    fn input(members: Vec<ConsumedReading>, subject: Option<&str>) -> ConsumedInput {
        ConsumedInput {
            variable: "doc".to_string(),
            kind: "parameter".to_string(),
            subject: subject.map(str::to_string),
            property: None,
            revision: Some(4),
            members,
            value: serde_json::json!(2.0),
        }
    }

    fn member(index: i16, revision: i64) -> ConsumedReading {
        ConsumedReading {
            stream_id: Uuid::nil(),
            time: Utc.with_ymd_and_hms(2025, 6, 1, 8, 0, 0).unwrap(),
            replicate_index: index,
            revision: Some(revision),
            value: Some(2.0),
        }
    }

    fn standing(revision: i64, value: f64) -> CurrentReading {
        CurrentReading {
            revision: Some(revision),
            value: Some(value),
            point: None,
        }
    }

    #[test]
    fn test_a_single_member_carries_its_current_value() {
        let m = member(0, 4);
        let current = HashMap::from([((m.stream_id, m.time, 0), standing(4, 2.0))]);
        let r = resolve_one(
            &input(vec![m], None),
            &current,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(r.state, UNCHANGED);
        assert_eq!(r.current_value, Some(serde_json::json!(2.0)));
    }

    #[test]
    fn test_a_statistic_over_several_members_carries_no_current_number() {
        let (a, b) = (member(0, 4), member(1, 4));
        let current = HashMap::from([
            ((a.stream_id, a.time, 0), standing(4, 2.0)),
            ((b.stream_id, b.time, 1), standing(5, 9.0)),
        ]);
        let r = resolve_one(
            &input(vec![a, b], None),
            &current,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(r.state, CHANGED, "one member moved");
        assert_eq!(r.current_value, None);
        assert_eq!(r.members[1].current_value, Some(9.0));
    }

    #[test]
    fn test_an_input_with_no_members_and_no_subject_is_unknown() {
        let r = resolve_one(
            &input(vec![], None),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(r.state, UNKNOWN);
    }

    #[test]
    fn test_a_subject_the_catalog_no_longer_holds_is_changed() {
        let r = resolve_one(
            &input(vec![], Some("constants:1")),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(r.state, CHANGED);
        assert_eq!(r.current_revision, None);
    }

    #[test]
    fn test_a_subject_at_the_revision_consumed_is_unchanged() {
        let revisions = HashMap::from([("constants:1".to_string(), 4)]);
        let r = resolve_one(
            &input(vec![], Some("constants:1")),
            &HashMap::new(),
            &revisions,
            &HashMap::new(),
        );
        assert_eq!(r.state, UNCHANGED);
    }

    #[test]
    fn test_a_mark_read_back_is_one_of_the_three() {
        assert_eq!(mark_of("changed"), CHANGED);
        assert_eq!(mark_of("unchanged"), UNCHANGED);
        assert_eq!(mark_of("unknown"), UNKNOWN);
        assert_eq!(mark_of("stale"), UNKNOWN);
    }
}

mod ruling {
    use std::collections::{HashMap, HashSet};

    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::super::{ReadingKey, follow_ruling, inputs_pending};

    fn key(n: u128) -> ReadingKey {
        (
            Uuid::from_u128(n),
            Utc.with_ymd_and_hms(2025, 6, 15, 9, 0, 0).unwrap(),
            0,
        )
    }

    /// DIC (1) and temperature (2) are entered; pCO2 (10) reads both; CO2 flux (20) reads pCO2.
    fn chain() -> HashMap<ReadingKey, Vec<ReadingKey>> {
        HashMap::from([(key(10), vec![key(1), key(2)]), (key(20), vec![key(10)])])
    }

    #[test]
    fn test_follow_ruling_holds_an_output_while_another_input_is_pending() {
        let pending: HashSet<ReadingKey> = [key(1), key(2), key(10), key(20)].into();
        assert!(follow_ruling(&chain(), &pending, &[key(1)], true).is_empty());
    }

    #[test]
    fn test_follow_ruling_releases_through_the_chain_on_the_last_input() {
        let pending: HashSet<ReadingKey> = [key(2), key(10), key(20)].into();
        assert_eq!(
            follow_ruling(&chain(), &pending, &[key(2)], true),
            vec![key(10), key(20)]
        );
    }

    #[test]
    fn test_follow_ruling_releases_nothing_already_released_or_reading_nothing() {
        let outputs = HashMap::from([(key(10), vec![key(1)]), (key(30), Vec::new())]);
        let pending: HashSet<ReadingKey> = [key(1), key(30)].into();
        // 10 was not pending, so there is nothing to release; 30 read nothing, so no ruling frees it.
        assert!(follow_ruling(&outputs, &pending, &[key(1)], true).is_empty());
    }

    #[test]
    fn test_inputs_pending_names_what_a_held_output_still_waits_on() {
        let pending: HashSet<ReadingKey> = [key(1), key(10), key(20)].into();
        assert_eq!(inputs_pending(&chain(), &pending, &[key(10)]), vec![key(1)]);
        // Flux waits on pCO2, which is itself pending.
        assert_eq!(
            inputs_pending(&chain(), &pending, &[key(20)]),
            vec![key(10)]
        );
    }

    #[test]
    fn test_inputs_pending_is_empty_for_an_entry_or_an_output_whose_inputs_are_verified() {
        let pending: HashSet<ReadingKey> = [key(1), key(10)].into();
        // DIC was entered, not computed.
        assert!(inputs_pending(&chain(), &pending, &[key(1)]).is_empty());
        let pending: HashSet<ReadingKey> = [key(10)].into();
        assert!(inputs_pending(&chain(), &pending, &[key(10)]).is_empty());
    }

    #[test]
    fn test_follow_ruling_reject_takes_every_output_downstream() {
        let pending: HashSet<ReadingKey> = [key(1), key(10), key(20)].into();
        assert_eq!(
            follow_ruling(&chain(), &pending, &[key(1)], false),
            vec![key(10), key(20)]
        );
        // An input nothing consumed takes nothing with it.
        assert!(follow_ruling(&chain(), &pending, &[key(3)], false).is_empty());
    }
}
