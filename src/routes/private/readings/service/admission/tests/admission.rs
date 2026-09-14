use super::*;

#[test]
fn declared_missing_markers_carry_no_value_and_no_error() {
    for cell in ["", "   ", "NaN", "nan", "NA", "na"] {
        assert_eq!(classify_cell(cell), Cell::Missing, "cell {cell:?}");
    }
}

#[test]
fn every_spelling_of_the_sentinel_is_the_same_marker() {
    for cell in ["-9999", "-9999.0", "-9999.00", " -9999.000 ", "-9.999e3"] {
        assert_eq!(classify_cell(cell), Cell::Missing, "cell {cell:?}");
    }
    assert!(is_missing_sentinel(-9999.0));
    assert!(!is_missing_sentinel(-9998.9));
    assert!(!is_missing_sentinel(9999.0));
}

#[test]
fn a_value_next_to_the_sentinel_is_a_measurement() {
    assert_eq!(classify_cell("-9999.5"), Cell::Value(-9999.5));
    assert_eq!(classify_cell("-999.9"), Cell::Value(-999.9));
}

#[test]
fn non_finite_cells_are_errors_rather_than_missing_values() {
    for cell in ["Inf", "inf", "-inf", "Infinity", "-Infinity"] {
        assert!(
            matches!(classify_cell(cell), Cell::Invalid(_)),
            "cell {cell:?} must be a row error"
        );
    }
}

#[test]
fn unparseable_cells_are_errors() {
    assert!(matches!(classify_cell("n/a"), Cell::Invalid(_)));
    assert!(matches!(classify_cell("12,5"), Cell::Invalid(_)));
}

#[test]
fn ordinary_cells_parse() {
    assert_eq!(classify_cell(" 12.5 "), Cell::Value(12.5));
    assert_eq!(classify_cell("0"), Cell::Value(0.0));
    assert_eq!(classify_cell("1e3"), Cell::Value(1000.0));
}

#[test]
fn non_finite_values_are_refused_on_every_path() {
    assert!(admit_value(f64::NAN).is_err());
    assert!(admit_value(f64::INFINITY).is_err());
    assert!(admit_value(f64::NEG_INFINITY).is_err());
    assert!(admit_value(0.0).is_ok());
    assert!(admit_value(MISSING_SENTINEL).is_ok());
}

#[test]
fn the_timestamp_window_holds_at_its_edges_and_refuses_beyond_them() {
    let now = Utc::now();
    let (min_time, max_time) = window(now);
    assert!(time_rejection_at(now, now).is_none());
    assert!(time_rejection_at(now, min_time + Duration::minutes(1)).is_none());
    assert!(time_rejection_at(now, max_time - Duration::minutes(1)).is_none());
    assert!(time_rejection_at(now, min_time - Duration::days(1)).is_some());
    assert!(time_rejection_at(now, max_time + Duration::days(1)).is_some());
}

/// Expected behaviour: the lead bound moves with the clock, so a file's rows are judged
/// against one reading of it. Judged a millisecond later, this timestamp changes answer.
#[test]
fn a_timestamp_just_past_the_lead_bound_turns_on_which_clock_read_judges_it() {
    let now = Utc::now();
    let just_past = now + Duration::days(MAX_LEAD_DAYS) + Duration::microseconds(500);
    assert!(time_rejection_at(now, just_past).is_some());
    assert!(time_rejection_at(now + Duration::milliseconds(1), just_past).is_none());
}

/// Expected behaviour: the floor is a fixed date, so a decade-old archive series stays
/// ingestible indefinitely. A relative floor would make the same reading admissible today
/// and refused later, which is what stalls a portal backfill.
#[test]
fn the_backward_bound_does_not_move_with_the_clock() {
    let (early, _) = window("2020-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
    let (late, _) = window("2099-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
    assert_eq!(early, late);

    let archive = "2016-08-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    assert!(time_rejection_at(Utc::now(), archive).is_none());
}

/// Expected behaviour: `rejection` and `admit` are one implementation, so the reason a
/// reading is skipped on `/ingest` is the reason it would be refused elsewhere.
#[test]
fn rejection_reports_what_admit_raises() {
    let now = Utc::now();
    let (_, max_time) = window(now);

    assert_eq!(rejection(now, 1.0, None), None);
    assert_eq!(rejection(now, MISSING_SENTINEL, Some("spot")), None);

    for (time, value, declared) in [
        (max_time + Duration::days(2), 1.0, None),
        (now, f64::NAN, None),
        (now, 1.0, Some("grab")),
    ] {
        let reason = rejection(time, value, declared);
        assert!(reason.is_some(), "expected a reason for {declared:?}");
        assert!(admit(time, value, declared).is_err());
    }
}

#[test]
fn the_classification_vocabulary_is_closed() {
    let now = Utc::now();
    for declared in [None, Some("continuous"), Some("spot"), Some("derived")] {
        assert!(admit(now, 1.0, declared).is_ok(), "declared {declared:?}");
    }
    assert!(admit(now, 1.0, Some("grab")).is_err());
}
