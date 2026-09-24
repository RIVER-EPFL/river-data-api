use super::{ClosureQuery, closure_subject, parse_ids};
use crate::routes::private::tools::models::Subject;

#[test]
fn an_empty_or_absent_list_is_no_parameters_rather_than_an_error() {
    assert!(parse_ids(None).expect("absent").is_empty());
    assert!(parse_ids(Some("")).expect("empty").is_empty());
    assert!(parse_ids(Some(" , ")).expect("separators only").is_empty());
}

#[test]
fn a_value_that_is_not_a_uuid_is_refused_rather_than_dropped() {
    assert!(parse_ids(Some("not-a-uuid")).is_err());
}

fn query() -> ClosureQuery {
    ClosureQuery {
        parameter_ids: None,
        calibration_id: None,
        site_parameter_id: None,
        stream_id: None,
        calculation: None,
        constant_id: None,
        site_id: None,
        include_coverage: false,
    }
}

#[test]
fn each_subject_is_recognised_from_the_field_that_names_it() {
    let id = uuid::Uuid::new_v4();
    assert!(matches!(
        closure_subject(&ClosureQuery { calibration_id: Some(id), ..query() }).unwrap(),
        Subject::Calibration(got) if got == id
    ));
    assert!(matches!(
        closure_subject(&ClosureQuery { site_parameter_id: Some(id), ..query() }).unwrap(),
        Subject::Slot(got) if got == id
    ));
    assert!(matches!(
        closure_subject(&ClosureQuery { stream_id: Some(id), ..query() }).unwrap(),
        Subject::Reading { stream_id, .. } if stream_id == id
    ));
    assert!(matches!(
        closure_subject(&ClosureQuery { calculation: Some("doc".into()), ..query() }).unwrap(),
        Subject::Calculation(name) if name == "doc"
    ));
    // Where is this constant used: the one subject an administrator asks from, before correcting
    // a molar weight every stored output was computed with.
    assert!(matches!(
        closure_subject(&ClosureQuery { constant_id: Some(id), ..query() }).unwrap(),
        Subject::Constant(got) if got == id
    ));
}

#[test]
fn a_constant_beside_another_subject_is_refused() {
    let q = ClosureQuery {
        constant_id: Some(uuid::Uuid::new_v4()),
        calculation: Some("doc".into()),
        ..query()
    };
    assert!(closure_subject(&q).is_err());
}

#[test]
fn no_subject_is_the_parameter_list_it_has_always_been() {
    let id = uuid::Uuid::new_v4();
    let q = ClosureQuery {
        parameter_ids: Some(id.to_string()),
        ..query()
    };
    assert!(matches!(closure_subject(&q).unwrap(), Subject::Parameters(ids) if ids == vec![id]));
    assert!(
        matches!(closure_subject(&query()).unwrap(), Subject::Parameters(ids) if ids.is_empty())
    );
}

#[test]
fn two_subjects_at_once_are_refused_rather_than_ranked() {
    let q = ClosureQuery {
        calibration_id: Some(uuid::Uuid::new_v4()),
        stream_id: Some(uuid::Uuid::new_v4()),
        ..query()
    };
    assert!(closure_subject(&q).is_err());
}

/// Scenario: two janitor runs in the window, each filling pCO2 at Saxon, one also at Sion, and a
/// run whose report predates the per-calculation record.
///
/// Expected behaviour: the fills are summed per calculation and site, a site outside the reader's
/// scope is left out, and a report without the record adds nothing.
#[test]
fn janitor_fills_are_summed_per_calculation_and_site_across_runs() {
    use uuid::Uuid;
    let (pco2, saxon, sion) = (Uuid::from_u128(1), Uuid::from_u128(10), Uuid::from_u128(11));
    let run = |sites: serde_json::Value| serde_json::json!({ "scope": { "filled_by_calculation": { pco2.to_string(): sites } } });
    let details = [
        run(serde_json::json!({ saxon.to_string(): { "values": 3, "instants": [] } })),
        run(serde_json::json!({
            saxon.to_string(): { "values": 11, "instants": [] },
            sion.to_string(): { "values": 2, "instants": [] },
        })),
        serde_json::json!({ "scope": {}, "counts": { "filled": 4 } }),
    ];

    let all = super::sum_janitor_fills(&details, None);
    assert_eq!(all[&pco2][&saxon], 14);
    assert_eq!(all[&pco2][&sion], 2);

    let scoped = super::sum_janitor_fills(&details, Some(&[saxon]));
    assert_eq!(scoped[&pco2].len(), 1);
    assert_eq!(scoped[&pco2][&saxon], 14);
}
