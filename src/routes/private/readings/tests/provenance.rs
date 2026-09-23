use chrono::{TimeZone, Utc};
use sea_orm::sea_query::PostgresQueryBuilder;
use uuid::Uuid;

use super::{
    PROVENANCE_KINDS, classify_source, decommission_of, provenance_kind_for_run,
    provenance_kind_for_stream, slot_holds_query, stream_holds_query,
};

#[test]
fn test_a_hold_on_a_record_names_its_calculation() {
    let at = Utc.with_ymd_and_hms(2026, 8, 2, 10, 0, 0).unwrap();
    let slot = slot_holds_query(Uuid::nil(), &[Uuid::nil()], at).to_string(PostgresQueryBuilder);
    let stream = stream_holds_query(&[Uuid::nil()], at).to_string(PostgresQueryBuilder);
    assert!(slot.contains("\"tool\""), "{slot}");
    assert!(stream.contains("\"tool\""), "{stream}");
}

#[test]
fn test_a_computed_value_is_not_classified_as_a_sync() {
    assert_eq!(classify_source("derived"), "derived");
    assert_eq!(classify_source("cnet"), "sync");
    assert_eq!(classify_source("grab_sample"), "manual");
}

#[test]
fn test_provenance_kind_for_stream_matches_the_trigger_rule() {
    assert_eq!(
        provenance_kind_for_stream(Some("derived"), "cnet"),
        "derived"
    );
    assert_eq!(
        provenance_kind_for_stream(Some("spot"), "grab_sample"),
        "manual"
    );
    assert_eq!(provenance_kind_for_stream(None, "api"), "batch");
    assert_eq!(
        provenance_kind_for_stream(Some("continuous"), "vaisala"),
        "sync"
    );
}

#[test]
fn test_a_source_the_rule_does_not_name_is_a_sync() {
    assert_eq!(provenance_kind_for_stream(None, "portal"), "sync");
    assert_eq!(provenance_kind_for_stream(Some("spot"), "nomis"), "sync");
}

#[test]
fn test_provenance_kind_for_run_follows_the_minting_path() {
    assert_eq!(provenance_kind_for_run(None), "manual");
    assert_eq!(provenance_kind_for_run(Some("interactive")), "tool_run");
    assert_eq!(provenance_kind_for_run(Some("chain")), "chain");
    assert_eq!(provenance_kind_for_run(Some("csv_import")), "csv_import");
}

#[test]
fn test_every_kind_a_writer_can_stamp_is_a_declared_kind() {
    for kind in [
        provenance_kind_for_stream(Some("derived"), "cnet"),
        provenance_kind_for_stream(None, "grab_sample"),
        provenance_kind_for_stream(None, "api"),
        provenance_kind_for_stream(None, "vaisala"),
        provenance_kind_for_run(None),
        provenance_kind_for_run(Some("interactive")),
        provenance_kind_for_run(Some("chain")),
        provenance_kind_for_run(Some("csv_import")),
    ] {
        assert!(
            PROVENANCE_KINDS.contains(&kind),
            "{kind} is not a declared kind"
        );
    }
}

mod saved_inputs {
    use super::super::check_saved_input;
    use std::collections::HashSet;

    fn bound(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn test_a_replicate_is_checked_against_the_position_the_run_consumed() {
        let inputs = serde_json::json!({ "doc": [120.0, 122.0] });
        let none = bound(&[]);
        assert!(check_saved_input("doc", &inputs, &none, "doc", 122.0, Some(1)).is_ok());
        let err = check_saved_input("doc", &inputs, &none, "doc", 120.0, Some(1)).unwrap_err();
        assert!(err.contains("consumed at replicate 1"), "{err}");
        let err = check_saved_input("doc", &inputs, &none, "doc", 120.0, None).unwrap_err();
        assert!(err.contains("needs the replicate_index"), "{err}");
    }

    #[test]
    fn test_a_typed_over_event_input_is_saved_as_one_measurement() {
        let inputs = serde_json::json!({ "temp": 14.0 });
        let temp = bound(&["temp"]);
        assert!(check_saved_input("pco2", &inputs, &temp, "temp", 14.0, Some(0)).is_ok());
        assert!(check_saved_input("pco2", &inputs, &temp, "temp", 14.0, None).is_ok());
        // The stored number is the one the run computed from.
        let err = check_saved_input("pco2", &inputs, &temp, "temp", 10.0, Some(0)).unwrap_err();
        assert!(err.contains("is not what this pco2 run consumed"), "{err}");
        let err = check_saved_input("pco2", &inputs, &temp, "temp", 14.0, Some(1)).unwrap_err();
        assert!(err.contains("saved at replicate 0"), "{err}");
    }

    #[test]
    fn test_a_numeric_setting_the_manifest_binds_to_nothing_is_not_saveable() {
        let inputs = serde_json::json!({ "volume": 40.0 });
        let err = check_saved_input("doc", &inputs, &bound(&["temp"]), "volume", 40.0, Some(0))
            .unwrap_err();
        assert!(err.contains("is a setting of this doc run"), "{err}");
    }

    #[test]
    fn test_a_name_the_run_never_carried_is_refused() {
        let inputs = serde_json::json!({ "doc": [1.0] });
        let err =
            check_saved_input("doc", &inputs, &bound(&["temp"]), "temp", 1.0, Some(0)).unwrap_err();
        assert!(err.contains("is not a replicates input"), "{err}");
    }
}

mod bound_parameters {
    use super::super::check_bound_parameter;
    use uuid::Uuid;

    const DOC_PPB: Uuid = Uuid::from_u128(1);
    const TEMP: Uuid = Uuid::from_u128(2);

    #[test]
    fn test_a_replicate_saved_under_another_parameter_is_refused_naming_both() {
        let err = check_bound_parameter("doc", "input", "DOC", Some((DOC_PPB, "DOC_ppb")), TEMP)
            .unwrap_err();
        assert!(err.contains("DOC_ppb"), "{err}");
        assert!(err.contains(&TEMP.to_string()), "{err}");
    }

    #[test]
    fn test_a_reading_of_the_bound_parameter_is_accepted() {
        assert!(
            check_bound_parameter("doc", "input", "DOC", Some((DOC_PPB, "DOC_ppb")), DOC_PPB)
                .is_ok()
        );
    }

    #[test]
    fn test_an_output_saved_under_another_parameter_is_refused() {
        let err = check_bound_parameter("doc", "output", "suva", Some((DOC_PPB, "DOC_ppb")), TEMP)
            .unwrap_err();
        assert!(err.contains("output 'suva'"), "{err}");
    }

    #[test]
    fn test_a_name_the_manifest_binds_to_nothing_keeps_the_requested_parameter() {
        assert!(check_bound_parameter("doc", "output", "suva", None, TEMP).is_ok());
    }
}

#[test]
fn test_a_decommission_is_reported_only_when_the_calculation_records_one() {
    let at = Utc.with_ymd_and_hms(2026, 9, 23, 10, 0, 0).unwrap();
    let decommission = decommission_of(
        Some(at),
        Some("evan".to_string()),
        Some("replaced by pco2_v2".to_string()),
    )
    .expect("a decommissioned calculation reports it");
    assert_eq!(decommission.at, at);
    assert_eq!(decommission.by, "evan");
    assert_eq!(decommission.reason, "replaced by pco2_v2");
    assert_eq!(
        decommission_of(None, None, None),
        None,
        "a live calculation reports none"
    );
}
