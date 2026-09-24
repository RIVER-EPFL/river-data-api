use super::{GrabFacts, StoredFacts, declared_instrument, refuse_hand_save_over_calculated};

fn stored(label: &str, notes: &str, author: &str) -> StoredFacts {
    StoredFacts {
        created_by: Some(author.to_string()),
        label: Some(label.to_string()),
        notes: Some(notes.to_string()),
        provenance: Some(serde_json::json!({ "tool": "doc" })),
        kind: Some("tool_run".to_string()),
    }
}

#[test]
fn a_row_the_curve_did_not_correct_takes_the_slot_s_declaration() {
    let curve_instrument = uuid::Uuid::new_v4();
    let declared = uuid::Uuid::new_v4();
    assert_eq!(
        declared_instrument(Some(curve_instrument), Some(declared)),
        Some(curve_instrument),
        "the row carrying the curve names the instrument that curve was fitted on"
    );
    assert_eq!(
        declared_instrument(None, Some(declared)),
        Some(declared),
        "every other row names what the slot says measures it"
    );
    assert_eq!(
        declared_instrument(None, None),
        None,
        "an undeclared slot resolves nothing here; the entry channel's marker is added at the write"
    );
}

#[test]
fn a_silent_field_keeps_what_the_group_carried_but_its_run() {
    let prior = stored("batch 7", "filtered on site", "lab");
    let request = GrabFacts {
        created_by: None,
        label: None,
        notes: Some("corrected note"),
        provenance: None,
        kind: "manual",
    };
    let merged = request.over(Some(&prior));
    assert_eq!(merged.label.as_deref(), Some("batch 7"));
    assert_eq!(merged.notes.as_deref(), Some("corrected note"));
    assert_eq!(merged.created_by.as_deref(), Some("lab"));
    assert_eq!(
        merged.provenance, None,
        "a rewrite that names no run explains its value by no run"
    );
    assert_eq!(merged.kind.as_deref(), Some("manual"));
}

fn site() -> uuid::Uuid {
    uuid::Uuid::from_u128(7)
}

#[test]
fn test_refuse_hand_save_over_calculated_names_the_calculation() {
    let calculated = uuid::Uuid::new_v4();
    let typed = uuid::Uuid::new_v4();
    let writers = std::collections::HashMap::from([(calculated, "pco2".to_string())]);
    let err = refuse_hand_save_over_calculated(None, site(), &[typed, calculated], &writers)
        .expect_err("a hand value over a calculated parameter is refused");
    assert!(err.to_string().contains("pco2"), "{err}");
    assert!(err.to_string().contains(&calculated.to_string()), "{err}");
}

#[test]
fn test_refuse_hand_save_over_calculated_lets_a_run_save_its_outputs() {
    let calculated = uuid::Uuid::new_v4();
    let writers = std::collections::HashMap::from([(calculated, "pco2".to_string())]);
    assert!(
        refuse_hand_save_over_calculated(
            Some(uuid::Uuid::new_v4()),
            site(),
            &[calculated],
            &writers
        )
        .is_ok()
    );
}

#[test]
fn test_refuse_hand_save_over_calculated_lets_a_measurement_through() {
    let writers = std::collections::HashMap::from([(uuid::Uuid::new_v4(), "pco2".to_string())]);
    assert!(
        refuse_hand_save_over_calculated(None, site(), &[uuid::Uuid::new_v4()], &writers).is_ok()
    );
    assert!(refuse_hand_save_over_calculated(None, site(), &[], &writers).is_ok());
}

#[test]
fn a_first_write_carries_only_what_the_request_says() {
    let request = GrabFacts {
        created_by: Some("evan"),
        label: None,
        notes: None,
        provenance: None,
        kind: "manual",
    };
    let merged = request.over(None);
    assert_eq!(merged.created_by.as_deref(), Some("evan"));
    assert_eq!(merged.label, None);
    assert!(!merged.is_empty(), "an author alone is worth storing");
    assert!(
        GrabFacts {
            created_by: None,
            label: None,
            notes: None,
            provenance: None,
            kind: "manual",
        }
        .over(None)
        .is_empty(),
        "a request that records nothing writes nothing"
    );
}

