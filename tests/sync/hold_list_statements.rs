//! The review queue's three statements, against the SQL they were spelled as.
//!
//! The counts, the page and the per-kind breakdown all read one filter set, so the only thing that
//! has to hold through the conversion is that the built statements answer what the text answered,
//! for every filter the queue takes and every hold shape it carries. The texts below are the
//! implementation as it stood before the conversion; the database evaluates both over the same
//! rows.
//!
//! Run: cargo test --test sync hold_list_statements -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, FromQueryResult, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use river_db::common::authz::AccessScope;
use river_db::routes::private::sync::models::{HoldRow, ListHoldsQuery};
use river_db::routes::private::sync::service as svc;

const PAIRED_STREAM: &str = "00000000-0000-4000-c000-0000000000a1";
const UNPAIRED_STREAM: &str = "00000000-0000-4000-c000-0000000000a2";
const OTHER_PROJECT: &str = "00000000-0000-4000-a000-0000000009f1";
const OTHER_SITE: &str = "00000000-0000-4000-a000-0000000009f2";

// --- The text spelling ---

const SCALE: &str = "GREATEST(\
     abs(COALESCE((h.expected->>'mean')::float8, 0)), \
     abs(COALESCE((h.computed->>'mean')::float8, 0)), \
     1e-9)";

fn relative_delta() -> String {
    format!(
        "GREATEST(COALESCE(abs((h.delta->>'mean')::float8), 0), \
          COALESCE(abs((h.delta->>'sd')::float8), 0)) / {SCALE}"
    )
}

fn relative_delta_of(statistic: &str) -> String {
    format!("COALESCE(abs((h.delta->>'{statistic}')::float8), 0) / {SCALE}")
}

fn text_counts(base: &str, status: &str) -> String {
    format!(
        "SELECT COUNT(*) FILTER (WHERE {status})::bigint AS total,
                COUNT(*) FILTER (WHERE h.status = 'pending')::bigint AS pending,
                COUNT(*) FILTER (WHERE h.status = 'deferred')::bigint AS deferred
         FROM replicate_audit_holds h
         LEFT JOIN data_streams ds ON ds.id = h.stream_id
         WHERE {base}"
    )
}

fn text_kind_counts(base: &str) -> String {
    format!(
        "SELECT h.kind, COUNT(*)::bigint AS n
         FROM replicate_audit_holds h
         LEFT JOIN data_streams ds ON ds.id = h.stream_id
         WHERE {base} AND h.status = 'pending'
         GROUP BY h.kind"
    )
}

fn text_list(base: &str, status: &str, order_by: &str, limit: u64, offset: u64) -> String {
    let relative = relative_delta();
    let mean = relative_delta_of("mean");
    let sd = relative_delta_of("sd");
    format!(
        "SELECT h.id, h.stream_id, h.kind, ds.source_system, ds.source_key, ds.source_name,
                COALESCE(s.id, es.id) AS site_id,
                COALESCE(ds.site_parameter_id, esp.id) AS site_parameter_id,
                COALESCE(s.name, es.name) AS site_name,
                COALESCE(p.name, ep.name) AS parameter_name,
                COALESCE(p.code, ep.code) AS parameter_code,
                h.tool,
                COALESCE(ds.site_parameter_id IS NOT NULL, FALSE) AS paired,
                h.group_time,
                h.expected, h.computed, h.delta, h.status,
                ''::text AS classification,
                h.resolution,
                h.created_at, h.acknowledged_by, h.acknowledged_at,
                {relative} AS relative_delta,
                {mean} AS mean_relative_delta,
                {sd} AS sd_relative_delta
         FROM replicate_audit_holds h
         LEFT JOIN data_streams ds ON ds.id = h.stream_id
         LEFT JOIN site_parameters sp ON sp.id = ds.site_parameter_id
         LEFT JOIN sites s ON s.id = sp.site_id
         LEFT JOIN parameters p ON p.id = sp.parameter_id
         LEFT JOIN sites es ON es.id = h.site_id
         LEFT JOIN parameters ep ON ep.id = h.parameter_id
         LEFT JOIN site_parameters esp ON esp.site_id = h.site_id AND esp.parameter_id = h.parameter_id
         WHERE {base} AND {status}
         ORDER BY {order_by}
         LIMIT {limit} OFFSET {offset}"
    )
}

