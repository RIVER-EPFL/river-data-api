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

/// Q41: the chain runs at a visit a person made and not at one the sync made, and every door asks
/// this one question rather than spelling the source out for itself.
#[test]
fn a_synced_visit_is_the_one_source_the_chain_stays_out_of() {
    assert!(chain_may_run("manual"));
    assert!(!chain_may_run(PORTAL_SYNC));
    // A source nobody writes today is not a reason to withhold the chain.
    assert!(chain_may_run("csv_import"));
}

#[test]
fn test_a_calculation_shows_its_repair_while_it_runs_and_when_it_failed() {
    assert_eq!(calculation_repair(Some("queued")), Some("queued"));
    assert_eq!(calculation_repair(Some("retrying")), Some("queued"));
    assert_eq!(calculation_repair(Some("running")), Some("running"));
    assert_eq!(calculation_repair(Some("failed")), Some("failed"));
}

#[test]
fn test_a_finished_or_absent_repair_leaves_the_findings_to_speak() {
    assert_eq!(calculation_repair(Some("completed")), None);
    assert_eq!(calculation_repair(Some("cancelled")), None);
    assert_eq!(calculation_repair(None), None);
}

#[test]
fn test_a_parameter_names_every_calculation_reading_it_and_the_one_writing_it() {
    use crate::routes::private::tools::models::{CalculationImpact, ImpactParameter};

    let dic = Uuid::from_u128(1);
    let pco2 = Uuid::from_u128(2);
    let param = |id: Uuid| ImpactParameter {
        parameter_id: id,
        parameter_code: String::new(),
    };
    let impact = |tool: &str, reads: Vec<Uuid>, outputs: Vec<Uuid>| CalculationImpact {
        tool: tool.to_string(),
        label: tool.to_string(),
        reads: reads.into_iter().map(param).collect(),
        outputs: outputs.into_iter().map(param).collect(),
    };
    let impacts = [
        impact("carbonate", vec![dic], vec![pco2]),
        impact("flux", vec![dic, pco2], vec![]),
    ];
    assert_eq!(
        parameter_roles(&impacts, dic),
        (vec!["carbonate".to_string(), "flux".to_string()], None)
    );
    assert_eq!(
        parameter_roles(&impacts, pco2),
        (vec!["flux".to_string()], Some("carbonate".to_string()))
    );
    assert_eq!(
        parameter_roles(&impacts, Uuid::from_u128(3)),
        (vec![], None)
    );
}
