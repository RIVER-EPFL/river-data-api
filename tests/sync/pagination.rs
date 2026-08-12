//! Pagination on the sync command and event listings.
//!
//! The page contract lives in the `Content-Range` header rather than the payload. Both endpoints
//! report an inclusive end, per RFC 7233, and both expose the header for browsers.
//!
//! Run: cargo test --test sync -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const PAGE_SIZE: usize = 30;

fn content_range(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("Content-Range")
        .expect("Content-Range header")
        .to_str()
        .unwrap()
        .to_string()
}

async fn seed_commands_and_events(db: &DatabaseConnection, service_id: Uuid) {
    for i in 0..PAGE_SIZE {
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
                 VALUES ('{}', '{service_id}', 'scheduled', 'completed', {i}, 0, \
                         now() - interval '{i} minutes')",
                Uuid::new_v4()
            ),
        )
        .await;
    }
}

#[tokio::test]
#[serial]
async fn command_and_event_listings_page_and_report_their_range() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_raw, service_id) = crate::common::seed_sync_session_token(&db).await;
    seed_commands_and_events(&db, service_id).await;
    let token = crate::common::seed_token_read_metadata_only(&db).await;
    let app = crate::common::build_test_app(db.clone());

    for path in ["/api/sync/commands", "/api/sync/events"] {
        let (status, _headers, body) =
            crate::common::get_with_token_headers(&app, path, &token).await;
        assert_eq!(status, 200, "{path} ({status}): {body}");
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(items.len(), 25, "{path} default page size");
    }

    let (_s, headers, body) =
        crate::common::get_with_token_headers(&app, "/api/sync/commands?page=2&per_page=10", &token)
            .await;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(items.len(), 10);
    assert_eq!(content_range(&headers), "items 10-19/30", "commands: inclusive end");
    assert!(headers.get("Access-Control-Expose-Headers").is_some());

    let (_s, headers, body) =
        crate::common::get_with_token_headers(&app, "/api/sync/events?page=2&per_page=10", &token)
            .await;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(items.len(), 10);
    assert_eq!(content_range(&headers), "items 10-19/30", "events: inclusive end");
    assert!(headers.get("Access-Control-Expose-Headers").is_some());
}

#[tokio::test]
#[serial]
async fn pagination_clamps_page_size_and_handles_out_of_range_pages() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (_raw, service_id) = crate::common::seed_sync_session_token(&db).await;
    seed_commands_and_events(&db, service_id).await;
    let token = crate::common::seed_token_read_metadata_only(&db).await;
    let app = crate::common::build_test_app(db.clone());

    for path in ["/api/sync/commands", "/api/sync/events"] {
        let (_s, _h, body) =
            crate::common::get_with_token_headers(&app, &format!("{path}?per_page=500"), &token)
                .await;
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(items.len(), PAGE_SIZE, "{path}: per_page clamps to 100");

        let (_s, _h, first) =
            crate::common::get_with_token_headers(&app, &format!("{path}?page=1&per_page=5"), &token)
                .await;
        let (_s, _h, zeroth) =
            crate::common::get_with_token_headers(&app, &format!("{path}?page=0&per_page=5"), &token)
                .await;
        assert_eq!(first, zeroth, "{path}: page=0 behaves as page 1");

        let (status, headers, body) =
            crate::common::get_with_token_headers(&app, &format!("{path}?page=99"), &token).await;
        assert_eq!(status, 200, "{path} beyond the last page: {body}");
        let items: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert!(items.is_empty());
        assert_eq!(content_range(&headers), "items */30", "{path}: empty page range");
    }
}
