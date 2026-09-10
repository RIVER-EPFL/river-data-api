use super::*;

fn rep(index: i16, value: f64) -> Replicate {
    Replicate {
        index,
        value,
        flagged: false,
        withdrawn: false,
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
    let (current, proposed, delta, rows) = preview(&group, "sample", &change);
    assert_eq!(current.n, 3);
    assert!(close(current.mean, 20.0));
    assert!(close(current.sd, 10.0));
    assert_eq!(proposed.n, 2);
    assert!(close(proposed.mean, 15.0), "{proposed:?}");
    assert!(close(proposed.sd, 7.071_067_811_865_476));
    assert_eq!(delta.n, -1);
    assert!(close(delta.mean, -5.0));
    assert!(rows[2].included_now && !rows[2].included_after);
    assert_eq!(proposed.sd_estimator, "sample");
}

#[test]
fn restoring_a_flagged_replicate_brings_it_back() {
    let mut group = [rep(0, 10.0), rep(1, 20.0), rep(2, 30.0)];
    group[2].flagged = true;
    let change = Change {
        include: &[2],
        ..Change::default()
    };
    let (current, proposed, _, rows) = preview(&group, "sample", &change);
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
    let (current, proposed, _, _) = preview(&group, "sample", &change);
    assert_eq!(current.n, 2);
    assert_eq!(proposed.n, 2);
}

#[test]
fn switching_the_divisor_moves_only_the_sd() {
    let group = [rep(0, 10.0), rep(1, 20.0), rep(2, 30.0)];
    let change = Change {
        estimator: Some("population"),
        ..Change::default()
    };
    let (current, proposed, delta, _) = preview(&group, "sample", &change);
    assert!(close(current.sd, 10.0));
    // 10 * sqrt(2/3)
    assert!(close(proposed.sd, 8.164_965_809_277_26), "{proposed:?}");
    assert_eq!(proposed.sd_estimator, "population");
    assert!(close(delta.mean, 0.0));
    assert_eq!(delta.n, 0);
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
    let (current, proposed, _, _) = preview(&group, "sample", &change);
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
