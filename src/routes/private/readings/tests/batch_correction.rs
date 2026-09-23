use super::{BatchCorrection, CorrectionMismatch, batch_correction};
use crate::routes::private::sensor_calibrations::service::Curve;
use uuid::Uuid;

const BASE: Curve = Curve {
    id: Uuid::from_u128(1),
    slope: 2.0,
    intercept: 0.0,
};
const STANDARD: Curve = Curve {
    id: Uuid::from_u128(2),
    slope: 1.0,
    intercept: 5.0,
};
const UNKNOWN: Uuid = Uuid::from_u128(3);

fn stored(calibration_id: Option<Uuid>, calibrated_value: Option<f64>) -> BatchCorrection {
    BatchCorrection {
        calibration_id,
        calibrated_value,
    }
}

#[test]
fn test_batch_correction_applies_the_base_when_no_value_is_submitted() {
    // 2.0 * 10.0 + 0.0
    assert_eq!(
        batch_correction(10.0, None, Some(BASE.id), Some(BASE), None),
        Ok(stored(Some(BASE.id), Some(20.0)))
    );
}

#[test]
fn test_batch_correction_refuses_a_value_the_base_does_not_produce() {
    assert_eq!(
        batch_correction(10.0, Some(99.0), Some(BASE.id), Some(BASE), None),
        Err(CorrectionMismatch {
            calibration_id: BASE.id,
            submitted: 99.0,
            computed: 20.0,
        })
    );
}

#[test]
fn test_batch_correction_stores_the_computed_value_when_the_submitted_one_agrees() {
    // 20.000000000001 is within the relative tolerance of 20.0, and 20.0 is what is stored so the
    // drift sweep finds nothing to rewrite.
    assert_eq!(
        batch_correction(
            10.0,
            Some(20.000_000_000_001),
            Some(BASE.id),
            Some(BASE),
            None
        ),
        Ok(stored(Some(BASE.id), Some(20.0)))
    );
}

#[test]
fn test_batch_correction_composes_a_standard_curve_on_the_base() {
    // (2.0 * 10.0) * 1.0 + 5.0, whatever value was submitted
    for submitted in [None, Some(999.0)] {
        assert_eq!(
            batch_correction(10.0, submitted, Some(BASE.id), Some(BASE), Some(STANDARD)),
            Ok(stored(Some(BASE.id), Some(25.0)))
        );
    }
    // 10.0 * 1.0 + 5.0
    assert_eq!(
        batch_correction(10.0, Some(999.0), None, None, Some(STANDARD)),
        Ok(stored(None, Some(15.0)))
    );
}

#[test]
fn test_batch_correction_keeps_an_unaccounted_value_under_no_calibration() {
    assert_eq!(
        batch_correction(10.0, Some(99.0), None, None, None),
        Ok(stored(None, Some(99.0)))
    );
    assert_eq!(
        batch_correction(10.0, None, None, None, None),
        Ok(stored(None, None))
    );
}

#[test]
fn test_batch_correction_leaves_an_unresolved_calibration_id_to_the_insert() {
    assert_eq!(
        batch_correction(10.0, Some(99.0), Some(UNKNOWN), None, None),
        Ok(stored(Some(UNKNOWN), Some(99.0)))
    );
}

#[test]
fn test_batch_correction_mismatch_names_the_calibration() {
    let message = CorrectionMismatch {
        calibration_id: BASE.id,
        submitted: 99.0,
        computed: 20.0,
    }
    .to_string();
    assert!(message.contains(&BASE.id.to_string()), "{message}");
    assert!(
        message.contains("99") && message.contains("20"),
        "{message}"
    );
}
