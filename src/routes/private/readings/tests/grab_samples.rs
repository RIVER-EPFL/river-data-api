use super::{GrabFacts, StoredFacts, declared_instrument};

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
fn a_silent_field_keeps_what_the_group_carried() {
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
        merged.provenance,
        Some(serde_json::json!({ "tool": "doc" })),
        "a rewrite that names no run keeps the blob behind the value"
    );
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
    use super::super::{ExistingGroup, ExistingReplicate, stored_values_moved};
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
