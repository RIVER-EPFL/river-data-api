//! A wide CSV covering several sites: the portals' high-frequency exports are one file per
//! resolution with a `Site_ID` column, and each row belongs to the site that column names.
//!
//! Run with: cargo test --test readings csv_import_multi_site

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// Two known sites by name, one by id, and one name no site answers to.
fn csv() -> String {
    format!(
        "DateTime,Site_ID,Dissolved_O2\n\
         2025-07-01 00:00:00,Upstream Station,250\n\
         2025-07-01 00:00:00,Downstream Station,260\n\
         2025-07-01 00:10:00,{site2},270\n\
         2025-07-01 00:20:00,Nowhere,280\n",
        site2 = crate::common::SITE2_ID
    )
}

async fn import(
    app: &axum::Router,
    token: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/readings/import_csv", body, token).await
}

async fn count_at_site(db: &DatabaseConnection, site_id: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT COUNT(*) AS n FROM readings WHERE site_id = '{site_id}' \
             AND parameter_id = '{param}' AND time >= '2025-07-01T00:00:00Z' \
             AND time < '2025-07-02T00:00:00Z'",
            param = crate::common::GLOBAL_PARAM_DO_ID
        ),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn poll_count(db: &DatabaseConnection, site_id: &str, want: i64) -> i64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let n = count_at_site(db, site_id).await;
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[tokio::test]
#[serial]
async fn every_row_lands_on_the_site_its_own_cell_names() {
    let (db, app, token) = setup().await;

    let (status, plan) = import(
        &app,
        &token,
        &serde_json::json!({
            "site": crate::common::SITE1_ID, "csv": csv(), "dry_run": true
        }),
    )
    .await;
    assert_eq!(status, 200, "dry_run ({status}): {plan}");
    assert_eq!(
        plan["row_count"].as_u64().unwrap(),
        3,
        "three rows name a site that exists: {plan}"
    );
    let unmapped: Vec<&str> = plan["unmapped_columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        !unmapped.contains(&"Site_ID"),
        "the site column is the file's own bookkeeping, not an unmapped parameter: {plan}"
    );
    let shares = plan["site_imports"].as_array().unwrap();
    assert_eq!(shares.len(), 2, "one share per site named: {plan}");
    assert_eq!(shares[0]["site_id"], crate::common::SITE1_ID);
    assert_eq!(shares[0]["row_count"], 1);
    assert_eq!(shares[1]["site_id"], crate::common::SITE2_ID);
    assert_eq!(shares[1]["row_count"], 2);

    let errors = plan["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "the unknown site is reported: {plan}");
    assert_eq!(
        errors[0]["row"].as_u64().unwrap(),
        5,
        "against the line of the file that was uploaded: {plan}"
    );
    assert!(
        errors[0]["message"].as_str().unwrap().contains("Nowhere"),
        "{plan}"
    );

    let (status, resp) = import(
        &app,
        &token,
        &serde_json::json!({ "site": crate::common::SITE1_ID, "csv": csv() }),
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(resp["inserted_total"].as_u64().unwrap(), 3);

    assert_eq!(
        poll_count(&db, crate::common::SITE1_ID, 1).await,
        1,
        "the upstream row is upstream's"
    );
    assert_eq!(
        poll_count(&db, crate::common::SITE2_ID, 2).await,
        2,
        "and the other two are downstream's"
    );
}

#[tokio::test]
#[serial]
async fn a_declared_site_column_the_file_lacks_is_refused() {
    let (_db, app, token) = setup().await;

    let (status, body) = import(
        &app,
        &token,
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "csv": "DateTime,Dissolved_O2\n2025-07-02 00:00:00,250\n",
            "site_column": "station",
            "dry_run": true
        }),
    )
    .await;
    assert_eq!(status, 400, "({status}): {body}");
    assert!(
        body.to_string().contains("station"),
        "the refusal names the column asked for: {body}"
    );
}
