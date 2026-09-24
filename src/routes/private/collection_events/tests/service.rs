use super::*;

#[test]
fn test_paging_is_opt_in() {
    assert!(paging(None, None).is_none());
    assert_eq!(paging(Some(3), None).unwrap().limit, MAX_PAGE_SIZE);
    assert_eq!(paging(None, Some(10)).unwrap().limit, 10);
    assert_eq!(paging(Some(0), Some(500)).unwrap().page(), 1);
    assert_eq!(paging(Some(0), Some(500)).unwrap().limit, MAX_PAGE_SIZE);
}

fn order_by_of(order: &[(Expr, Order)]) -> String {
    let mut query = visits_where(Condition::all());
    query.column((ce(), super::super::Column::Id));
    for (expr, direction) in order {
        query.order_by_expr(expr.clone(), direction.clone());
    }
    let sql = query.to_string(PostgresQueryBuilder);
    sql[sql.find("ORDER BY").expect("an ORDER BY")..].to_string()
}

#[test]
fn test_visit_list_order_defaults_and_refuses_unknown() {
    assert_eq!(
        order_by_of(&visit_list_order(None, None).unwrap()),
        r#"ORDER BY "ce"."collected_at" DESC, "ce"."collected_at" DESC, "ce"."id" ASC"#
    );
    assert_eq!(
        order_by_of(&visit_list_order(Some("findings_open"), Some("asc")).unwrap()),
        r#"ORDER BY "findings_open" ASC, "ce"."collected_at" DESC, "ce"."id" ASC"#
    );
    assert!(visit_list_order(Some("notes"), None).is_err());
    assert!(visit_list_order(None, Some("random")).is_err());
}

#[test]
fn test_visit_headers_bind_the_bounds_and_the_page() {
    let site = Uuid::from_u128(7);
    let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let window = paging(Some(3), Some(20)).unwrap();
    let filter = visit_filter(
        Some(site),
        Some(vec![Uuid::from_u128(9)]),
        Some(start),
        None,
    );
    let (sql, values) = visit_headers(filter, Some(window)).build(PostgresQueryBuilder);
    assert!(sql.contains(r#""ce"."site_id" = $1"#), "{sql}");
    assert!(sql.contains(r#""s"."project_id" IN ($2)"#), "{sql}");
    assert!(sql.contains(r#""ce"."collected_at" >= $3"#), "{sql}");
    assert!(sql.ends_with("LIMIT $4 OFFSET $5"), "{sql}");
    assert_eq!(values.0.len(), 5);
}

#[test]
fn test_visit_headers_without_bounds_list_every_visit() {
    let (sql, values) =
        visit_headers(visit_filter(None, None, None, None), None).build(PostgresQueryBuilder);
    assert!(
        sql.ends_with(r#"ON "s"."id" = "ce"."site_id" WHERE TRUE"#),
        "{sql}"
    );
    assert!(values.0.is_empty());
}

#[test]
fn test_holding_counts_each_named_parameter_once() {
    assert!(holding(&[]).is_none());
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let filter = visit_filter(None, None, None, None).add_option(holding(&[a, b, a]));
    let (sql, values) = visit_headers(filter, None).build(PostgresQueryBuilder);
    assert!(
        sql.contains(r#""ce"."id" IN (SELECT "readings"."collection_event_id""#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"HAVING COUNT(DISTINCT "readings"."parameter_id") = $4"#),
        "{sql}"
    );
    // a, b, the unflagged false and the count of two
    assert_eq!(values.0.len(), 4);
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

#[test]
fn test_stage_visit_statement_upserts_on_the_slot_and_returns_whether_it_inserted() {
    let site = Uuid::from_u128(1);
    let at = DateTime::parse_from_rfc3339("2026-06-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let (sql, _) = stage_visit_statement(site, at, "alice", None, true).build(PostgresQueryBuilder);
    assert!(sql.starts_with(r#"INSERT INTO "collection_events" ("site_id", "collected_at", "source", "created_by", "notes", "unverified")"#), "{sql}");
    assert!(
        sql.contains(r#"ON CONFLICT ("site_id", "collected_at") DO UPDATE SET "site_id" = "excluded"."site_id""#),
        "{sql}"
    );
    assert!(
        sql.ends_with(r#"RETURNING "id", "site_id", "collected_at", "source", "created_by", "notes", "unverified", "xmax" = $7"#),
        "{sql}"
    );
}

#[test]
fn test_newest_job_per_visit_keeps_the_latest_run_of_each() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap();
    let row = |id: &str, status: &str, when: &str| RecomputeJobRow {
        event_id: Some(id.to_string()),
        status: status.to_string(),
        created_at: at(when),
    };
    let latest = newest_job_per_visit([
        row(&a.to_string(), "completed", "2026-06-01T10:00:00Z"),
        row(&b.to_string(), "failed", "2026-06-01T09:00:00Z"),
        row(&a.to_string(), "failed", "2026-06-01T08:00:00Z"),
        row(&a.to_string(), "running", "2026-06-01T11:00:00Z"),
        row("not-a-uuid", "queued", "2026-06-01T12:00:00Z"),
        RecomputeJobRow {
            event_id: None,
            status: "queued".to_string(),
            created_at: at("2026-06-01T12:00:00Z"),
        },
    ]);
    assert_eq!(latest.len(), 2);
    assert_eq!(latest[&a], "running");
    assert_eq!(latest[&b], "failed");
}

#[test]
fn test_newest_job_per_visit_of_no_runs_is_empty() {
    assert!(newest_job_per_visit([]).is_empty());
}

#[test]
fn test_cell_curves_is_each_distinct_curve_in_replicate_order() {
    let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let names =
        std::collections::HashMap::from([(a, Some("Curve 2026-03".to_string())), (b, None)]);
    let curves = cell_curves(&[b, a, b], &names);
    let got: Vec<(Uuid, Option<&str>)> = curves.iter().map(|c| (c.id, c.name.as_deref())).collect();
    assert_eq!(got, vec![(b, None), (a, Some("Curve 2026-03"))]);
}

#[test]
fn test_cell_curves_is_empty_for_an_uncorrected_cell() {
    assert!(cell_curves(&[], &std::collections::HashMap::new()).is_empty());
}