mod moved {
    use super::super::{ExistingGroup, ExistingReplicate, entered_rows, stored_values_moved};
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 6, 20, 10, 30, 0).unwrap()
    }

    fn replicate(index: i16, raw_value: f64) -> ExistingReplicate {
        ExistingReplicate {
            replicate_index: index,
            raw_value,
            calibrated_value: None,
            standard_curve_id: None,
        }
    }

    fn group(parameter_id: Uuid, replicates: Vec<ExistingReplicate>) -> ExistingGroup {
        ExistingGroup {
            parameter_id,
            time: at(),
            replicates,
        }
    }

    #[test]
    fn only_a_new_or_changed_replicate_is_the_save_s_entry() {
        let doc = Uuid::new_v4();
        let ph = Uuid::new_v4();
        let carried = [
            (doc, at(), 0, 12.0),
            (doc, at(), 1, 13.0),
            (doc, at(), 2, 12.8),
            (ph, at(), 0, 7.1),
        ];
        let existing = [
            group(doc, vec![replicate(0, 12.0), replicate(1, 13.5)]),
            group(ph, vec![replicate(0, 7.1)]),
        ];
        // repeat 0 and pH carried as stored; repeat 1 changed; repeat 2 new
        assert_eq!(
            entered_rows(&carried, &existing),
            [false, true, true, false]
        );
    }

    #[test]
    fn a_new_replicate_beside_an_unchanged_one_moves_nothing() {
        let doc = Uuid::new_v4();
        let carried = [(doc, at(), 0, 12.0), (doc, at(), 1, 13.5)];
        let existing = [group(doc, vec![replicate(0, 12.0)])];
        assert_eq!(stored_values_moved(&carried, &existing), 0);
    }

    #[test]
    fn a_different_number_at_a_stored_index_moves_it() {
        let doc = Uuid::new_v4();
        let carried = [(doc, at(), 0, 12.5)];
        let existing = [group(doc, vec![replicate(0, 12.0)])];
        assert_eq!(stored_values_moved(&carried, &existing), 1);
    }

    #[test]
    fn a_stored_replicate_the_save_leaves_out_moves_because_the_replace_retracts_it() {
        let doc = Uuid::new_v4();
        let carried = [(doc, at(), 0, 12.0)];
        let existing = [group(doc, vec![replicate(0, 12.0), replicate(1, 13.0)])];
        assert_eq!(stored_values_moved(&carried, &existing), 1);
    }

    #[test]
    fn another_parameter_s_stored_replicate_is_not_this_group_s() {
        let doc = Uuid::new_v4();
        let ph = Uuid::new_v4();
        let carried = [(doc, at(), 0, 12.0)];
        let existing = [
            group(doc, vec![replicate(0, 12.0)]),
            group(ph, vec![replicate(0, 7.1)]),
        ];
        assert_eq!(
            stored_values_moved(&carried, &existing),
            1,
            "pH is stored and the save carries nothing for it, so the replace would retract it"
        );
    }
}

mod staleness {
    use super::super::{ExistingGroup, ExistingReplicate, ExpectedGroup, groups_changed};
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2025, 6, 20, 10, 30, 0).unwrap()
    }

    fn stored(parameter_id: Uuid, indices: &[i16]) -> ExistingGroup {
        ExistingGroup {
            parameter_id,
            time: at(),
            replicates: indices
                .iter()
                .map(|index| ExistingReplicate {
                    replicate_index: *index,
                    raw_value: 12.0,
                    calibrated_value: None,
                    standard_curve_id: None,
                })
                .collect(),
        }
    }

    fn read(parameter_id: Uuid, indices: &[i16]) -> ExpectedGroup {
        ExpectedGroup {
            parameter_id,
            time: at(),
            replicate_indices: indices.to_vec(),
        }
    }

    #[test]
    fn a_group_that_still_holds_what_was_read_has_not_changed() {
        let doc = Uuid::new_v4();
        assert!(groups_changed(&[read(doc, &[0, 1, 2])], &[stored(doc, &[0, 1, 2])]).is_empty());
    }

    #[test]
    fn a_repeat_added_underneath_the_client_is_named() {
        let doc = Uuid::new_v4();
        assert_eq!(
            groups_changed(&[read(doc, &[0, 1, 2])], &[stored(doc, &[0, 1, 2, 3])]),
            vec![(doc, at())]
        );
    }

    #[test]
    fn a_group_emptied_underneath_the_client_is_named() {
        let doc = Uuid::new_v4();
        assert_eq!(groups_changed(&[read(doc, &[0])], &[]), vec![(doc, at())]);
    }

    #[test]
    fn a_group_the_client_read_as_empty_and_still_is_has_not_changed() {
        let doc = Uuid::new_v4();
        assert!(groups_changed(&[read(doc, &[])], &[]).is_empty());
    }

    #[test]
    fn order_is_not_a_change() {
        let doc = Uuid::new_v4();
        assert!(groups_changed(&[read(doc, &[2, 0, 1])], &[stored(doc, &[0, 1, 2])]).is_empty());
    }
}

