//! Origin on the entity lists: a row a sync minted carries `discovered_at`, one entered by hand
//! does not, and the lists filter on that column so an import can be reviewed where the rows live.

use crate::common::fixtures::{SITE1_ID, SITE2_ID};
use crate::common::{
    build_test_app, cleanup_test_db, e2e, exec, full_permissions, get_with_token, seed_api_token,
    seed_test_data, setup_test_db,
};
use serde_json::Value;
use serial_test::serial;

async fn list(app: &axum::Router, token: &str, path: &str, filter: &str) -> Vec<Value> {
    let uri = format!("/api/{path}?filter={}", e2e::percent_encode(filter));
    let (status, body) = get_with_token(app, &uri, token).await;
    assert_eq!(status, 200, "GET {uri} ({status}): {body}");
    serde_json::from_str::<Value>(&body)
        .unwrap_or_else(|e| panic!("bad JSON: {e}\n{body}"))
        .as_array()
        .unwrap_or_else(|| panic!("GET {uri} returns an array: {body}"))
        .clone()
}

fn names(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
#[serial]
async fn sites_separate_what_a_sync_brought_from_what_was_entered_by_hand() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());

    // The seed enters both sites by hand; a sync stamps the one it minted.
    exec(
        &db,
        &format!("UPDATE sites SET discovered_at = NOW() WHERE id = '{SITE1_ID}'"),
    )
    .await;

    let synced = list(&app, &token, "sites", r#"{"discovered_at_neq":null}"#).await;
    assert_eq!(synced.len(), 1, "one site carries a sync stamp: {synced:?}");
    assert_eq!(synced[0]["id"], SITE1_ID);

    let by_hand = list(&app, &token, "sites", r#"{"discovered_at":null}"#).await;
    assert_eq!(
        by_hand.iter().map(|r| r["id"].as_str()).collect::<Vec<_>>(),
        vec![Some(SITE2_ID)],
        "the rest were entered by hand: {}",
        names(&by_hand).join(", ")
    );

    let all = list(&app, &token, "sites", "{}").await;
    assert_eq!(all.len(), 2, "and neither filter loses a row");
}

#[tokio::test]
#[serial]
async fn site_parameters_separate_the_same_two_origins() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());

    let total = list(&app, &token, "site_parameters", "{}").await.len();
    assert!(total > 1, "the seed configures several slots");
    exec(
        &db,
        "UPDATE site_parameters SET discovered_at = NOW() \
         WHERE id = (SELECT id FROM site_parameters ORDER BY id LIMIT 1)",
    )
    .await;

    let synced = list(&app, &token, "site_parameters", r#"{"discovered_at_neq":null}"#).await;
    assert_eq!(synced.len(), 1, "only the stamped slot: {synced:?}");
    let by_hand = list(&app, &token, "site_parameters", r#"{"discovered_at":null}"#).await;
    assert_eq!(by_hand.len(), total - 1, "and the rest are hand-entered");
}
