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
