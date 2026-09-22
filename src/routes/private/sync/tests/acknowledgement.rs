use super::*;

/// Scenario: four purposes acknowledge a hold, and each owns the kinds its UI raises.
/// Expected behaviour: a purpose owns no kind another owns, so no route can reach another
/// purpose's rows.
#[test]
fn every_kind_belongs_to_at_most_one_purpose() {
    for purpose in Purpose::ALL {
        for kind in purpose.kinds() {
            let owners: Vec<Purpose> = Purpose::ALL
                .iter()
                .copied()
                .filter(|p| p.owns(*kind))
                .collect();
            assert_eq!(owners.len(), 1, "{kind:?} is owned by {owners:?}");
        }
    }
}

#[test]
fn a_purpose_refuses_a_kind_it_does_not_own() {
    assert!(Purpose::StreamBrake.owns(HoldKind::BrakeFired));
    assert!(!Purpose::StreamBrake.owns(HoldKind::ReplicateStats));
    assert!(!Purpose::StreamBrake.owns(HoldKind::SourceIdentityChanged));
    assert!(Purpose::CalculationFinding.owns(HoldKind::MissingOutput));
    assert!(Purpose::CalculationFinding.owns(HoldKind::StaleOutput));
    assert!(Purpose::CalculationFinding.owns(HoldKind::SkippedOutput));
    assert!(!Purpose::CalculationFinding.owns(HoldKind::UnverifiedEntry));
}

/// A released brake is not an accepted statistic: B410 was one sentence minted for every kind.
#[test]
fn each_purpose_mints_its_own_sentence() {
    let expected = serde_json::json!({ "mean": 10.0, "sd": 1.0, "n": 3 });
    let computed = serde_json::json!({ "mean": 11.0, "sd": 2.0, "n": 3 });
    let sentences: Vec<String> = Purpose::ALL
        .iter()
        .map(|p| p.sentence(&expected, &computed, "ada"))
        .collect();
    for (i, one) in sentences.iter().enumerate() {
        assert!(one.contains("ada"), "{one} does not name who acted");
        for other in &sentences[i + 1..] {
            assert_ne!(one, other);
        }
    }
    let brake = Purpose::StreamBrake.sentence(&expected, &computed, "ada");
    assert!(
        !brake.contains("statistics computed here stand"),
        "a released brake mints the statistics sentence: {brake}"
    );
    assert!(
        Purpose::Statistics
            .sentence(&expected, &computed, "ada")
            .contains("statistics computed here stand")
    );
}

/// The numbers belong to the statistics sentence alone: a brake's `expected` carries a window,
/// not a mean, so `disagreement_phrase` over it reads as "source mean none sd none".
#[test]
fn only_the_statistics_sentence_carries_the_numbers() {
    let window = serde_json::json!({ "window": { "from": "2026-01-01T00:00:00Z" } });
    let empty = serde_json::json!({});
    for purpose in Purpose::ALL {
        if purpose == Purpose::Statistics {
            continue;
        }
        let sentence = purpose.sentence(&window, &empty, "ada");
        assert!(
            !sentence.contains("source mean"),
            "{purpose:?} mints a disagreement phrase: {sentence}"
        );
    }
}

#[test]
fn a_purpose_records_its_own_action() {
    let actions: Vec<&str> = Purpose::ALL.iter().map(|p| p.action()).collect();
    let mut sorted = actions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), actions.len(), "two purposes share an action");
}
