use sea_orm::sea_query::PostgresQueryBuilder;
use uuid::Uuid;

use crate::routes::private::tools::models::RecomputeScope;

fn events_sql(scope: &RecomputeScope) -> String {
    scope.events_query().to_string(PostgresQueryBuilder)
}

fn site() -> Uuid {
    Uuid::from_u128(1)
}

const SITE: &str = "'00000000-0000-0000-0000-000000000001'";

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
            site_id: Some(site()),
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

/// A synced visit is recomputed like any other (Q259), so no scope filters on the source.
#[test]
fn portal_sync_visits_are_in_every_scope() {
    let sql = events_sql(&RecomputeScope {
        only_findings: true,
        ..Default::default()
    });
    assert!(!sql.contains("source"), "{sql}");
}

#[test]
fn a_calculation_narrows_the_findings_arm_to_its_name() {
    let sql = events_sql(&RecomputeScope {
        only_findings: true,
        calculation: Some("pco2".to_string()),
        ..Default::default()
    });
    assert!(sql.contains(r#""h"."tool" = 'pco2'"#), "{sql}");
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
    let sql = events_sql(&RecomputeScope {
        site_id: Some(site()),
        calculation: Some("pco2".to_string()),
        ..Default::default()
    });
    assert!(!sql.contains(r#""h"."tool""#), "{sql}");
    assert!(!sql.contains("'pco2'"), "{sql}");
}

#[test]
fn each_term_adds_its_clause() {
    let scope = RecomputeScope {
        site_id: Some(site()),
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        only_findings: true,
        calculation: None,
        version: None,
        constant: None,
    };
    let sql = events_sql(&scope);
    assert!(
        sql.contains(&format!(r#""ce"."site_id" = {SITE}"#)),
        "{sql}"
    );
    assert!(
        sql.contains(r#""ce"."collected_at" >= '2025-06-01"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""ce"."collected_at" <= '2025-06-30"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""h"."kind" IN ('missing_output', 'stale_output', 'skipped_output')"#),
        "{sql}"
    );
    assert!(sql.contains(r#""h"."status" = 'pending'"#), "{sql}");
    assert!(
        sql.ends_with(r#"ORDER BY "ce"."collected_at" ASC"#),
        "{sql}"
    );
}

#[test]
fn a_range_alone_names_no_site() {
    let scope = RecomputeScope {
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        ..Default::default()
    };
    let sql = events_sql(&scope);
    assert!(!sql.contains(r#""ce"."site_id""#), "{sql}");
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(
        sql.contains(r#""ce"."collected_at" >= '2025-06-01"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""ce"."collected_at" <= '2025-06-30"#),
        "{sql}"
    );
}

/// The set a backfill asks for: every manual visit at a site, in a window, whether or not a
/// finding was ever raised there. A calculation that has never run raises nothing, so the
/// findings arm cannot reach the visits it has to compute at.
#[test]
fn a_site_and_range_without_only_findings_names_no_holds() {
    let scope = RecomputeScope {
        site_id: Some(site()),
        start: Some(at("2025-06-01T00:00:00Z")),
        end: Some(at("2025-06-30T00:00:00Z")),
        only_findings: false,
        calculation: None,
        version: None,
        constant: None,
    };
    assert!(scope.is_bounded());
    let sql = events_sql(&scope);
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(
        sql.contains(&format!(r#""ce"."site_id" = {SITE}"#)),
        "{sql}"
    );
    assert!(
        sql.contains(r#""ce"."collected_at" >= '2025-06-01"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""ce"."collected_at" <= '2025-06-30"#),
        "{sql}"
    );
}

/// A site alone is a scope: the whole history of entered visits there.
#[test]
fn a_site_alone_names_every_manual_visit_there() {
    let scope = RecomputeScope {
        site_id: Some(site()),
        ..Default::default()
    };
    let sql = events_sql(&scope);
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(!sql.contains(r#""ce"."collected_at" >="#), "{sql}");
    assert!(
        sql.contains(&format!(r#""ce"."site_id" = {SITE}"#)),
        "{sql}"
    );
}

/// A superseded script version is a bound of its own: the visits it produced values at are a
/// finite set the provenance names, which is what an author's migrate arm has to cover.
#[test]
fn a_superseded_version_is_a_scope_and_selects_by_provenance() {
    let version = Uuid::new_v4();
    let scope = RecomputeScope {
        version: Some(version),
        ..Default::default()
    };
    assert!(scope.is_bounded());
    let sql = events_sql(&scope);
    assert!(!sql.contains("replicate_audit_holds"), "{sql}");
    assert!(
        sql.contains(&format!(
            r#"(("r"."provenance" -> 'tool_version') ->> 'script_version_id') = '{version}'"#
        )),
        "{sql}"
    );
    assert!(
        sql.contains(r#""r"."collection_event_id" = "ce"."id""#),
        "{sql}"
    );
}

/// The version is a bound, not a narrowing of the findings arm: a migrate has to reach the visits
/// nothing has been raised about, which is most of them.
#[test]
fn a_version_narrows_a_site_scope_rather_than_replacing_it() {
    let scope = RecomputeScope {
        site_id: Some(site()),
        version: Some(Uuid::new_v4()),
        ..Default::default()
    };
    let sql = events_sql(&scope);
    assert!(
        sql.contains(&format!(r#""ce"."site_id" = {SITE}"#)),
        "{sql}"
    );
    assert!(sql.contains("'script_version_id'"), "{sql}");
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
    let sql = events_sql(&RecomputeScope {
        constant: Some("molar_mass_c".to_string()),
        ..Default::default()
    });
    assert!(
        sql.contains(r#"jsonb_exists("r"."provenance" -> 'constants', 'molar_mass_c')"#),
        "{sql}"
    );
}

/// A constant narrows a site-and-range scope rather than replacing it: a correction confined to
/// one site repairs that site's visits naming the constant and no others.
#[test]
fn a_constant_narrows_a_site_scope() {
    let sql = events_sql(&RecomputeScope {
        site_id: Some(site()),
        constant: Some("molar_mass_c".to_string()),
        ..Default::default()
    });
    assert!(
        sql.contains(&format!(r#""ce"."site_id" = {SITE}"#)),
        "{sql}"
    );
    assert!(sql.contains("'molar_mass_c'"), "{sql}");
}

/// The audit reports on the set the repair can repair, which holds synced visits now (Q259).
#[test]
fn the_audit_set_holds_portal_sync_visits_too() {
    let sql =
        crate::routes::private::tools::flows::audit_event_set(None, None, None, None).to_string();
    assert!(!sql.contains("source"), "{sql}");
}
