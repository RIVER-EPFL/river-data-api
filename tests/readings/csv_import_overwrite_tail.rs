//! A CSV import in `overwrite` mode displaces the stored replicates beyond the incoming column
//! count. Displacement is a `withdrawn_at` stamp, never a delete: the row, its flag and its
//! hand-picked standard curve survive, and a displaced curated row raises a `source_modified`
//! hold so a person decides what the group holds.
//!
//! Run: cargo test --test readings csv_import_overwrite_tail -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T: &str = "2025-07-04T09:00:00Z";
const T_CSV: &str = "2025-07-04 09:00:00";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let f = crate::common::seeded_app().await;
    Fixture {
        db: f.db,
        app: f.app,
        token: f.token,
    }
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn poll_count(db: &DatabaseConnection, sql: &str, want: i64, max_secs: u64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_secs);
    loop {
        let n = scalar_i64(db, sql).await;
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

fn csv(values: &[f64]) -> String {
    let mut out = String::from("DateTime,Dissolved_O2\n");
    for v in values {
        out.push_str(&format!("{T_CSV},{v}\n"));
    }
    out
}

async fn import(fx: &Fixture, values: &[f64], overwrite: bool) -> (u16, serde_json::Value) {
    let mut body = json!({
        "site": crate::common::SITE1_ID,
        "csv": csv(values),
        "measurement_type": "spot",
    });
    if overwrite {
        body["conflict"] = json!("overwrite");
    }
    crate::common::post_screened_import(&fx.app, &body, &fx.token).await
}

async fn spot_rows(fx: &Fixture) -> Vec<(i16, f64, bool, bool, bool)> {
    let rows = fx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index, raw_value, is_flagged IS TRUE AS flagged, \
                        standard_curve_id IS NOT NULL AS curved, \
                        withdrawn_at IS NOT NULL AS withdrawn \
                 FROM readings \
                 WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{T}' \
                 ORDER BY replicate_index",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_DO_ID
            ),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.try_get::<i16>("", "replicate_index").unwrap(),
                r.try_get::<f64>("", "raw_value").unwrap(),
                r.try_get::<bool>("", "flagged").unwrap(),
                r.try_get::<bool>("", "curved").unwrap(),
                r.try_get::<bool>("", "withdrawn").unwrap(),
            )
        })
        .collect()
}

async fn holds(fx: &Fixture) -> Vec<(String, String)> {
    let rows = fx
        .db
        .query_all_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT kind, status FROM replicate_audit_holds WHERE group_time = '{T}'"),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.try_get::<String>("", "kind").unwrap(),
                r.try_get::<String>("", "status").unwrap(),
            )
        })
        .collect()
}

async fn seed_five(fx: &Fixture) {
    let (status, body) = import(fx, &[10.0, 11.0, 12.0, 13.0, 14.0], false).await;
    assert_eq!(status, 200, "seed import ({status}): {body}");
    let count = poll_count(
        &fx.db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{T}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
        5,
        15,
    )
    .await;
    assert_eq!(count, 5, "five replicates seeded");
}

/// Scenario: an operator has flagged one replicate and hand-picked a standard curve on another,
/// then a three-column file of the same instant is imported with `conflict: "overwrite"`.
/// Expected behaviour: neither curated row is destroyed, both keep what the operator put on them,
/// both leave the served group by a reversible stamp, and the disagreement reaches the queue.
#[tokio::test]
#[serial]
async fn overwrite_withdraws_the_displaced_tail_and_holds_curated_rows() {
    let fx = setup().await;
    seed_five(&fx).await;

    let sensor_id = "00000000-0000-4000-c000-0000000000e1";
    let curve_id = "00000000-0000-4000-c000-0000000000e2";
    crate::common::exec(
        &fx.db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, created_at) \
             VALUES ('{sensor_id}', 'Bench reader', true, true, now())"
        ),
    )
    .await;
    crate::common::exec(
        &fx.db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve_id}', '{sensor_id}', 2.0, 1.0, 'Plate A')"
        ),
    )
    .await;
    crate::common::exec(
        &fx.db,
        &format!(
            "UPDATE readings SET standard_curve_id = '{curve_id}' \
             WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{T}' AND replicate_index = 3",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;
    crate::common::exec(
        &fx.db,
        &format!(
            "UPDATE readings SET is_flagged = true \
             WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{T}' AND replicate_index = 4",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
    )
    .await;

    let (status, body) = import(&fx, &[20.0, 21.0, 22.0], true).await;
    assert_eq!(status, 200, "overwrite import ({status}): {body}");

    let withdrawn = poll_count(
        &fx.db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{T}' \
               AND withdrawn_at IS NOT NULL",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
        2,
        15,
    )
    .await;
    assert_eq!(
        withdrawn, 2,
        "the two displaced rows are stamped, not deleted"
    );

    let rows = spot_rows(&fx).await;
    assert_eq!(rows.len(), 5, "no row is removed: {rows:?}");
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4],
        "every replicate index survives: {rows:?}"
    );
    assert!(
        rows[..3].iter().all(|r| !r.4),
        "the overwritten head stays served: {rows:?}"
    );
    assert!(
        (rows[0].1 - 20.0).abs() < 1e-9 && (rows[2].1 - 22.0).abs() < 1e-9,
        "the head carries the new values: {rows:?}"
    );
    assert!(
        rows[3].3,
        "the hand-picked curve survives on index 3: {rows:?}"
    );
    assert!(rows[3].4, "index 3 is withdrawn from the group: {rows:?}");
    assert!(rows[4].2, "the flag survives on index 4: {rows:?}");
    assert!(rows[4].4, "index 4 is withdrawn from the group: {rows:?}");

    let raised = holds(&fx).await;
    assert_eq!(
        raised.len(),
        1,
        "one source_modified hold for the displaced curated rows: {raised:?}"
    );
    assert_eq!(raised[0].0, "source_modified", "{raised:?}");
    assert_eq!(raised[0].1, "pending", "{raised:?}");
}

/// Expected behaviour: displacing rows nobody has curated needs no ruling, so it stamps them and
/// raises nothing.
#[tokio::test]
#[serial]
async fn overwrite_of_an_uncurated_tail_raises_no_hold() {
    let fx = setup().await;
    seed_five(&fx).await;

    let (status, body) = import(&fx, &[20.0, 21.0, 22.0, 23.0], true).await;
    assert_eq!(status, 200, "overwrite import ({status}): {body}");

    let withdrawn = poll_count(
        &fx.db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE site_id = '{}' AND parameter_id = '{}' AND time = '{T}' \
               AND withdrawn_at IS NOT NULL",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_DO_ID
        ),
        1,
        15,
    )
    .await;
    assert_eq!(withdrawn, 1, "only the one displaced row is stamped");
    assert_eq!(spot_rows(&fx).await.len(), 5, "nothing is deleted");
    assert!(holds(&fx).await.is_empty(), "no ruling is needed");
}