mod tool_link {
    use super::super::output_carries_value;
    use serde_json::json;

    #[test]
    fn test_a_scalar_output_carries_exactly_its_value() {
        assert!(output_carries_value(&json!(1.25), 1.25));
        assert!(!output_carries_value(&json!(1.25), 1.250_000_1));
    }

    #[test]
    fn test_a_replicate_shaped_output_carries_any_of_its_leaves() {
        let output = json!({ "values": [1.0, null, 3.5], "mean": 2.25 });
        assert!(output_carries_value(&output, 3.5));
        assert!(output_carries_value(&output, 2.25));
        assert!(!output_carries_value(&output, 2.0));
    }

    #[test]
    fn test_a_non_numeric_output_carries_no_value() {
        assert!(!output_carries_value(&json!("1.25"), 1.25));
        assert!(!output_carries_value(&json!(null), 0.0));
    }
}

mod steps {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::super::{
        ExistingGroup, GrabSampleReading, GrabWriteMode, grab_groups, grab_span, grab_writer,
        refuse_intern_rewrite, refuse_unasked_replace,
    };
    use crate::common::authz::Role;
    use crate::error::AppError;
    use crate::routes::private::collection_events::flows::Writer;

    fn at(hour: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 14, hour, 0, 0).unwrap()
    }

    fn reading(parameter: u128, hour: u32, value: f64) -> GrabSampleReading {
        serde_json::from_value(serde_json::json!({
            "parameter_id": Uuid::from_u128(parameter),
            "value": value,
            "time": at(hour),
        }))
        .expect("a grab reading")
    }

    fn stored(parameter: u128, hour: u32) -> ExistingGroup {
        ExistingGroup {
            parameter_id: Uuid::from_u128(parameter),
            time: at(hour),
            replicates: Vec::new(),
        }
    }

    #[test]
    fn test_grab_groups_names_each_parameter_and_instant_once_in_order() {
        let readings = [
            reading(1, 9, 1.0),
            reading(1, 9, 1.1),
            reading(2, 9, 5.0),
            reading(1, 10, 2.0),
        ];
        assert_eq!(
            grab_groups(&readings),
            vec![
                (Uuid::from_u128(1), at(9)),
                (Uuid::from_u128(2), at(9)),
                (Uuid::from_u128(1), at(10)),
            ]
        );
    }

    #[test]
    fn test_grab_span_covers_every_instant_and_is_none_for_nothing() {
        let readings = [reading(1, 11, 1.0), reading(2, 8, 1.0), reading(1, 9, 1.0)];
        assert_eq!(grab_span(&readings), Some((at(8), at(11))));
        assert_eq!(grab_span(&[]), None);
    }

    #[test]
    fn test_only_the_chains_own_save_is_the_chain() {
        assert_eq!(grab_writer(Some("chain")), Writer::Chain);
        assert_eq!(grab_writer(Some("interactive")), Writer::Person);
        assert_eq!(grab_writer(None), Writer::Person);
    }

    #[test]
    fn test_a_save_on_stored_groups_rewrites_them_only_when_it_says_replace() {
        let existing = [stored(1, 9)];
        assert!(matches!(
            refuse_unasked_replace(None, &existing),
            Err(AppError::ConflictDetail { .. })
        ));
        assert!(refuse_unasked_replace(Some(GrabWriteMode::Replace), &existing).is_ok());
        assert!(
            refuse_unasked_replace(None, &[]).is_ok(),
            "nothing stored, nothing to refuse"
        );
    }

    #[test]
    fn test_only_an_intern_replace_is_held_to_the_stored_values() {
        let carried = [(Uuid::from_u128(1), at(9), 0, 7.5)];
        assert!(
            refuse_intern_rewrite(
                Some(&Role::Manager),
                Some(GrabWriteMode::Replace),
                &carried,
                &[]
            )
            .is_ok()
        );
        assert!(refuse_intern_rewrite(Some(&Role::Intern), None, &carried, &[]).is_ok());
        assert!(
            refuse_intern_rewrite(
                Some(&Role::Intern),
                Some(GrabWriteMode::Replace),
                &carried,
                &[]
            )
            .is_ok(),
            "an intern's replace that moves no stored value is an entry"
        );
    }
}
