use super::family_parameter_suggestion;

#[test]
fn test_family_suggestion_strips_only_the_structural_avg_segment() {
    assert_eq!(family_parameter_suggestion("DOC_avg_ppb"), "DOC_ppb");
    assert_eq!(family_parameter_suggestion("NO2_avg_mgL"), "NO2_mgL");
    assert_eq!(family_parameter_suggestion("avg"), "avg");
}

#[test]
fn test_a_units_bearing_column_never_resolves_onto_a_shorter_code() {
    // The suggestion does not read the catalog: a catalog holding `DOC` is not a
    // reason to export a `DOC` header where the portal wrote `DOC_avg_ppb`.
    assert_eq!(family_parameter_suggestion("DOC_ppb"), "DOC_ppb");
    assert_eq!(family_parameter_suggestion("DOC"), "DOC");
}

#[test]
fn slot_name_candidates_qualify_by_units_then_code() {
    assert_eq!(
        super::slot_name_candidates("Flux", "mg/L", "FluxA"),
        vec!["Flux", "Flux (mg/L)", "Flux (FluxA)"]
    );
    // A blank unit contributes no candidate of its own.
    assert_eq!(
        super::slot_name_candidates("Flux", "  ", "FluxA"),
        vec!["Flux", "Flux (FluxA)"]
    );
    // A code equal to the units would repeat the same name.
    assert_eq!(
        super::slot_name_candidates("Flux", "mg/L", "mg/L"),
        vec!["Flux", "Flux (mg/L)"]
    );
}

#[test]
fn a_name_reads_the_same_through_case_spacing_punctuation_and_leading_zeros() {
    let existing = ["FP1", "DOC_avg_ppb"];
    for proposed in ["FP-1", "fp 1", "FP01", "f p 1."] {
        assert_eq!(
            super::near_duplicate_of(proposed, existing),
            Some("FP1"),
            "{proposed} reads as FP1"
        );
    }
    assert_eq!(
        super::near_duplicate_of("doc.avg.ppb", existing),
        Some("DOC_avg_ppb")
    );
}

#[test]
fn two_stations_are_not_one_because_a_digit_differs() {
    let existing = ["FP1", "Depth"];
    // An edit distance would call these the same; a reader would not.
    assert_eq!(super::near_duplicate_of("FP2", existing), None);
    assert_eq!(super::near_duplicate_of("FP10", existing), None);
    assert_eq!(super::near_duplicate_of("Depths", existing), None);
    // The exact match is the catalog's own, not a near miss.
    assert_eq!(super::near_duplicate_of("fp1", ["FP1"]), None);
    // A name with nothing to canonicalise cannot collide with everything else that has none.
    assert_eq!(super::near_duplicate_of("---", ["***"]), None);
}