// --- The rows the statements are run over ---

/// A hold as the queue can carry it: kind, status, the three documents, and the slot it names.
struct Hold {
    kind: &'static str,
    status: &'static str,
    stream: Option<&'static str>,
    site: Option<&'static str>,
    parameter: Option<&'static str>,
    expected: serde_json::Value,
    computed: serde_json::Value,
    delta: serde_json::Value,
}

fn shapes() -> Vec<Hold> {
    let stats = |status, expected, computed, delta| Hold {
        kind: "replicate_stats",
        status,
        stream: Some(PAIRED_STREAM),
        site: None,
        parameter: None,
        expected,
        computed,
        delta,
    };
    vec![
        // A plain disagreement the signature does not explain.
        stats(
            "pending",
            json!({"n": 3, "mean": 150.0, "sd": 2.0}),
            json!({"n": 3, "mean": 150.4, "sd": 2.5}),
            json!({"mean": -0.4, "sd": -0.5}),
        ),
        // The n divisor, which the classification filters partition on.
        stats(
            "pending",
            json!({"n": 4, "mean": 10.0, "sd": 1.732_050_807_568_877_2}),
            json!({"n": 4, "mean": 10.0, "sd": 2.0}),
            json!({"sd": -0.267_949_192_431_122_8}),
        ),
        // A disagreement the mean carries alone, which orders differently from one the sd carries.
        stats(
            "pending",
            json!({"n": 3, "mean": 10.0, "sd": 1.0}),
            json!({"n": 3, "mean": 10.5, "sd": 1.0}),
            json!({"mean": -0.5}),
        ),
        // A count mismatch, and a hold missing a statistic: both leave the signature NULL.
        stats(
            "deferred",
            json!({"n": 2, "mean": 5.0, "sd": 1.0}),
            json!({"n": 3, "mean": 5.0, "sd": 1.0}),
            json!({"n": -1}),
        ),
        stats(
            "acknowledged",
            json!({"n": 3, "mean": 7.0}),
            json!({"n": 3, "mean": 7.1, "sd": 0.5}),
            json!({"mean": -0.1}),
        ),
        // A zero mean, which the scale floor carries.
        stats(
            "remediated",
            json!({"n": 3, "mean": 0.0, "sd": 0.0}),
            json!({"n": 3, "mean": 0.0, "sd": 0.0}),
            json!({}),
        ),
        // An unpaired stream: no slot labels, and out of a restricted caller's reach.
        Hold {
            kind: "replicate_stats",
            status: "deferred",
            stream: Some(UNPAIRED_STREAM),
            site: None,
            parameter: None,
            expected: json!({"n": 3, "mean": 12.0, "sd": 1.0}),
            computed: json!({"n": 3, "mean": 12.9, "sd": 1.4}),
            delta: json!({"mean": -0.9, "sd": -0.4}),
        },
        // Findings no stream produced, keyed on the slot itself.
        Hold {
            kind: "missing_output",
            status: "pending",
            stream: None,
            site: Some(crate::common::SITE2_ID),
            parameter: Some(crate::common::GLOBAL_PARAM_DO_ID),
            expected: json!({}),
            computed: json!({}),
            delta: json!({}),
        },
        Hold {
            kind: "stale_output",
            status: "pending",
            stream: None,
            site: Some(OTHER_SITE),
            parameter: Some(crate::common::GLOBAL_PARAM_COND_ID),
            expected: json!({}),
            computed: json!({}),
            delta: json!({}),
        },
    ]
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn seed(db: &DatabaseConnection) {
    crate::common::seed_test_data(db).await;
    exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{OTHER_PROJECT}', 'Other project', 'Out of the caller''s reach', 'test')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name) \
             VALUES ('{OTHER_SITE}', '{OTHER_PROJECT}', 'Other site')"
        ),
    )
    .await;
    exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, \
                                       site_parameter_id, created_at) \
             VALUES ('{PAIRED_STREAM}', 'listsrc', 'paired', 'Paired stream', '{sp}', NOW()), \
                    ('{UNPAIRED_STREAM}', 'othersrc', 'unpaired', 'Unpaired stream', NULL, NOW())",
            sp = crate::common::PARAM_S1_TEMP_ID
        ),
    )
    .await;
    for (n, hold) in shapes().into_iter().enumerate() {
        let id = Uuid::new_v4();
        let quoted = |v: Option<&str>| v.map_or("NULL".to_string(), |v| format!("'{v}'"));
        exec(
            db,
            &format!(
                "INSERT INTO replicate_audit_holds \
                   (id, stream_id, site_id, parameter_id, group_time, expected, computed, delta, \
                    status, kind, created_at) \
                 VALUES ('{id}', {stream}, {site}, {parameter}, \
                         TIMESTAMPTZ '2025-06-01T00:00:00Z' + make_interval(hours => {n}), \
                         '{expected}'::jsonb, '{computed}'::jsonb, '{delta}'::jsonb, \
                         '{status}', '{kind}', \
                         TIMESTAMPTZ '2025-06-01T00:00:00Z' + make_interval(hours => {n}))",
                stream = quoted(hold.stream),
                site = quoted(hold.site),
                parameter = quoted(hold.parameter),
                expected = hold.expected,
                computed = hold.computed,
                delta = hold.delta,
                status = hold.status,
                kind = hold.kind,
            ),
        )
        .await;
    }
}

