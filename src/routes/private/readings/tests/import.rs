use super::*;
use crate::routes::private::tools::models::ManifestCurve;
use crate::routes::private::tools::models::ManifestParam;

fn slots() -> Vec<ManifestCurve> {
    vec![ManifestCurve {
        name: "std_curve".into(),
        label: "Standard curve".into(),
        required: false,
        description: None,
    }]
}

fn doc_params() -> Vec<ManifestParam> {
    serde_json::from_value(serde_json::json!([
        { "name": "DOC", "label": "DOC", "kind": "replicates",
          "parameter_code": "DOC", "curve": "std_curve" },
        { "name": "volume", "label": "Volume", "kind": "number" }
    ]))
    .unwrap()
}

const CURVE_A: Uuid = Uuid::from_u128(0xa);
const CURVE_B: Uuid = Uuid::from_u128(0xb);

#[test]
fn test_curve_column_matches_a_slot_name_case_insensitively() {
    assert_eq!(
        curve_column(&slots(), "STD_Curve").as_deref(),
        Some("std_curve")
    );
    assert_eq!(curve_column(&slots(), "DOC_rep_1"), None);
}

#[test]
fn test_row_curves_cell_overrides_the_request_default() {
    let defaults = HashMap::from([("std_curve".to_string(), CURVE_A)]);
    let curves = row_curves(&defaults, &[("std_curve", &CURVE_B.to_string())]).unwrap();
    assert_eq!(curves["std_curve"], CURVE_B);
}

#[test]
fn test_row_curves_blank_cell_keeps_the_request_default() {
    let defaults = HashMap::from([("std_curve".to_string(), CURVE_A)]);
    let curves = row_curves(&defaults, &[("std_curve", "  ")]).unwrap();
    assert_eq!(curves["std_curve"], CURVE_A);
}

#[test]
fn test_row_curves_without_a_column_or_default_names_nothing() {
    assert!(row_curves(&HashMap::new(), &[]).unwrap().is_empty());
}

#[test]
fn test_row_curves_rejects_a_cell_that_is_not_an_id() {
    let err = row_curves(&HashMap::new(), &[("std_curve", "plate 3")]).unwrap_err();
    assert!(
        err.contains("std_curve") && err.contains("plate 3"),
        "{err}"
    );
}

#[test]
fn test_replicate_curve_is_the_slot_the_param_declares() {
    let curves = HashMap::from([("std_curve".to_string(), CURVE_A)]);
    assert_eq!(
        replicate_curve(&doc_params(), "DOC", &curves),
        Some(CURVE_A)
    );
    // A param declaring no slot carries nothing, whatever the row resolved.
    assert_eq!(replicate_curve(&doc_params(), "volume", &curves), None);
    // A declared slot the row left empty carries nothing.
    assert_eq!(replicate_curve(&doc_params(), "DOC", &HashMap::new()), None);
}

#[test]
fn test_timestamp_column_accepts_the_three_header_names_in_any_case() {
    assert_eq!(timestamp_column(&["DateTime", "Depth"]), Ok(0));
    assert_eq!(
        timestamp_column(&["Site_ID", "Date", "WaterTempdegC"]),
        Ok(1)
    );
    assert_eq!(timestamp_column(&["Site_ID", "TIME"]), Ok(1));
}

#[test]
fn test_timestamp_column_names_the_headers_it_saw_when_none_is_a_time() {
    let refused = timestamp_column(&["Site_ID", "WaterTempdegC"]).unwrap_err();
    assert!(refused.contains("Site_ID"), "{refused}");
    assert!(refused.contains("WaterTempdegC"), "{refused}");
}
