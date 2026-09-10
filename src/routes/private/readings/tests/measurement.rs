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

/// The resolution order, most specific first: a per-reading override beats the stream's default,
/// which beats the owning sensor's frequency, which beats continuous. Each rung is asserted with
/// every rung below it set to something else, so a reordered `.or()` chain fails here rather than
/// through an ingest response.
#[test]
fn test_resolve_measurement_type_takes_the_most_specific_rung() {
    let sensor = Uuid::from_u128(1);
    let mut sensors = HashMap::new();
    sensors.insert(sensor, MeasurementType::Spot.as_str());

    let cases: [(Option<&str>, Option<&str>, Option<Uuid>, &str); 6] = [
        // An override wins over a stream default and a sensor that both say otherwise.
        (Some("derived"), Some("spot"), Some(sensor), "derived"),
        // With no override, the stream's default wins over the sensor's frequency.
        (None, Some("continuous"), Some(sensor), "continuous"),
        // With neither, the owning sensor's frequency decides.
        (None, None, Some(sensor), "spot"),
        // A sensor absent from the map decides nothing.
        (None, None, Some(Uuid::from_u128(2)), "continuous"),
        // Nothing to go on at all.
        (None, None, None, "continuous"),
        // An override alone, with no stream and no sensor.
        (Some("spot"), None, None, "spot"),
    ];

    for (override_value, stream_default, sensor_id, expected) in cases {
        assert_eq!(
            resolve_measurement_type(override_value, stream_default, sensor_id, &sensors),
            expected,
            "override={override_value:?} stream={stream_default:?} sensor={sensor_id:?}"
        );
    }
}

/// The four arguments are distinct types only in pairs, so a caller that swaps the two `Option<&str>`
/// rungs compiles. This pins which one is the override.
#[test]
fn test_resolve_measurement_type_reads_the_override_first() {
    let sensors = HashMap::new();
    assert_eq!(
        resolve_measurement_type(Some("spot"), Some("derived"), None, &sensors),
        "spot",
        "the first argument is the per-reading override"
    );
}
