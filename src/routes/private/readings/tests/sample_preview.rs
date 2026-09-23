use super::*;

fn rep(index: i16, value: f64) -> Replicate {
    Replicate {
        index,
        value,
        flagged: false,
        withdrawn: false,
        unverified: false,
    }
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

#[test]
fn excluding_the_highest_of_three_recomputes_over_the_other_two() {
    let group = [rep(0, 10.0), rep(1, 20.0), rep(2, 30.0)];
    let change = Change {
        exclude: &[2],
        ..Change::default()
    };
    let (current, proposed, delta, rows) = preview_statistics(&group, &change);
    assert_eq!(current.n, 3);
    assert!(close(current.mean, 20.0));
    assert!(close(current.sd, 10.0));
    assert_eq!(proposed.n, 2);
    assert!(close(proposed.mean, 15.0), "{proposed:?}");
    assert!(close(proposed.sd, 7.071_067_811_865_476));
    assert_eq!(delta.n, -1);
    assert!(close(delta.mean, -5.0));
    assert!(rows[2].included_now && !rows[2].included_after);
}

#[test]
fn restoring_a_flagged_replicate_brings_it_back() {
    let mut group = [rep(0, 10.0), rep(1, 20.0), rep(2, 30.0)];
    group[2].flagged = true;
    let change = Change {
        include: &[2],
        ..Change::default()
    };
    let (current, proposed, _, rows) = preview_statistics(&group, &change);
    assert_eq!(current.n, 2);
    assert_eq!(proposed.n, 3);
    assert!(close(proposed.mean, 20.0));
    assert!(!rows[2].included_now && rows[2].included_after);
}

#[test]
fn a_withdrawn_replicate_is_outside_both_counts() {
    let mut group = [rep(0, 10.0), rep(1, 20.0), rep(2, 30.0)];
    group[2].withdrawn = true;
    let change = Change {
        include: &[2],
        ..Change::default()
    };
    let (current, proposed, _, _) = preview_statistics(&group, &change);
    assert_eq!(current.n, 2);
    assert_eq!(proposed.n, 2);
}

#[test]
fn a_hold_is_met_when_the_proposed_statistics_agree_within_tolerance() {
    let hold_id = Uuid::nil();
    let group = [rep(0, 10.0), rep(1, 20.0), rep(2, 999.0)];
    let expected = GroupAudit {
        time: Utc::now(),
        expected_mean: Some(15.0),
        expected_sd: Some(7.071_067_811_865_476),
        expected_n: None,
    };
    let change = Change {
        exclude: &[2],
        ..Change::default()
    };
    let (current, proposed, _, _) = preview_statistics(&group, &change);
    let m = hold_match(hold_id, &expected, &current, &proposed);
    assert!(!m.meets_now);
    assert!(m.meets_after, "{m:?}");
    assert!(m.mean_agrees && m.sd_agrees && m.n_agrees);

    // The source counted three cells, so dropping to two cannot meet it.
    let expected_n = GroupAudit {
        expected_n: Some(3),
        ..expected
    };
    let m = hold_match(hold_id, &expected_n, &current, &proposed);
    assert!(m.mean_agrees && m.sd_agrees && !m.n_agrees);
    assert!(!m.meets_after);
}

/// An intern's pending replicate is outside the samples row until a manager verifies it, so the
/// preview leaves it out of both counts, whatever the change names.
#[test]
fn an_unverified_replicate_is_outside_both_counts() {
    let mut group = [rep(0, 2.0), rep(1, 3.0), rep(2, 10.0), rep(3, 2.5)];
    for r in &mut group[..3] {
        r.unverified = true;
    }
    let change = Change {
        include: &[0],
        ..Change::default()
    };
    let (current, proposed, _, rows) = preview_statistics(&group, &change);
    assert_eq!(current.n, 1);
    assert!(close(current.mean, 2.5), "{current:?}");
    assert_eq!(proposed.n, 1);
    assert!(!rows[0].included_now && !rows[0].included_after);
}

#[test]
fn test_counts_in_sample_is_the_trigger_s_rule() {
    assert!(counts_in_sample(false, false, false));
    assert!(!counts_in_sample(true, false, false));
    assert!(!counts_in_sample(false, true, false));
    assert!(!counts_in_sample(false, false, true));
}
