//! CSV import never mints replicate indexes onto a replicate-family stream: a slot instant owned
//! by a family refuses the incoming row with a named error in the import report, and the rest of
//! the file still imports.
//!
//! Run: cargo test --test readings csv_import_family_guard -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const T_HELD: &str = "2025-06-02T00:00:00Z";
const T_FREE: &str = "2025-06-02T00:10:00Z";

const CSV: &str = "DateTime,Dissolved_O2\n\
2025-06-02 00:00:00,250\n\
2025-06-02 00:10:00,300\n";

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
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

/// A family stream paired to (SITE1, Dissolved_O2), holding a sparse replicate group at
/// [`T_HELD`].
async fn seed_family_at_do_slot(db: &DatabaseConnection) -> Uuid {
    let family = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams \
                (id, source_system, source_key, is_active, measurement_type, \
                 site_parameter_id, metadata) \
             VALUES ('{family}', 'csvfam', 'STA:DO_avg:reps', true, 'spot', '{sp}', \
                     '{{\"replicates\": {{\"source_columns\": [\"DO_1\", \"DO_2\"]}}}}')",
            sp = crate::common::PARAM_S1_DO_ID,
        ),
    )
    .await;
    for (index, value) in [(1, 240.0), (2, 260.0)] {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings \
                    (stream_id, site_id, parameter_id, time, replicate_index, raw_value, \
                     logged, measurement_type, is_flagged) \
                 VALUES ('{family}', '{site}', '{param}', '{T_HELD}', {index}, {value}, \
                         true, 'spot', false)",
                site = crate::common::SITE1_ID,
                param = crate::common::GLOBAL_PARAM_DO_ID,
            ),
        )
        .await;
    }
    family
}

#[tokio::test]
#[serial]
async fn a_family_owned_instant_refuses_the_csv_row_by_name() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let family = seed_family_at_do_slot(&db).await;

    let (status, dry) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV, "dry_run": true }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "dry run ({status}): {dry}");
    assert_eq!(
        dry["error_count"], 1,
        "the preview names the refusal: {dry}"
    );
    assert!(
        dry["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("STA:DO_avg:reps"),
        "{dry}"
    );

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": CSV }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(resp["error_count"], 1, "{resp}");
    let message = resp["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("STA:DO_avg:reps"), "{resp}");
    assert!(message.contains("Dissolved_O2"), "{resp}");
    assert_eq!(
        resp["errors"][0]["row"], 2,
        "the offending CSV line: {resp}"
    );

    let imported = poll_count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM readings \
             WHERE parameter_id = '{}' AND time = '{T_FREE}'",
            crate::common::GLOBAL_PARAM_DO_ID
        ),
        1,
        10,
    )
    .await;
    assert_eq!(imported, 1, "the unoccupied instant still imports");

    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT count(*) AS n FROM readings WHERE stream_id = '{family}'"),
        )
        .await,
        2,
        "the family group is untouched: no fabricated replicate, no overwrite"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*) AS n FROM readings \
                 WHERE parameter_id = '{}' AND time = '{T_HELD}'",
                crate::common::GLOBAL_PARAM_DO_ID
            ),
        )
        .await,
        2,
        "no second channel lands beside the family at its instant"
    );
}
