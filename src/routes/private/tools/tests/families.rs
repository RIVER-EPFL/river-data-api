//! The served value of a visit's family, once the operator's unsaved cells are laid over it.
//!
//! Scenario: a calculation reads a spot parameter at a visit. Expected behaviour: a family nobody
//! has typed into is served exactly as it was before the overlay existed (the stored `samples`
//! mean, else the lowest live replicate), and a family the pass moved has its statistics taken
//! here under the same rule the samples trigger applies.

use super::*;

fn member(replicate_index: i16, value: f64, mean: Option<f64>) -> SpotMember {
    SpotMember {
        stream_id: Some(Uuid::from_u128(0x5741_0100)),
        replicate_index,
        value: Some(value),
        mean,
        revision: None,
        judged: false,
    }
}

fn family(members: Vec<Option<SpotMember>>, restated: bool) -> StagedFamily {
    StagedFamily { members, restated }
}

/// Untouched, with a sample row: the stored mean is served, over every live member.
#[test]
fn test_untouched_family_serves_the_stored_mean() {
    let f = family(
        vec![
            Some(member(0, 1.0, Some(2.0))),
            Some(member(1, 3.0, Some(2.0))),
        ],
        false,
    );
    let (value, kind, behind) = served_of(&f).expect("a family with two live members is served");
    assert!((value - 2.0).abs() < f64::EPSILON);
    assert_eq!(kind, "mean");
    assert_eq!(behind.len(), 2);
}

/// Untouched, with no sample row: the lowest live replicate is the reading served.
#[test]
fn test_untouched_single_member_serves_the_reading() {
    let f = family(vec![None, Some(member(1, 4.5, None))], false);
    let (value, kind, behind) = served_of(&f).expect("one live member is served");
    assert!((value - 4.5).abs() < f64::EPSILON);
    assert_eq!(kind, "reading");
    assert_eq!(behind.len(), 1);
}

/// Restated: the mean is recomputed over the merged members rather than read from a `samples` row
/// that no longer describes them.
#[test]
fn test_restated_family_recomputes_the_mean() {
    // The stored row says 2.0; the operator has typed the second replicate up to 5.0.
    let f = family(
        vec![
            Some(member(0, 1.0, Some(2.0))),
            Some(member(1, 5.0, Some(2.0))),
        ],
        true,
    );
    let (value, kind, _) = served_of(&f).expect("a restated family is served");
    assert!((value - 3.0).abs() < f64::EPSILON); // (1.0 + 5.0) / 2
    assert_eq!(kind, "mean");
}

/// Restated down to one live member: a group of one has no statistic, so the value is served as
/// the reading it is, the rule the samples trigger applies.
#[test]
fn test_restated_family_of_one_serves_the_reading() {
    let f = family(vec![Some(member(0, 1.0, Some(2.0))), None], true);
    let (value, kind, behind) = served_of(&f).expect("one live member is served");
    assert!((value - 1.0).abs() < f64::EPSILON);
    assert_eq!(kind, "reading");
    assert_eq!(behind.len(), 1);
}

/// Restated to nothing: the operator emptied every cell, so the calculation has no value to read
/// and its own requiredness decides what that costs.
#[test]
fn test_restated_empty_family_serves_nothing() {
    let f = family(vec![None, None], true);
    assert!(served_of(&f).is_none());
}

/// A gap is a gap: an index the visit holds no live reading of does not shift the ones after it.
#[test]
fn test_gaps_keep_their_position() {
    let f = family(
        vec![Some(member(0, 1.0, None)), None, Some(member(2, 3.0, None))],
        true,
    );
    let live = f.live();
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].replicate_index, 0);
    assert_eq!(live[1].replicate_index, 2);
    let (value, kind, _) = served_of(&f).expect("two live members are served");
    assert!((value - 2.0).abs() < f64::EPSILON); // (1.0 + 3.0) / 2
    assert_eq!(kind, "mean");
}

/// A staged cell at a slot with no grab stream names no row, so it carries no consumed member and
/// its value stands in the input's own value.
#[test]
fn test_a_staged_cell_with_no_stream_records_no_member() {
    let at = chrono::Utc::now();
    let mut m = member(0, 1.0, None);
    m.stream_id = None;
    assert!(consumed_reading(&m, at).is_none());
    assert!(consumed_reading(&member(0, 1.0, None), at).is_some());
}

/// A restated family's statistics are the sample statistics: the same mean the serving path takes,
/// and a sample sd of n-1 beside it (Q203). There is no second arithmetic here to drift from the
/// stored one.
#[test]
fn test_restated_statistics_are_the_sample_statistics() {
    let f = family(
        vec![
            Some(member(0, 4.0, Some(15.0))),
            Some(member(1, 8.0, Some(15.0))),
        ],
        true,
    );
    let values: Vec<f64> = f.live().iter().filter_map(|m| m.value).collect();
    let stats = crate::routes::private::sync::service::group_stats(&values);
    let (value, _, _) = served_of(&f).expect("a restated family is served");
    assert_eq!(Some(value), stats.mean);
    assert_eq!(stats.n, 2);
    // sqrt(((4-6)^2 + (8-6)^2) / (2-1))
    assert!((stats.sd.expect("a group of two has an sd") - 8.0_f64.sqrt()).abs() < 1e-12);
}
