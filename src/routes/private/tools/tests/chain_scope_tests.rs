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
fn a_calculation_narrows_the_findings_arm_and_binds_its_name() {
    let (sql, binds) = RecomputeScope {
        only_findings: true,
        calculation: Some("pco2".to_string()),
        ..Default::default()
    }
    .events_sql();
    assert!(sql.contains("AND h.tool = $1"), "{sql}");
    assert_eq!(binds.len(), 1);
}

#[test]
fn a_calculation_alone_is_not_a_scope() {
    assert!(
        !RecomputeScope {
            calculation: Some("pco2".to_string()),
            ..Default::default()
        }
        .is_bounded()
    );
}

/// The narrowing belongs to the findings arm alone: a site-and-range scope names its own visits,
/// and a calculation there would silently drop the ones it has raised nothing about.
#[test]
fn a_calculation_without_only_findings_adds_no_clause() {
    let (sql, binds) = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        calculation: Some("pco2".to_string()),
        ..Default::default()
    }
    .events_sql();
    assert!(!sql.contains("h.tool"), "{sql}");
    assert_eq!(binds.len(), 1, "the site is the only bind");
}

#[test]
fn each_term_adds_its_clause_with_binds_in_order() {
    let scope = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        only_findings: true,
        calculation: None,
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

/// The set a backfill asks for: every manual visit at a site, in a window, whether or not a
/// finding was ever raised there. A calculation that has never run raises nothing, so the
/// findings arm cannot reach the visits it has to compute at.
#[test]
fn a_site_and_range_without_only_findings_names_no_holds() {
    let scope = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        only_findings: false,
        calculation: None,
    };
    assert!(scope.is_bounded());
    let (sql, binds) = scope.events_sql();
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(sql.contains("ce.site_id = $1"));
    assert!(sql.contains("ce.collected_at >= $2"));
    assert!(sql.contains("ce.collected_at <= $3"));
    assert_eq!(binds.len(), 3);
}

/// A site alone is a scope: the whole history of entered visits there.
#[test]
fn a_site_alone_names_every_manual_visit_there() {
    let scope = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        ..Default::default()
    };
    let (sql, binds) = scope.events_sql();
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(!sql.contains("ce.collected_at >="), "{sql}");
    assert!(sql.contains("ce.site_id = $1"));
    assert_eq!(binds.len(), 1);
}
