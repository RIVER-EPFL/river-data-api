//! Scenario: two writers store a version of one calculation at the same moment, the second
//! starting while the first's transaction is still open.
//!
//! Expected behaviour: the second waits for the first to commit, then takes the next number, so
//! the calculation ends with versions 1 and 2 rather than a unique violation on `version_no`.
//!
//! Run with: cargo test --test tools concurrent_version_saves -- --test-threads=1

use std::time::Duration;

use river_db::routes::private::tools::models::version;
use river_db::routes::private::tools::service::insert_version;
use sea_orm::{ConnectionTrait, Set, Statement, TransactionTrait};
use serial_test::serial;
use uuid::Uuid;

fn version_row(calculation: Uuid, hash: &str) -> version::ActiveModel {
    version::ActiveModel {
        tool_script_id: Set(calculation),
        script: Set("[]".to_string()),
        entry_function: Set("formula".to_string()),
        manifest: Set(serde_json::json!({})),
        content_hash: Set(hash.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
#[serial]
async fn a_second_version_waits_for_the_first_and_takes_the_next_number() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let calculation = crate::common::seed_formula_calculation(&db, "concurrent_versions").await;

    let first = db.begin().await.expect("first transaction");
    insert_version(&first, version_row(calculation, "first"))
        .await
        .expect("first version stored");

    let second_db = db.clone();
    let second = tokio::spawn(async move {
        let txn = second_db.begin().await.expect("second transaction");
        let stored = insert_version(&txn, version_row(calculation, "second")).await;
        txn.commit().await.expect("second commit");
        stored
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "the second writer waits while the first holds the calculation"
    );

    first.commit().await.expect("first commit");
    second
        .await
        .expect("second task")
        .expect("second version stored after the first commits");

    let numbers: Vec<i32> = db
        .query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT version_no FROM tool_script_versions WHERE tool_script_id = $1 \
             ORDER BY version_no",
            [calculation.into()],
        ))
        .await
        .expect("versions read")
        .iter()
        .map(|r| r.try_get("", "version_no").expect("version_no"))
        .collect();
    assert_eq!(numbers, vec![1, 2]);
}
