use sea_orm::ActiveValue::{NotSet, Set};

/// The arrival stamp is the database's, and a fresh row carries no curation, no event and no
/// origin blob. Every write path builds on these, so they are stated once.
#[test]
fn test_a_new_reading_is_uncurated_and_unstamped() {
    let model = super::new(uuid::Uuid::nil(), chrono::Utc::now().into(), 2, 7.5);
    assert!(matches!(model.ingested_at, NotSet));
    assert_eq!(model.replicate_index, Set(2));
    assert_eq!(model.raw_value, Set(7.5));
    assert_eq!(model.is_flagged, Set(Some(false)));
    assert_eq!(model.flag_reason, Set(None));
    assert_eq!(model.withdrawn_at, Set(None));
    assert_eq!(model.withdrawn_reason, Set(None));
    assert_eq!(model.collection_event_id, Set(None));
    assert_eq!(model.sample_id, Set(None));
    assert_eq!(model.provenance, Set(None));
    assert_eq!(model.provenance_kind, Set(None));
    assert_eq!(model.label, Set(None));
    assert_eq!(model.notes, Set(None));
    assert_eq!(model.created_by, Set(None));
    assert_eq!(model.calibrated_value, Set(None));
    assert_eq!(model.standard_curve_id, Set(None));
}

mod assertion_over {
    use super::super::{EditDecision, EditOption, Kind, Selection, SelectionKey};

    fn decision(value: Option<f64>) -> EditDecision {
        EditDecision {
            kind: "value_correction".to_string(),
            value,
            target_id: None,
            reason: None,
        }
    }

    fn key(value: Option<f64>) -> SelectionKey {
        SelectionKey {
            stream_id: uuid::Uuid::nil(),
            time: chrono::Utc::now(),
            replicate_index: Some(0),
            value,
        }
    }

    #[test]
    fn test_a_correction_naming_a_value_per_key_asserts_nothing_at_set_level() {
        let selection = Selection {
            keys: vec![key(Some(1.5)), key(Some(2.5))],
            ..Default::default()
        };
        let (new, option) = decision(None)
            .assertion_over(Kind::ValueCorrection, &selection)
            .expect("keyed");
        assert_eq!(new, serde_json::json!({}));
        assert_eq!(option, EditOption::ValueCorrection);
    }

    #[test]
    fn test_a_single_correction_asserts_its_value() {
        let (new, _) = decision(Some(4.0))
            .assertion_over(Kind::ValueCorrection, &Selection::default())
            .expect("one value");
        assert_eq!(new, serde_json::json!({ "raw_value": 4.0 }));
    }

    #[test]
    fn test_a_correction_with_no_value_anywhere_is_refused() {
        assert!(
            decision(None)
                .assertion_over(Kind::ValueCorrection, &Selection::default())
                .is_err()
        );
    }

    #[test]
    fn test_a_selection_giving_values_to_some_keys_only_is_refused() {
        let selection = Selection {
            keys: vec![key(Some(1.5)), key(None)],
            ..Default::default()
        };
        assert!(
            decision(None)
                .assertion_over(Kind::ValueCorrection, &selection)
                .is_err()
        );
    }
}
