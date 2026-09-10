use crate::routes::private::tools::models::RecomputeScope;
use uuid::Uuid;

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .with_timezone(&chrono::Utc)
}

#[test]
fn an_empty_scope_is_not_bounded_and_any_single_term_is() {
    assert!(!RecomputeScope::default().is_bounded());
    assert!(
        RecomputeScope {
            site_id: Some(Uuid::new_v4()),
            ..Default::default()
        }
        .is_bounded()
    );
    assert!(
        RecomputeScope {
            start: Some(at("2025-06-01T00:00:00Z")),
            ..Default::default()
        }
        .is_bounded()
    );
    assert!(
        RecomputeScope {
            end: Some(at("2025-06-01T00:00:00Z")),
            ..Default::default()
        }
        .is_bounded()
    );
    assert!(
        RecomputeScope {
            only_findings: true,
            ..Default::default()
        }
        .is_bounded()
    );
}

#[test]
fn portal_sync_visits_are_excluded_whatever_the_scope() {
    let (sql, binds) = RecomputeScope {
        only_findings: true,
        ..Default::default()
    }
    .events_sql();
    assert!(sql.contains("ce.source <> 'portal_sync'"));
    assert!(binds.is_empty());
}

#[test]
fn each_term_adds_its_clause_with_binds_in_order() {
    let scope = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        only_findings: true,
    };
    let (sql, binds) = scope.events_sql();
    assert!(sql.contains("ce.site_id = $1"));
    assert!(sql.contains("ce.collected_at >= $2"));
    assert!(sql.contains("ce.collected_at <= $3"));
    assert_eq!(binds.len(), 3);
    assert!(sql.contains("h.kind IN ('missing_output', 'stale_output', 'skipped_output')"));
    assert!(sql.contains("h.status = 'pending'"));
    assert!(sql.trim_end().ends_with("ORDER BY ce.collected_at"));
}

#[test]
fn a_range_alone_binds_two_and_names_no_site() {
    let scope = RecomputeScope {
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        ..Default::default()
    };
    let (sql, binds) = scope.events_sql();
    assert!(!sql.contains("ce.site_id"));
    assert!(!sql.contains("replicate_audit_holds"));
    assert!(sql.contains(">= $1") && sql.contains("<= $2"));
    assert_eq!(binds.len(), 2);
}
