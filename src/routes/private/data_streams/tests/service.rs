use super::*;
use crate::routes::private::data_streams::models::ColumnAssignment;

#[test]
fn nomis_is_refused_and_says_why() {
    let reason = pairing_refusal("nomis").expect("NOMIS is refused");
    assert!(reason.contains("no zone"), "{reason}");
    assert!(reason.contains("ADR 0004"), "{reason}");
    assert!(
        pairing_refusal("NOMIS").is_some(),
        "the source system is compared without regard to case"
    );
}

#[test]
fn every_other_source_pairs() {
    for source in ["cnet", "metalp", "vaisala", "api", "grab_sample"] {
        assert!(pairing_refusal(source).is_none(), "{source} pairs");
    }
}

fn declaration(columns: &[&str]) -> river_data_core::models::ReplicateSpec {
    river_data_core::models::ReplicateSpec {
        source_columns: columns.iter().map(ToString::to_string).collect(),
        portal_mean_column: Some("DOC_avg_ppb".to_string()),
        portal_sd_column: Some("DOC_sd_ppb".to_string()),
        curve_ref_column: Some("doc_std_curve_id".to_string()),
        calc: Some("calcDOCavg".to_string()),
        sd_estimator: None,
    }
}

fn spec(columns: &[&str]) -> ReplicateSpec {
    ReplicateSpec {
        declared: declaration(columns),
        assignments: Vec::new(),
    }
}

#[test]
fn a_valid_spec_roundtrips_through_metadata() {
    let s = spec(&["DOC_rep_1", "DOC_rep_2", "DOC_rep_3"]);
    validate_declaration(&s.declared, Some("spot")).unwrap();
    let mut metadata = serde_json::json!({"hierarchy": {"site": "DGT"}});
    s.embed(&mut metadata).unwrap();
    let parsed = ReplicateSpec::from_metadata(&metadata).unwrap();
    assert_eq!(parsed.declared.source_columns, s.declared.source_columns);
    assert_eq!(
        parsed.declared.portal_mean_column.as_deref(),
        Some("DOC_avg_ppb")
    );
    assert_eq!(metadata["hierarchy"]["site"], "DGT");
}

#[test]
fn a_single_member_is_refused() {
    assert!(validate_declaration(&declaration(&["DOC_rep_1"]), Some("spot")).is_err());
}

#[test]
fn duplicate_members_are_refused() {
    assert!(validate_declaration(&declaration(&["DOC_rep_1", "DOC_rep_1"]), Some("spot")).is_err());
}

/// A spec stored before pinning resolves to each declared column at its
/// position, which is the index its readings were stored under. The sync
/// client reads the same metadata through `ColumnAssignment::from_metadata`
/// in `river-data-core` and must land on this same mapping, or a legacy
/// stream's replicates are indexed one way on write and another on read.
#[test]
fn an_unpinned_spec_resolves_to_column_positions() {
    let resolved = spec(&["DOC_rep_1", "DOC_rep_2", "DOC_rep_3"]).column_assignments();
    assert_eq!(
        resolved,
        vec![
            ColumnAssignment {
                column: "DOC_rep_1".to_string(),
                index: 0,
                retired: false,
            },
            ColumnAssignment {
                column: "DOC_rep_2".to_string(),
                index: 1,
                retired: false,
            },
            ColumnAssignment {
                column: "DOC_rep_3".to_string(),
                index: 2,
                retired: false,
            },
        ]
    );
}

#[test]
fn a_non_spot_stream_cannot_declare_replicates() {
    assert!(
        validate_declaration(
            &declaration(&["DOC_rep_1", "DOC_rep_2"]),
            Some("continuous")
        )
        .is_err()
    );
    assert!(validate_declaration(&declaration(&["DOC_rep_1", "DOC_rep_2"]), None).is_err());
}
