use super::Severity::{Error, Info, Warning};
use super::*;

/// One row of each source shape, at each severity it can reach.
#[test]
fn every_source_maps_onto_the_same_three_words() {
    assert_eq!(Severity::job("failed"), Error);
    assert_eq!(Severity::job("cancelled"), Warning);
    assert_eq!(Severity::job("completed"), Info);
    assert_eq!(Severity::job("queued"), Info);

    assert_eq!(Severity::job_log("error"), Error);
    assert_eq!(Severity::job_log("warn"), Warning);
    assert_eq!(Severity::job_log("info"), Info);

    assert_eq!(Severity::notification("failed", Some("410 Gone")), Error);
    assert_eq!(Severity::notification("sent", None), Info);

    assert_eq!(Severity::ingest_receipt(0, true), Error);
    assert_eq!(Severity::ingest_receipt(3, false), Warning);
    assert_eq!(Severity::ingest_receipt(0, false), Info);

    assert_eq!(Severity::audit_hold("pending"), Error);
    assert_eq!(Severity::audit_hold("deferred"), Warning);
    assert_eq!(Severity::audit_hold("acknowledged"), Info);

    assert_eq!(Severity::alarm(2, false), Error);
    assert_eq!(Severity::alarm(1, false), Warning);
    assert_eq!(Severity::alarm(2, true), Info);

    assert_eq!(Severity::sync_event("failed"), Error);
    assert_eq!(Severity::sync_event("partial"), Warning);
    assert_eq!(Severity::sync_event("completed"), Info);
}

/// A delivery that recorded no status but did record a message is still a failure, and one
/// whose message is blank is not.
#[test]
fn a_delivery_is_read_from_both_of_its_columns() {
    assert_eq!(Severity::notification("sent", Some("410 Gone")), Error);
    assert_eq!(Severity::notification("sent", Some("   ")), Info);
}

/// A delivery the dispatcher declined to make is not a delivery that failed.
#[test]
fn declining_to_send_is_not_a_failure() {
    assert_eq!(Severity::notification("muted", None), Info);
    assert_eq!(Severity::notification("skipped", None), Info);
}

#[test]
fn the_words_are_the_filter_values() {
    assert_eq!(Error.as_str(), "error");
    assert_eq!(Warning.as_str(), "warning");
    assert_eq!(Info.as_str(), "info");
}
