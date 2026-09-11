use super::tests::plan_entry;
use super::{ReviewState, compute_summary, review_state};

#[test]
fn test_review_state_asks_only_where_the_evidence_is_short() {
    let matched = plan_entry("FP1", "Depth", "exact", 0);
    assert_eq!(review_state(&matched), ReviewState::SelfValidated);

    let warned = plan_entry("FP1", "CDOM", "exact", 1);
    assert_eq!(review_state(&warned), ReviewState::NeedsChecking);

    let unmatched = plan_entry("FP2", "Depth", "none", 0);
    assert_eq!(review_state(&unmatched), ReviewState::NeedsChecking);

    // A tick settles the entry whatever its evidence said.
    let mut acknowledged = plan_entry("FP2", "CDOM", "none", 2);
    acknowledged.acknowledged = true;
    assert_eq!(review_state(&acknowledged), ReviewState::Acknowledged);
}

#[test]
fn test_compute_summary_counts_the_three_states_over_pairing_entries_only() {
    let mut skipped = plan_entry("FP3", "Depth", "none", 1);
    skipped.action = "skip".to_string();
    let mut acknowledged = plan_entry("FP2", "CDOM", "none", 1);
    acknowledged.acknowledged = true;

    let summary = compute_summary(&[
        plan_entry("FP1", "Depth", "exact", 0),
        plan_entry("FP1", "CDOM", "exact", 0),
        plan_entry("FP2", "Depth", "none", 0),
        acknowledged,
        skipped,
    ]);

    assert_eq!(summary.will_pair, 4);
    assert_eq!(summary.self_validated, 2);
    assert_eq!(summary.needs_checking, 1);
    assert_eq!(summary.acknowledged, 1);
    assert_eq!(
        summary.self_validated + summary.needs_checking + summary.acknowledged,
        summary.will_pair,
        "every pairing entry is in exactly one state"
    );
}

#[test]
fn test_apply_is_refused_while_a_pairing_row_still_needs_checking() {
    use super::refuse_unchecked_entries;

    // A row that resolved cleanly is still a row nobody looked at (Q155).
    let clean = [
        plan_entry("FP1", "Depth", "exact", 0),
        plan_entry("FP1", "CDOM", "exact", 0),
    ];
    let message = refuse_unchecked_entries(&clean).unwrap_err().to_string();
    assert!(message.contains("2 row(s) still to tick"), "{message}");

    let unmatched = [
        plan_entry("FP1", "Depth", "exact", 0),
        plan_entry("FP2", "Turbidity", "none", 0),
    ];
    let message = refuse_unchecked_entries(&unmatched)
        .unwrap_err()
        .to_string();
    assert!(message.contains("2 row(s) still to tick"), "{message}");
    assert!(message.contains("FP2"), "{message}");

    // A tick is what clears it, which is the only thing the review can do to a warned row.
    let mut acknowledged = plan_entry("FP2", "Turbidity", "none", 2);
    acknowledged.acknowledged = true;
    assert!(refuse_unchecked_entries(&[acknowledged]).is_ok());

    // A skipped row is not paired, so it is not asked about.
    let mut skipped = plan_entry("FP2", "Turbidity", "none", 1);
    skipped.action = "skip".to_string();
    assert!(refuse_unchecked_entries(&[skipped]).is_ok());
}
