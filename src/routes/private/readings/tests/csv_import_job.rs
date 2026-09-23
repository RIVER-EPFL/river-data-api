use super::should_drop_staged;

#[test]
fn test_should_drop_staged_keeps_the_rows_for_a_retry() {
    assert!(!should_drop_staged(false, false));
}

#[test]
fn test_should_drop_staged_takes_them_on_the_last_failure_or_a_success() {
    assert!(should_drop_staged(false, true));
    assert!(should_drop_staged(true, false));
    assert!(should_drop_staged(true, true));
}
