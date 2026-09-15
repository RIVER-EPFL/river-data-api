use uuid::Uuid;

use crate::routes::private::tools::flows::readings_for_output;

fn at() -> chrono::DateTime<chrono::Utc> {
    "2025-06-15T09:00:00Z".parse().unwrap()
}

#[test]
fn a_scalar_output_is_one_reading_with_no_replicate_index() {
    let readings = readings_for_output("doc", Uuid::nil(), &serde_json::json!(4.2), at());
    assert_eq!(readings.len(), 1);
    assert!((readings[0].value - 4.2).abs() < f64::EPSILON);
    assert_eq!(readings[0].replicate_index, None);
    assert_eq!(readings[0].output.as_deref(), Some("doc"));
}

/// Expected behaviour: the output's replicate identity is the input's, so a gap keeps its index
/// and the repeats after it keep theirs.
#[test]
fn a_per_replicate_output_is_one_reading_per_index_and_a_gap_stays_at_its_index() {
    let readings = readings_for_output(
        "doc",
        Uuid::nil(),
        &serde_json::json!([1.0, null, 3.0]),
        at(),
    );
    let indexes: Vec<Option<i16>> = readings.iter().map(|r| r.replicate_index).collect();
    assert_eq!(indexes, vec![Some(0), Some(2)]);
    let values: Vec<f64> = readings.iter().map(|r| r.value).collect();
    assert_eq!(values, vec![1.0, 3.0]);
}

#[test]
fn an_output_with_no_number_is_no_reading() {
    assert!(readings_for_output("doc", Uuid::nil(), &serde_json::json!(null), at()).is_empty());
    assert!(
        readings_for_output("doc", Uuid::nil(), &serde_json::json!([null, null]), at()).is_empty()
    );
}
