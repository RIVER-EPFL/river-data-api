use super::*;

/// Every kind is one round trip through the vocabulary, so a variant added without its string is
/// a compile error and a string added without its variant has nowhere to live.
#[test]
fn test_every_hold_kind_names_itself_once() {
    let all = [
        HoldKind::ReplicateStats,
        HoldKind::MissingOutput,
        HoldKind::StaleOutput,
        HoldKind::SkippedOutput,
        HoldKind::SourceModified,
        HoldKind::BrakeFired,
        HoldKind::SourceIdentityChanged,
        HoldKind::CurveClaimStripped,
        HoldKind::UnverifiedEntry,
    ];
    let mut names: Vec<&str> = all.iter().map(|k| k.as_str()).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(names.len(), before, "two kinds share a name: {names:?}");
}

/// The event-audit set is what every reader of calculation findings filters on, so it is asserted
/// rather than respelled: a kind added to the set reaches those readers.
#[test]
fn test_the_event_audit_set_is_the_three_calculation_findings() {
    assert_eq!(
        HoldKind::sql_list(&HoldKind::EVENT_AUDIT),
        "('missing_output', 'stale_output', 'skipped_output')"
    );
}

#[test]
fn test_a_single_kind_list_is_still_a_sql_list() {
    assert_eq!(
        HoldKind::sql_list(&[HoldKind::BrakeFired]),
        "('brake_fired')"
    );
}

/// The open and resolved sets partition the vocabulary, which is what lets the queue's counts and
/// the `resolved` filter add up to the whole table.
#[test]
fn test_open_and_resolved_partition_every_status() {
    let mut all: Vec<&str> = HoldStatus::ALL.iter().map(|s| s.as_str()).collect();
    all.sort_unstable();
    let mut split: Vec<&str> = HoldStatus::OPEN
        .iter()
        .chain(HoldStatus::RESOLVED.iter())
        .map(|s| s.as_str())
        .collect();
    split.sort_unstable();
    assert_eq!(all, split);
}

#[test]
fn test_every_status_parses_back_to_itself() {
    for status in HoldStatus::ALL {
        assert_eq!(HoldStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(HoldStatus::parse("nonsense"), None);
}

/// `reopen` takes a hold back from a decision, never from a status that was never decided.
#[test]
fn test_reopenable_is_a_subset_of_resolved() {
    for status in HoldStatus::REOPENABLE {
        assert!(
            HoldStatus::RESOLVED.contains(&status),
            "{} is reopenable but not resolved",
            status.as_str()
        );
    }
}
