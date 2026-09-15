use crate::routes::private::collection_events::service::PORTAL_SYNC;
use crate::routes::private::tools::flows::audit_event_set;
use uuid::Uuid;

/// A synced visit is the portal's and the repair refuses one (Q41), so every scope excludes it
/// and its value is the first bind (Q175).
#[test]
fn an_unscoped_audit_covers_every_visit_but_the_synced_ones() {
    let statement = audit_event_set(None, None, None, None);
    assert_eq!(
        statement.sql,
        r#"SELECT "id" FROM "collection_events" WHERE "source" <> $1 ORDER BY "collected_at" ASC"#
    );
    let values = statement.values.expect("the excluded source is bound").0;
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].to_string(), format!("'{PORTAL_SYNC}'"));
}

#[test]
fn a_constant_scope_narrows_to_the_events_whose_provenance_names_it() {
    let statement = audit_event_set(None, None, Some("xO2"), None);
    assert!(
        statement
            .sql
            .contains(r#"jsonb_exists("r"."provenance" -> 'constants', $2)"#),
        "{}",
        statement.sql
    );
    assert_eq!(statement.values.map(|v| v.0.len()), Some(2));
}

#[test]
fn a_site_scope_and_a_constant_scope_both_apply_and_bind_in_order() {
    let site = Uuid::new_v4();
    let statement = audit_event_set(None, Some(site), Some("xO2"), None);
    assert!(
        statement.sql.contains(r#""site_id" = $2"#),
        "{}",
        statement.sql
    );
    assert!(statement.sql.contains(", $3)"), "{}", statement.sql);
    assert_eq!(statement.values.map(|v| v.0.len()), Some(3));
}

/// A calculation edit audits the visits that calculation actually wrote, which is what makes
/// the report proportionate to the edit rather than a pass over every visit ever recorded.
#[test]
fn a_calculation_scope_narrows_to_the_visits_it_wrote() {
    let statement = audit_event_set(None, None, None, Some("pco2"));
    assert!(
        statement
            .sql
            .contains(r#""r"."provenance" ->> 'tool' = $2"#),
        "the scope reads the stored provenance: {}",
        statement.sql
    );
    assert_eq!(statement.values.map(|v| v.0.len()), Some(2));
}

/// The two content scopes stack, so editing a constant a calculation reads audits only where
/// both are named.
#[test]
fn a_constant_and_a_calculation_scope_both_apply_and_bind_in_order() {
    let statement = audit_event_set(None, None, Some("xO2"), Some("pco2"));
    assert!(
        statement
            .sql
            .contains(r#"jsonb_exists("r"."provenance" -> 'constants', $2)"#)
    );
    assert!(
        statement
            .sql
            .contains(r#""r"."provenance" ->> 'tool' = $3"#)
    );
    assert_eq!(statement.values.map(|v| v.0.len()), Some(3));
}

#[test]
fn an_event_scope_outranks_a_site_scope() {
    let statement = audit_event_set(Some(Uuid::new_v4()), Some(Uuid::new_v4()), None, None);
    assert!(statement.sql.contains(r#""id" = $2"#), "{}", statement.sql);
    assert!(!statement.sql.contains("site_id"), "{}", statement.sql);
}

/// The correlation is what keeps the scope a per-visit index probe rather than a scan.
#[test]
fn a_content_scope_correlates_the_subquery_to_the_visit() {
    let statement = audit_event_set(None, None, Some("xO2"), None);
    assert!(
        statement
            .sql
            .contains(r#""r"."collection_event_id" = "collection_events"."id""#),
        "{}",
        statement.sql
    );
}
