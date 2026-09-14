use super::{
    PROVENANCE_KINDS, classify_source, provenance_kind_for_run, provenance_kind_for_stream,
};

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
