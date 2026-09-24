use serde_json::json;

use super::{FamilyRepeats, TakeoverFacts, TakeoverVerdict, family_repeats, takeover_verdict};

fn facts() -> TakeoverFacts {
    TakeoverFacts {
        code: "CO2_HS_Um".to_string(),
        code_holder: None,
        catalog_units: Some("umol/L".to_string()),
        published: false,
        portal_function: None,
        family: None,
        formula_units: "umol/L".to_string(),
    }
}

#[test]
fn test_takeover_verdict_frees_a_code_nothing_holds() {
    let free = TakeoverFacts {
        catalog_units: None,
        ..facts()
    };
    assert_eq!(takeover_verdict(&free), TakeoverVerdict::Free);
}

#[test]
fn test_takeover_verdict_offers_a_decommissioned_calculation_s_column() {
    let held = TakeoverFacts {
        code_holder: Some(("pco2".to_string(), true)),
        published: true,
        ..facts()
    };
    assert_eq!(
        takeover_verdict(&held),
        TakeoverVerdict::Eligible {
            kind: "decommissioned",
            computed_by: "pco2".to_string()
        }
    );
}

#[test]
fn test_takeover_verdict_refuses_a_live_calculation_s_formula() {
    let held = TakeoverFacts {
        code_holder: Some(("pco2".to_string(), false)),
        published: true,
        ..facts()
    };
    let TakeoverVerdict::Refused(why) = takeover_verdict(&held) else {
        panic!("refused")
    };
    assert!(why.contains("CO2_HS_Um is a formula of pco2"), "{why}");
}

#[test]
fn test_takeover_verdict_offers_a_column_the_portal_computed() {
    let portal = TakeoverFacts {
        portal_function: Some("calcCO2".to_string()),
        ..facts()
    };
    assert_eq!(
        takeover_verdict(&portal),
        TakeoverVerdict::Eligible {
            kind: "portal",
            computed_by: "calcCO2".to_string()
        }
    );
}

#[test]
fn test_takeover_verdict_leaves_a_typed_column_to_the_catalog_guard() {
    // Neither a formula nor the portal computed it: measured or typed, never taken over.
    assert_eq!(takeover_verdict(&facts()), TakeoverVerdict::Typed);
}

#[test]
fn test_takeover_verdict_offers_a_family_of_computed_repeats() {
    // CO2_HS_Um_A|B: calcCO2 per repeat, calcMean over them.
    let family = TakeoverFacts {
        portal_function: Some("calcMean".to_string()),
        family: Some(FamilyRepeats::Computed("calcCO2".to_string())),
        ..facts()
    };
    assert_eq!(
        takeover_verdict(&family),
        TakeoverVerdict::Eligible {
            kind: "portal",
            computed_by: "calcCO2".to_string()
        }
    );
}

#[test]
fn test_takeover_verdict_leaves_a_family_of_typed_repeats_to_the_catalog_guard() {
    // Reach_depth_rep_*: typed repeats, the portal computed only the mean over them.
    let family = TakeoverFacts {
        portal_function: Some("calcMean".to_string()),
        family: Some(FamilyRepeats::Typed),
        ..facts()
    };
    assert_eq!(takeover_verdict(&family), TakeoverVerdict::Typed);
}

#[test]
fn test_takeover_verdict_refuses_a_family_of_mixed_repeats() {
    let family = TakeoverFacts {
        portal_function: Some("calcMean".to_string()),
        family: Some(FamilyRepeats::Mixed),
        ..facts()
    };
    let TakeoverVerdict::Refused(why) = takeover_verdict(&family) else {
        panic!("refused")
    };
    assert!(why.contains("replicate family"), "{why}");
}

#[test]
fn test_family_repeats_reads_a_calculation_per_repeat_as_computed() {
    let computed = json!({"members": ["CO2_HS_Um_A", "CO2_HS_Um_B"], "member_calculations": [
        {"function": "calcCO2", "inputs": ["CO2_HS_Um_A_ppm"]},
        {"function": "calcCO2", "inputs": ["CO2_HS_Um_B_ppm"]},
    ]});
    assert_eq!(
        family_repeats(&[computed]),
        Some(FamilyRepeats::Computed("calcCO2".to_string()))
    );
}

#[test]
fn test_family_repeats_reads_null_as_typed() {
    let typed =
        json!({"members": ["lab_co2_co2ppm_A", "lab_co2_co2ppm_B"], "member_calculations": null});
    let unrecorded = json!({"members": ["Reach_depth_rep_1", "Reach_depth_rep_2"]});
    assert_eq!(
        family_repeats(&[typed, unrecorded]),
        Some(FamilyRepeats::Typed)
    );
}

#[test]
fn test_family_repeats_reads_a_gap_or_disagreement_as_mixed() {
    let gap = json!({"member_calculations": [{"function": "calcCO2"}, null]});
    assert_eq!(family_repeats(&[gap]), Some(FamilyRepeats::Mixed));
    let one = json!({"member_calculations": [{"function": "calcCO2"}]});
    let other = json!({"member_calculations": [{"function": "calcO2"}]});
    assert_eq!(
        family_repeats(&[one.clone(), other]),
        Some(FamilyRepeats::Mixed)
    );
    assert_eq!(
        family_repeats(&[one, json!({"member_calculations": null})]),
        Some(FamilyRepeats::Mixed)
    );
}

#[test]
fn test_family_repeats_is_none_without_a_family() {
    assert_eq!(family_repeats(&[]), None);
}

#[test]
fn test_takeover_verdict_refuses_other_units_naming_both() {
    let other = TakeoverFacts {
        portal_function: Some("calcCO2".to_string()),
        formula_units: "ppm".to_string(),
        ..facts()
    };
    let TakeoverVerdict::Refused(why) = takeover_verdict(&other) else {
        panic!("refused")
    };
    assert!(why.contains("ppm") && why.contains("umol/L"), "{why}");
}

#[test]
fn test_takeover_verdict_leaves_a_parameter_a_formula_already_publishes() {
    // A live calculation's output under another formula code, or this set's own: no takeover.
    let published = TakeoverFacts {
        published: true,
        ..facts()
    };
    assert_eq!(takeover_verdict(&published), TakeoverVerdict::Free);
}

#[test]
fn test_takeover_suffix_keeps_the_code_and_dates_it() {
    let on: chrono::NaiveDate = "2026-09-24".parse().unwrap();
    assert_eq!(
        super::taken_over_code("CO2_HS_Um", on),
        "CO2_HS_Um~taken-over-2026-09-24"
    );
}
