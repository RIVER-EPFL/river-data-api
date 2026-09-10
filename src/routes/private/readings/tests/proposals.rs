#[test]
fn only_accept_and_reject_are_decisions() {
    assert!(super::parse_decision("accept").unwrap());
    assert!(!super::parse_decision("reject").unwrap());
    assert!(super::parse_decision("apply").is_err());
    assert!(super::parse_decision("").is_err());
}
