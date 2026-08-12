//! The page size the sync listings accept, at its edges.
//!
//! `Paginator::paginate` asserts a non-zero page size, so a zero that reaches it panics the
//! handler instead of answering. Both listings resolve their page through one place, so the
//! bounds are asserted on both.
//!
//! Run: cargo test --test sync -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const LISTINGS: [&str; 2] = ["/api/sync/commands", "/api/sync/events"];
const SEEDED: usize = 120;

async fn seed(db: &DatabaseConnection, service_id: Uuid) {
    for i in 0..SEEDED {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO sync_commands (id, service_id, command, status, created_at, expires_at) \
                 VALUES ('{}', '{service_id}', 'trigger_sync', 'pending', \
                         now() - interval '{i} minutes', now() + interval '1 hour')",
                Uuid::new_v4()
            ),
        )
        .await;
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO sync_events (id, service_id, event_type, status, readings_synced, \
                    status_events_synced, started_at) \
                 VALUES ('{}', '{service_id}', 'scheduled', 'completed', 0, 0, \
                         now() - interval '{i} minutes')",
                Uuid::new_v4()
            ),
        )
        .await;
    }
}

/// Run the request on its own task so a panic inside the handler surfaces as an error rather than
/// unwinding the test.
async fn get(app: &axum::Router, uri: &str, token: &str) -> Result<(u16, String), String> {
    let app = app.clone();
    let uri = uri.to_string();
    let token = token.to_string();
    tokio::spawn(async move { crate::common::get_with_token(&app, &uri, &token).await })
        .await
        .map_err(|e| e.to_string())
}

async fn items(app: &axum::Router, uri: &str, token: &str) -> Vec<serde_json::Value> {
    let (status, body) = get(app, uri, token)
        .await
        .unwrap_or_else(|e| panic!("{uri}: {e}"));
    assert_eq!(status, 200, "{uri} ({status}): {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{uri} is not a JSON array: {e}: {body}"))
}

#[tokio::test]
#[serial]
async fn a_zero_page_size_is_refused_and_the_bounds_around_it_hold() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_raw, service_id) = crate::common::seed_sync_session_token(&db).await;
    seed(&db, service_id).await;
    let token = crate::common::seed_token_read_metadata_only(&db).await;
    let app = crate::common::build_test_app(db.clone());

    for path in LISTINGS {
        let outcome = get(&app, &format!("{path}?per_page=0"), &token).await;
        assert!(
            outcome.is_ok(),
            "{path} with per_page=0 must answer the caller, not panic: {outcome:?}"
        );
        let (status, body) = outcome.unwrap();
        assert_eq!(
            status, 400,
            "{path} refuses a page size of zero ({status}): {body}"
        );

        assert_eq!(
            items(&app, &format!("{path}?per_page=1"), &token)
                .await
                .len(),
            1,
            "{path}: one is the smallest page"
        );
        assert_eq!(
            items(&app, &format!("{path}?per_page=100"), &token)
                .await
                .len(),
            100,
            "{path}: the largest page is served whole"
        );
        assert_eq!(
            items(&app, &format!("{path}?per_page=101"), &token)
                .await
                .len(),
            100,
            "{path}: one over the maximum clamps rather than refusing"
        );
        assert_eq!(
            items(&app, &format!("{path}?per_page=100000"), &token)
                .await
                .len(),
            100,
            "{path}: a wildly oversized page clamps too"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_page_size_of_one_walks_every_row_without_repeating() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_raw, service_id) = crate::common::seed_sync_session_token(&db).await;
    let token = crate::common::seed_token_read_metadata_only(&db).await;
    let app = crate::common::build_test_app(db.clone());

    for i in 0..3 {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO sync_commands (id, service_id, command, status, created_at, expires_at) \
                 VALUES ('{}', '{service_id}', 'trigger_sync', 'pending', \
                         now() - interval '{i} minutes', now() + interval '1 hour')",
                Uuid::new_v4()
            ),
        )
        .await;
    }

    let mut seen = std::collections::HashSet::new();
    for page in 1..=3 {
        let rows = items(
            &app,
            &format!("/api/sync/commands?page={page}&per_page=1"),
            &token,
        )
        .await;
        assert_eq!(rows.len(), 1, "page {page} carries one row");
        assert!(
            seen.insert(rows[0]["id"].as_str().expect("row id").to_string()),
            "page {page} repeats a row already returned"
        );
    }
}
