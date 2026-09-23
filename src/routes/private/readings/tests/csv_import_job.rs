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

#[test]
fn test_orphaned_tokens_keeps_what_a_live_import_will_read() {
    use super::orphaned_tokens;
    use uuid::Uuid;
    let (live, dead, gone) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    assert_eq!(
        orphaned_tokens(&[live, dead, gone], &[live]),
        vec![dead, gone]
    );
    assert!(orphaned_tokens(&[live], &[live]).is_empty());
    assert!(orphaned_tokens(&[], &[live]).is_empty());
}
