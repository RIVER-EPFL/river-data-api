use crate::routes::private::tools::flows::audit_event_set;
use uuid::Uuid;

#[test]
fn an_unscoped_audit_covers_every_event_and_reads_no_provenance() {
    let (sql, binds) = audit_event_set(None, None, None, None);
    assert_eq!(
        sql,
        "SELECT id FROM collection_events ORDER BY collected_at"
    );
    assert!(binds.is_empty());
}

#[test]
fn a_constant_scope_narrows_to_the_events_whose_provenance_names_it() {
    let (sql, binds) = audit_event_set(None, None, Some("xO2"), None);
    assert!(
        sql.contains("jsonb_exists(r.provenance -> 'constants', $1)"),
        "{sql}"
    );
    assert_eq!(binds.len(), 1);
}

#[test]
fn a_site_scope_and_a_constant_scope_both_apply_and_bind_in_order() {
    let site = Uuid::new_v4();
    let (sql, binds) = audit_event_set(None, Some(site), Some("xO2"), None);
    assert!(sql.contains("site_id = $1"), "{sql}");
    assert!(sql.contains(", $2)"), "{sql}");
    assert_eq!(binds.len(), 2);
}

/// A calculation edit audits the visits that calculation actually wrote, which is what makes
/// the report proportionate to the edit rather than a pass over every visit ever recorded.
#[test]
fn a_calculation_scope_narrows_to_the_visits_it_wrote() {
    let (sql, binds) = audit_event_set(None, None, None, Some("pco2"));
    assert!(
        sql.contains("r.provenance ->> 'tool' = $1"),
        "the scope reads the stored provenance: {sql}"
    );
    assert_eq!(binds.len(), 1);
}

/// The two content scopes stack, so editing a constant a calculation reads audits only where
/// both are named.
#[test]
fn a_constant_and_a_calculation_scope_both_apply_and_bind_in_order() {
    let (sql, binds) = audit_event_set(None, None, Some("xO2"), Some("pco2"));
    assert!(sql.contains("jsonb_exists(r.provenance -> 'constants', $1)"));
    assert!(sql.contains("r.provenance ->> 'tool' = $2"));
    assert_eq!(binds.len(), 2);
}

#[test]
fn an_event_scope_outranks_a_site_scope() {
    let (sql, _) = audit_event_set(Some(Uuid::new_v4()), Some(Uuid::new_v4()), None, None);
    assert!(sql.contains("id = $1"), "{sql}");
    assert!(!sql.contains("site_id"), "{sql}");
}
