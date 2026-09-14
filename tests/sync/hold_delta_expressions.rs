//! The delta and signature expressions the review queue triages on, against the SQL that shipped.
//!
//! The list filters, the sort and the threshold bulk acknowledge all read one producer, so the
//! only thing that has to hold through a rewrite is that the new spelling answers what the old one
//! answered, for every hold shape the queue can carry. The texts below are the implementation as
//! it stood before the conversion; the database evaluates both over the same rows.
//!
//! Run: cargo test --test sync hold_delta_expressions -- --test-threads=1

use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::sync::service as svc;

/// One built expression as the SQL the database is asked to evaluate. Rendering lives here, in the
/// test, so the component itself never stringifies a statement.
fn render(e: sea_orm::sea_query::Expr) -> String {
    sea_orm::sea_query::Query::select()
        .expr(e)
        .to_owned()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder)
        .trim_start_matches("SELECT ")
        .to_string()
}

const SCALE: &str = "GREATEST(\
     abs(COALESCE((h.expected->>'mean')::float8, 0)), \
     abs(COALESCE((h.computed->>'mean')::float8, 0)), \
     1e-9)";

fn mean_relative_delta() -> String {
    format!("COALESCE(abs((h.delta->>'mean')::float8), 0) / {SCALE}")
}

fn sd_relative_delta() -> String {
    format!("COALESCE(abs((h.delta->>'sd')::float8), 0) / {SCALE}")
}

fn relative_delta() -> String {
    format!(
        "GREATEST(COALESCE(abs((h.delta->>'mean')::float8), 0), \
          COALESCE(abs((h.delta->>'sd')::float8), 0)) / {SCALE}"
    )
}

/// The population-divisor signature as `POPULATION_SD_SQL` spelled it, tolerances inlined.
fn population_sd() -> String {
    "((h.expected->>'n') IS NULL \
       OR (h.expected->>'n')::int = (h.computed->>'n')::int) \
     AND (h.computed->>'n')::int >= 2 \
     AND (h.expected->>'mean') IS NOT NULL AND (h.computed->>'mean') IS NOT NULL \
     AND abs((h.expected->>'mean')::float8 - (h.computed->>'mean')::float8) \
         <= GREATEST(0.00001 * GREATEST(abs((h.expected->>'mean')::float8), \
                                        abs((h.computed->>'mean')::float8)), \
                     0.0001, 0.0050000010000000004) \
     AND (h.expected->>'sd') IS NOT NULL AND (h.computed->>'sd') IS NOT NULL \
     AND abs((h.expected->>'sd')::float8 \
             - ((h.computed->>'sd')::float8 \
                * sqrt(((h.computed->>'n')::float8 - 1) / (h.computed->>'n')::float8))) \
         <= GREATEST(0.001 * GREATEST(abs((h.expected->>'sd')::float8), \
                                      abs((h.computed->>'sd')::float8 \
                                          * sqrt(((h.computed->>'n')::float8 - 1) \
                                                 / (h.computed->>'n')::float8))), \
                     0.001, 0.0050000010000000004)"
        .to_string()
}

/// Hold shapes the queue carries: a plain disagreement, the population signature, a count
/// mismatch, one missing a statistic, one whose mean is zero (the scale floor), a finding no
/// stream produced, and one whose computed document carries no `n` at all, which the population
/// signature divides by (I95).
const SHAPES: &[(&str, &str, &str)] = &[
    (
        r#"{"n": 3, "mean": 150.0, "sd": 2.0}"#,
        r#"{"n": 3, "mean": 150.4, "sd": 2.5}"#,
        r#"{"mean": -0.4, "sd": -0.5}"#,
    ),
    (
        r#"{"n": 4, "mean": 10.0, "sd": 1.7320508075688772}"#,
        r#"{"n": 4, "mean": 10.0, "sd": 2.0}"#,
        r#"{"sd": -0.2679491924311228}"#,
    ),
    (
        r#"{"n": 2, "mean": 5.0, "sd": 1.0}"#,
        r#"{"n": 3, "mean": 5.0, "sd": 1.0}"#,
        r#"{"n": -1}"#,
    ),
    (
        r#"{"n": 3, "mean": 7.0}"#,
        r#"{"n": 3, "mean": 7.1, "sd": 0.5}"#,
        r#"{"mean": -0.1}"#,
    ),
    (
        r#"{"n": 3, "mean": 0.0, "sd": 0.0}"#,
        r#"{"n": 3, "mean": 0.0, "sd": 0.0}"#,
        r#"{}"#,
    ),
    (r#"{"n": 3}"#, r#"{"n": 3}"#, r#"{}"#),
    (
        r#"{"n": 3, "mean": 12.0, "sd": 1.5}"#,
        r#"{"mean": 12.4, "sd": 1.8}"#,
        r#"{"mean": -0.4, "sd": -0.3}"#,
    ),
];

async fn seed_shapes(db: &sea_orm::DatabaseConnection) {
    // A `replicate_stats` hold names a stream (the table's `audit_hold_subject` CHECK), so the
    // shapes hang off one registered stream; the expressions read only the three documents.
    let stream = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, created_at) \
             VALUES ('{stream}', 'test', 'delta-expressions', NOW())"
        ),
    )
    .await;
    for (expected, computed, delta) in SHAPES {
        let id = Uuid::new_v4();
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO replicate_audit_holds \
                   (id, stream_id, group_time, expected, computed, delta, status, kind, created_at) \
                 VALUES ('{id}', '{stream}', NOW(), '{expected}'::jsonb, '{computed}'::jsonb, \
                         '{delta}'::jsonb, 'pending', 'replicate_stats', NOW())"
            ),
        )
        .await;
    }
}

/// Each built expression answers what its text answered, row for row.
#[tokio::test]
#[serial]
async fn the_built_expressions_answer_what_the_text_answered() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    seed_shapes(&db).await;

    for (label, text) in [
        ("relative_delta", relative_delta()),
        ("mean_relative_delta", mean_relative_delta()),
        ("sd_relative_delta", sd_relative_delta()),
    ] {
        let built = render(match label {
            "relative_delta" => svc::relative_delta_expr(),
            other => svc::relative_delta_of(other.trim_end_matches("_relative_delta")),
        });
        let disagreements = crate::common::e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM replicate_audit_holds h \
                 WHERE ({built}) IS DISTINCT FROM ({text})"
            ),
        )
        .await;
        assert_eq!(disagreements, 0, "{label} disagrees with its text spelling");
    }

    let built = render(sea_orm::sea_query::Expr::from(svc::population_sd_expr()));
    let text = population_sd();
    let disagreements = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM replicate_audit_holds h \
             WHERE COALESCE(({built}), false) IS DISTINCT FROM COALESCE(({text}), false)"
        ),
    )
    .await;
    assert_eq!(
        disagreements, 0,
        "the population signature disagrees with its text spelling"
    );

    crate::common::cleanup_test_db(&db).await;
}
