use uuid::Uuid;

use super::{CorrectedRow, Selection, corrected_grab_pairs};
use crate::routes::private::readings::models::SelectionKey;

const STREAM: Uuid = Uuid::from_u128(1);
const SITE: Uuid = Uuid::from_u128(2);
const PARAMETER: Uuid = Uuid::from_u128(3);

fn at() -> chrono::DateTime<chrono::Utc> {
    "2025-06-15T10:00:00Z".parse().unwrap()
}

fn row(index: i16, measurement_type: &str) -> CorrectedRow {
    CorrectedRow {
        stream_id: STREAM,
        time: at(),
        replicate_index: index,
        site_id: Some(SITE),
        parameter_id: Some(PARAMETER),
        measurement_type: Some(measurement_type.to_string()),
    }
}

fn key(index: i16, value: Option<f64>) -> SelectionKey {
    SelectionKey {
        stream_id: STREAM,
        time: at(),
        replicate_index: Some(index),
        value,
    }
}

#[test]
fn test_corrected_grab_pairs_single_value() {
    let selection = Selection {
        keys: vec![key(0, None)],
        ..Default::default()
    };
    let pairs = corrected_grab_pairs(&[row(0, "spot")], &selection, Some(12000.0));
    assert_eq!(pairs[&SITE], vec![(PARAMETER, 12000.0)]);
}

#[test]
fn test_corrected_grab_pairs_value_per_key() {
    let selection = Selection {
        keys: vec![key(0, Some(1.0)), key(1, Some(2.0))],
        ..Default::default()
    };
    let pairs = corrected_grab_pairs(&[row(0, "spot"), row(1, "spot")], &selection, None);
    assert_eq!(pairs[&SITE], vec![(PARAMETER, 1.0), (PARAMETER, 2.0)]);
}

#[test]
fn test_corrected_grab_pairs_sensor_rows_unscreened() {
    let selection = Selection {
        stream_id: Some(STREAM),
        ..Default::default()
    };
    let pairs = corrected_grab_pairs(&[row(0, "continuous")], &selection, Some(5.0));
    assert!(pairs.is_empty());
}
