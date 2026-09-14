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

// --- The site a row belongs to (M202) ---

const SITE_A: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00a1);
const SITE_B: Uuid = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00b2);

fn twelve_sites() -> SiteLookup {
    SiteLookup {
        by_id: HashSet::from([SITE_A, SITE_B]),
        by_name: HashMap::from([("and".to_string(), SITE_A), ("vim".to_string(), SITE_B)]),
    }
}

#[test]
fn test_site_column_is_the_declared_one_or_the_files_own() {
    let headers = ["Date", "Site_ID", "PAR1Lux_measured"];
    assert_eq!(site_column_index(&headers, None), Ok(Some(1)));
    assert_eq!(site_column_index(&headers, Some("site_id")), Ok(Some(1)));
    // A file naming no site keeps the request's single target.
    assert_eq!(site_column_index(&["Date", "Depth"], None), Ok(None));
    // A declared column the file lacks is refused, not ignored.
    assert!(site_column_index(&headers, Some("station")).is_err());
}

#[test]
fn test_a_row_resolves_its_own_site_by_name_or_id() {
    let sites = twelve_sites();
    assert_eq!(resolve_row_site("AND", &sites, SITE_B), Ok(SITE_A));
    assert_eq!(resolve_row_site(" vim ", &sites, SITE_A), Ok(SITE_B));
    assert_eq!(
        resolve_row_site(&SITE_A.to_string(), &sites, SITE_B),
        Ok(SITE_A)
    );
    // An empty cell is the request's target; an unknown name is nobody's.
    assert_eq!(resolve_row_site("", &sites, SITE_B), Ok(SITE_B));
    let err = resolve_row_site("PEU", &sites, SITE_B).unwrap_err();
    assert!(err.contains("PEU"), "{err}");
    // A well-formed id no site carries does not pass as one.
    let err = resolve_row_site(&Uuid::nil().to_string(), &sites, SITE_B).unwrap_err();
    assert!(err.contains(&Uuid::nil().to_string()), "{err}");
}
