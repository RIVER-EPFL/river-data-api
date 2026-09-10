use super::gap_scan;

#[test]
fn test_gap_scan_bounds_the_readings_side_when_given_a_window() {
    let bounded = gap_scan(Some(chrono::Utc::now())).to_string();
    assert!(
        bounded.contains("r.time >= "),
        "a bounded run must not hash the whole hypertable: {bounded}"
    );

    let full = gap_scan(None).to_string();
    assert!(
        !full.contains("r.time >= "),
        "the periodic full run is the one that covers older drift: {full}"
    );
}
