use super::*;

// 0.15 of the stored rows, and 0.5 of a replicate index's groups. A pass that reshapes fewer
// than RECONCILE_BRAKE_MIN_ROWS rows is never braked, whatever the fractions say.

#[test]
fn test_brake_verdict_below_the_floor_is_never_braked() {
    // 4 of 5 rows is 80%, and every group loses its index, but the floor is not reached.
    assert_eq!(brake_verdict(5, 4, &[(4, 5)]), None);
}

#[test]
fn test_brake_verdict_at_the_floor_the_fractions_apply() {
    assert_eq!(brake_verdict(5, 5, &[]), Some(Brake::Fraction));
}

#[test]
fn test_brake_verdict_at_the_row_fraction_passes() {
    // 15 of 100 is exactly the threshold, which is not over it.
    assert_eq!(brake_verdict(100, 15, &[]), None);
}

#[test]
fn test_brake_verdict_one_row_over_the_fraction_brakes() {
    assert_eq!(brake_verdict(100, 16, &[]), Some(Brake::Fraction));
}

#[test]
fn test_brake_verdict_at_the_index_fraction_passes() {
    // Half of the groups carrying index 1 lose it, which is not more than half.
    assert_eq!(brake_verdict(100, 5, &[(5, 10)]), None);
}

#[test]
fn test_brake_verdict_one_group_over_the_index_fraction_brakes() {
    assert_eq!(brake_verdict(100, 6, &[(6, 10)]), Some(Brake::IndexLoss));
}

#[test]
fn test_brake_verdict_index_loss_is_seen_where_the_row_fraction_is_not() {
    // 6 of 200 rows is 3%, well under the row fraction, but they are every group's index 1.
    assert_eq!(brake_verdict(200, 6, &[(6, 6)]), Some(Brake::IndexLoss));
}

#[test]
fn test_brake_verdict_index_loss_is_per_index_not_pooled() {
    // Neither index loses more than half, though together they are half the withdrawals.
    assert_eq!(brake_verdict(200, 10, &[(5, 10), (5, 10)]), None);
}

#[test]
fn test_brake_verdict_an_empty_window_has_no_row_fraction() {
    // Nothing stored is nothing to reshape; only new rows can be in such a pass.
    assert_eq!(brake_verdict(0, 9, &[]), None);
}

mod dishonest_window {
    use super::super::refuse_dishonest_window;
    use river_data_core::models::SourceWindow;

    fn window(source_rows_read: u64) -> SourceWindow {
        SourceWindow {
            from: chrono::Utc::now() - chrono::Duration::days(1),
            to: chrono::Utc::now(),
            source_rows_read,
            dropped_times: Vec::new(),
            content_digest: None,
        }
    }

    #[test]
    fn test_a_window_the_store_holds_nothing_for_is_never_refused() {
        assert!(refuse_dishonest_window(&window(0), 0, 0).is_ok());
        assert!(refuse_dishonest_window(&window(12), 0, 0).is_ok());
    }

    #[test]
    fn test_an_empty_payload_claiming_source_rows_is_not_read_as_a_deletion() {
        let err = refuse_dishonest_window(&window(12), 0, 30).expect_err("refused");
        assert!(
            err.to_string().contains("never read as a deletion"),
            "{err}"
        );
    }

    #[test]
    fn test_a_window_claiming_no_source_rows_over_stored_readings_is_refused() {
        let err = refuse_dishonest_window(&window(0), 0, 30).expect_err("refused");
        assert!(err.to_string().contains("zero source rows"), "{err}");
    }

    #[test]
    fn test_a_window_asserting_its_content_passes_however_little_it_admits() {
        assert!(refuse_dishonest_window(&window(12), 1, 30).is_ok());
        assert!(refuse_dishonest_window(&window(12), 12, 3).is_ok());
    }
}