// --- The comparison ---

#[derive(FromQueryResult)]
struct Counts {
    total: i64,
    pending: i64,
    deferred: i64,
}

#[derive(FromQueryResult)]
struct KindCount {
    kind: String,
    n: i64,
}

fn query(params: serde_json::Value) -> ListHoldsQuery {
    serde_json::from_value(params).expect("query")
}

/// One case: the query the route takes, and the same filters, status view and order as text.
struct Case {
    label: &'static str,
    scope: AccessScope,
    query: ListHoldsQuery,
    base: String,
    status: String,
    order_by: String,
}

fn cases() -> Vec<Case> {
    let pending = "h.status = 'pending'".to_string();
    let case = |label, query: serde_json::Value, base: &str, status: &str, order_by: &str| Case {
        label,
        scope: AccessScope::Unrestricted,
        query: self::query(query),
        base: base.to_string(),
        status: status.to_string(),
        order_by: order_by.to_string(),
    };
    let relative = relative_delta();
    vec![
        case("default", json!({}), "TRUE", &pending, "h.created_at DESC"),
        case(
            "resolved view",
            json!({"status": "resolved"}),
            "TRUE",
            "h.status IN ('acknowledged', 'remediated', 'superseded', 'use_portal', \
             'use_manual', 'consumed')",
            "h.created_at DESC",
        ),
        case(
            "deferred view, largest first",
            json!({"status": "deferred", "sort": "relative_delta_desc"}),
            "TRUE",
            "h.status = 'deferred'",
            &format!("{relative} DESC, h.created_at DESC"),
        ),
        case(
            "smallest first",
            json!({"sort": "relative_delta_asc"}),
            "TRUE",
            &pending,
            &format!("{relative} ASC, h.created_at DESC"),
        ),
        case(
            "one stream",
            json!({"stream_id": PAIRED_STREAM}),
            &format!("h.stream_id = '{PAIRED_STREAM}'"),
            &pending,
            "h.created_at DESC",
        ),
        case(
            "several streams",
            json!({"stream_ids": format!("{PAIRED_STREAM}, {UNPAIRED_STREAM}")}),
            &format!("h.stream_id = ANY(ARRAY['{PAIRED_STREAM}', '{UNPAIRED_STREAM}']::uuid[])"),
            &pending,
            "h.created_at DESC",
        ),
        case(
            "one source system",
            json!({"source_system": "listsrc", "status": "deferred"}),
            "ds.source_system = 'listsrc'",
            "h.status = 'deferred'",
            "h.created_at DESC",
        ),
        case(
            "under a ceiling",
            json!({"max_relative_delta": 0.01}),
            &format!("{relative} <= 0.01"),
            &pending,
            "h.created_at DESC",
        ),
        case(
            "under both statistic ceilings",
            json!({"max_mean_relative_delta": 0.05, "max_sd_relative_delta": 0.2}),
            &format!(
                "{mean} <= 0.05 AND {sd} <= 0.2",
                mean = relative_delta_of("mean"),
                sd = relative_delta_of("sd")
            ),
            &pending,
            "h.created_at DESC",
        ),
        case(
            "the n-divisor signature",
            json!({"classification": "source_sd_matches_n_divisor"}),
            &format!(
                "h.kind = 'replicate_stats' AND ({})",
                *river_db::routes::private::sync::service::SOURCE_SD_MATCHES_N_DIVISOR_SQL
            ),
            &pending,
            "h.created_at DESC",
        ),
        case(
            "what it does not explain",
            json!({"classification": "not_source_sd_matches_n_divisor"}),
            &format!(
                "h.kind = 'replicate_stats' AND NOT COALESCE(({}), false)",
                *river_db::routes::private::sync::service::SOURCE_SD_MATCHES_N_DIVISOR_SQL
            ),
            &pending,
            "h.created_at DESC",
        ),
        case(
            "every status",
            json!({"status": "any"}),
            "TRUE",
            "TRUE",
            "h.created_at DESC",
        ),
        case(
            "one site, through the pairing or the finding",
            json!({"site_id": crate::common::SITE2_ID, "status": "any"}),
            &format!(
                "(h.site_id = '{s}' OR EXISTS (SELECT 1 FROM site_parameters sp \
                   WHERE sp.id = ds.site_parameter_id AND sp.site_id = '{s}'))",
                s = crate::common::SITE2_ID
            ),
            "TRUE",
            "h.created_at DESC",
        ),
        case(
            "one parameter",
            json!({"parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID, "status": "any"}),
            &format!(
                "(h.parameter_id = '{p}' OR EXISTS (SELECT 1 FROM site_parameters sp \
                   WHERE sp.id = ds.site_parameter_id AND sp.parameter_id = '{p}'))",
                p = crate::common::GLOBAL_PARAM_TEMP_ID
            ),
            "TRUE",
            "h.created_at DESC",
        ),
        case(
            "a period",
            json!({"from": "2025-06-01T02:00:00Z", "to": "2025-06-01T05:00:00Z", "status": "any"}),
            "h.group_time >= '2025-06-01T02:00:00Z' AND h.group_time < '2025-06-01T05:00:00Z'",
            "TRUE",
            "h.created_at DESC",
        ),
        Case {
            label: "a caller confined to one project",
            scope: AccessScope::Projects(std::sync::Arc::new(
                [Uuid::parse_str(crate::common::PROJECT_ID).unwrap()]
                    .into_iter()
                    .collect(),
            )),
            query: query(json!({})),
            base: format!(
                "(EXISTS (SELECT 1 FROM site_parameters sp JOIN sites st ON st.id = sp.site_id \
                   WHERE sp.id = ds.site_parameter_id AND st.project_id = ANY(ARRAY['{p}']::uuid[])) \
                 OR EXISTS (SELECT 1 FROM sites st WHERE st.id = h.site_id \
                   AND st.project_id = ANY(ARRAY['{p}']::uuid[])))",
                p = crate::common::PROJECT_ID
            ),
            status: pending.clone(),
            order_by: "h.created_at DESC".to_string(),
        },
        Case {
            label: "a caller confined to no project at all",
            scope: AccessScope::Projects(std::sync::Arc::new(std::collections::HashSet::new())),
            query: query(json!({})),
            base: "FALSE".to_string(),
            status: pending,
            order_by: "h.created_at DESC".to_string(),
        },
    ]
}

async fn text_rows(db: &DatabaseConnection, sql: &str) -> Vec<serde_json::Value> {
    HoldRow::find_by_statement(Statement::from_string(DatabaseBackend::Postgres, sql))
        .all(db)
        .await
        .unwrap_or_else(|e| panic!("text list: {e}"))
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect()
}

async fn built_rows(db: &DatabaseConnection, statement: Statement) -> Vec<serde_json::Value> {
    HoldRow::find_by_statement(statement)
        .all(db)
        .await
        .unwrap_or_else(|e| panic!("built list: {e}"))
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect()
}

/// Every filter, view and sort the queue takes, answered the same by the built statements and by
/// the text they replaced: the same page in the same order, the same three counts, the same split
/// by kind.
#[tokio::test]
#[serial]
async fn the_built_statements_answer_what_the_text_answered() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed(&db).await;

    for case in cases() {
        let filters = svc::hold_filters(&case.scope, &case.query).expect(case.label);
        let status = svc::hold_status_condition(case.query.status.as_deref()).expect(case.label);
        let order_by = svc::hold_order_by(case.query.sort.as_deref()).expect(case.label);

        let built = built_rows(
            &db,
            svc::built(svc::hold_list_statement(
                &filters, &status, &order_by, 50, 0,
            )),
        )
        .await;
        let text = text_rows(
            &db,
            &text_list(&case.base, &case.status, &case.order_by, 50, 0),
        )
        .await;
        assert_eq!(built, text, "{}: the page differs", case.label);
        assert!(
            !built.is_empty() || case.label.contains("no project"),
            "{}: the case selects nothing, so it proves nothing",
            case.label
        );

        let built = Counts::from_query_result(
            &db.query_one_raw(svc::built(svc::hold_counts_statement(&filters, &status)))
                .await
                .unwrap()
                .expect("built counts"),
            "",
        )
        .unwrap();
        let text = Counts::from_query_result(
            &db.query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                text_counts(&case.base, &case.status),
            ))
            .await
            .unwrap()
            .expect("text counts"),
            "",
        )
        .unwrap();
        assert_eq!(
            (built.total, built.pending, built.deferred),
            (text.total, text.pending, text.deferred),
            "{}: the counts differ",
            case.label
        );

        let by_kind = |rows: Vec<sea_orm::QueryResult>| {
            let mut counts: Vec<(String, i64)> = rows
                .iter()
                .map(|r| {
                    let r = KindCount::from_query_result(r, "").unwrap();
                    (r.kind, r.n)
                })
                .collect();
            counts.sort();
            counts
        };
        let built = by_kind(
            db.query_all_raw(svc::built(svc::hold_kind_counts_statement(&filters)))
                .await
                .unwrap(),
        );
        let text = by_kind(
            db.query_all_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                text_kind_counts(&case.base),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(built, text, "{}: the per-kind counts differ", case.label);
    }

    // The page window is the statement's, not the caller's slicing of a full read.
    let filters = svc::hold_filters(&AccessScope::Unrestricted, &query(json!({}))).unwrap();
    let status = svc::hold_status_condition(None).unwrap();
    let order_by = svc::hold_order_by(None).unwrap();
    let first = built_rows(
        &db,
        svc::built(svc::hold_list_statement(&filters, &status, &order_by, 1, 0)),
    )
    .await;
    let second = built_rows(
        &db,
        svc::built(svc::hold_list_statement(&filters, &status, &order_by, 1, 1)),
    )
    .await;
    assert_eq!(first.len(), 1, "a page of one holds one row");
    assert_eq!(second.len(), 1, "the second page holds the next row");
    assert_ne!(first, second, "the two pages hold different rows");

    crate::common::cleanup_test_db(&db).await;
}

/// A filter, a view or a sort the route does not offer is a 400 naming what was asked for.
#[tokio::test]
#[serial]
async fn unknown_filters_are_refused() {
    let unknown = svc::hold_filters(
        &AccessScope::Unrestricted,
        &query(json!({"classification": "stale_subset"})),
    );
    assert!(
        format!("{:?}", unknown.err()).contains("stale_subset"),
        "an unfilterable signature names itself"
    );
    assert!(
        format!("{:?}", svc::hold_status_condition(Some("archived")).err()).contains("archived"),
        "an unknown status names itself"
    );
    assert!(
        format!("{:?}", svc::hold_order_by(Some("created_at_asc")).err())
            .contains("created_at_asc"),
        "an unknown sort names itself"
    );
    assert!(
        format!(
            "{:?}",
            svc::hold_filters(
                &AccessScope::Unrestricted,
                &query(json!({"stream_ids": "not-a-uuid"}))
            )
            .err()
        )
        .contains("not-a-uuid"),
        "an unparseable stream id names itself"
    );
}
