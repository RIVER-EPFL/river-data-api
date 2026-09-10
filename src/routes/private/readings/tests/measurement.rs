use super::*;

#[test]
fn test_measurement_type_rejection_admits_every_member() {
    for v in MeasurementType::ALL {
        assert_eq!(measurement_type_rejection(Some(v.as_str())), None);
    }
    assert_eq!(measurement_type_rejection(None), None);
}

#[test]
fn test_measurement_type_rejection_names_the_whole_vocabulary() {
    let reason = measurement_type_rejection(Some("spott")).expect("a typo is refused");
    for v in MeasurementType::ALL {
        assert!(reason.contains(v.as_str()), "{reason} omits {v}");
    }
}

#[test]
fn test_retag_target_rejection_admits_the_vocabulary_and_declared() {
    for v in MeasurementType::ALL {
        assert_eq!(retag_target_rejection(v.as_str()), None);
    }
    assert_eq!(retag_target_rejection(RETAG_DECLARED), None);
    assert!(retag_target_rejection("hourly").is_some());
}
