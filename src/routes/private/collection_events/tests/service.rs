use super::*;

#[test]
fn test_paging_is_opt_in() {
    assert!(paging(None, None).is_none());
    assert_eq!(paging(Some(3), None).unwrap().limit, MAX_PAGE_SIZE);
    assert_eq!(paging(None, Some(10)).unwrap().limit, 10);
    assert_eq!(paging(Some(0), Some(500)).unwrap().page(), 1);
    assert_eq!(paging(Some(0), Some(500)).unwrap().limit, MAX_PAGE_SIZE);
}

#[test]
fn test_visit_list_order_defaults_and_refuses_unknown() {
    assert_eq!(
        visit_list_order(None, None).unwrap(),
        "ce.collected_at DESC, ce.collected_at DESC, ce.id"
    );
    assert_eq!(
        visit_list_order(Some("findings_open"), Some("asc")).unwrap(),
        "findings_open ASC, ce.collected_at DESC, ce.id"
    );
    assert!(visit_list_order(Some("notes"), None).is_err());
    assert!(visit_list_order(None, Some("random")).is_err());
}

#[test]
fn an_active_job_is_the_state_whatever_the_findings_say() {
    assert_eq!(visit_status(Some("queued"), true), "queued");
    assert_eq!(visit_status(Some("pending"), false), "queued");
    assert_eq!(visit_status(Some("retrying"), true), "queued");
    assert_eq!(visit_status(Some("running"), true), "running");
}

#[test]
fn a_failed_repair_outranks_a_stale_finding_and_a_finished_one_defers_to_it() {
    assert_eq!(visit_status(Some("failed"), true), "failed");
    assert_eq!(visit_status(Some("failed"), false), "failed");
    assert_eq!(visit_status(Some("completed"), true), "stale");
    assert_eq!(visit_status(Some("cancelled"), true), "stale");
    assert_eq!(visit_status(Some("completed"), false), "current");
    assert_eq!(visit_status(None, false), "current");
    assert_eq!(visit_status(None, true), "stale");
}

#[test]
fn the_dedupe_key_is_one_per_visit() {
    let id = uuid::Uuid::nil();
    assert_eq!(dedupe_key(id), format!("event_recompute:{id}"));
}
