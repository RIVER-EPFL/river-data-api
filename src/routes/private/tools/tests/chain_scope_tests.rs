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
        version: None,
        constant: None,
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
        version: None,
        constant: None,
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

/// A superseded script version is a bound of its own: the visits it produced values at are a
/// finite set the provenance names, which is what an author's migrate arm has to cover.
#[test]
fn a_superseded_version_is_a_scope_and_selects_by_provenance() {
    let scope = RecomputeScope {
        version: Some(Uuid::new_v4()),
        ..Default::default()
    };
    assert!(scope.is_bounded());
    let (sql, binds) = scope.events_sql();
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(
        sql.contains("'tool_version' ->> 'script_version_id' = $1"),
        "{sql}"
    );
    assert!(sql.contains("r.collection_event_id = ce.id"), "{sql}");
    assert_eq!(binds.len(), 1);
}

/// The version is a bound, not a narrowing of the findings arm: a migrate has to reach the visits
/// nothing has been raised about, which is most of them.
#[test]
fn a_version_narrows_a_site_scope_rather_than_replacing_it() {
    let scope = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        version: Some(Uuid::new_v4()),
        ..Default::default()
    };
    let (sql, binds) = scope.events_sql();
    assert!(sql.contains("ce.site_id = $1"), "{sql}");
    assert!(sql.contains("script_version_id' = $2"), "{sql}");
    assert_eq!(binds.len(), 2);
}

/// Scenario: a constant's value is corrected, so every visit whose stored provenance names that
/// constant holds a value computed from the old one.
///
/// Expected behaviour: the constant is a bound in its own right. It names a set of visits without
/// a site or a range, which is what lets the correction repair exactly what it moved rather than
/// waiting for a person to name a window.
#[test]
fn a_constant_is_a_scope_on_its_own() {
    assert!(
        RecomputeScope {
            constant: Some("molar_mass_c".to_string()),
            ..Default::default()
        }
        .is_bounded()
    );
}

#[test]
fn a_constant_selects_the_visits_whose_provenance_names_it() {
    let (sql, binds) = RecomputeScope {
        constant: Some("molar_mass_c".to_string()),
        ..Default::default()
    }
    .events_sql();
    assert!(sql.contains("jsonb_exists"), "{sql}");
    assert!(sql.contains("'constants'"), "{sql}");
    assert!(sql.contains("$1"), "{sql}");
    assert_eq!(binds.len(), 1);
}

/// A constant narrows a site-and-range scope rather than replacing it: a correction confined to
/// one site repairs that site's visits naming the constant and no others.
#[test]
fn a_constant_narrows_a_site_scope_and_binds_after_it() {
    let (sql, binds) = RecomputeScope {
        site_id: Some(Uuid::new_v4()),
        constant: Some("molar_mass_c".to_string()),
        ..Default::default()
    }
    .events_sql();
    assert!(sql.contains("ce.site_id = $1"), "{sql}");
    assert!(sql.contains("$2"), "{sql}");
    assert_eq!(binds.len(), 2);
}

/// The audit reports on the set the repair can repair: a synced visit is the portal's, so neither
/// covers one (Q175).
#[test]
fn the_audit_set_excludes_portal_sync_visits_too() {
    let sql = crate::routes::private::tools::flows::audit_event_set(None, None, None, None)
        .to_string();
    assert!(sql.contains("portal_sync"), "{sql}");
}
