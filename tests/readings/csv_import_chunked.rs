//! A file too large for one request arrives as appends against one staging session, and the import
//! then names the session. The portals' `10min_data.csv` is 474 MB against a 50 MB body limit.
//!
//! The session is rows of `csv_import_chunks`, not process memory, so an upload survives a restart
//! and a chunk that reaches another replica appends to the same file.
//!
//! Run with: cargo test --test readings csv_import_chunked

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn chunk(
    app: &axum::Router,
    token: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(app, "/api/readings/import_csv/chunk", body, token)
        .await
}

async fn poll_count(db: &DatabaseConnection, want: i64) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) AS n FROM readings WHERE site_id = '{site}' \
         AND parameter_id = '{param}' AND time >= '2025-08-01T00:00:00Z' \
         AND time < '2025-08-02T00:00:00Z'",
        site = crate::common::SITE1_ID,
        param = crate::common::GLOBAL_PARAM_DO_ID
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let n = db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                sql.clone(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get::<i64>("", "n")
            .unwrap();
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

#[tokio::test]
#[serial]
async fn a_file_uploaded_in_slices_imports_as_one_file() {
    let (db, app, token) = setup().await;

    // The slices are cut mid-row: the session holds text, so the append rejoins them.
    let (status, opened) = chunk(
        &app,
        &token,
        &serde_json::json!({ "chunk": "DateTime,Dissolved_O2\n2025-08-01 00:00:00,25" }),
    )
    .await;
    assert_eq!(status, 200, "first chunk ({status}): {opened}");
    let session_id = opened["session_id"]
        .as_str()
        .expect("a session id")
        .to_string();

    let (status, appended) = chunk(
        &app,
        &token,
        &serde_json::json!({
            "session_id": session_id,
            "chunk": "0\n2025-08-01 00:10:00,260\n"
        }),
    )
    .await;
    assert_eq!(status, 200, "second chunk ({status}): {appended}");
    assert_eq!(
        appended["session_id"].as_str().unwrap(),
        session_id,
        "the session is the one that was opened: {appended}"
    );
    assert!(
        appended["bytes"].as_u64().unwrap() > opened["bytes"].as_u64().unwrap(),
        "the session grew: {appended}"
    );

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "session_id": session_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(
        resp["row_count"].as_u64().unwrap(),
        2,
        "both slices are in the file that was imported: {resp}"
    );
    assert_eq!(resp["inserted_total"].as_u64().unwrap(), 2);
    assert_eq!(poll_count(&db, 2).await, 2, "both readings land");
}

#[tokio::test]
#[serial]
async fn a_chunk_naming_no_open_session_is_refused() {
    let (_db, app, token) = setup().await;

    let (status, body) = chunk(
        &app,
        &token,
        &serde_json::json!({
            "session_id": uuid::Uuid::new_v4(),
            "chunk": "DateTime,Dissolved_O2\n2025-08-03 00:00:00,250\n"
        }),
    )
    .await;
    assert_eq!(status, 400, "({status}): {body}");
    assert!(
        body.to_string().contains("session"),
        "the refusal says the session is gone: {body}"
    );
}

/// Scenario: the process that took the first chunks is gone, or the next chunk reaches a second
/// replica.
///
/// Expected behaviour: the session is where the database holds it, so a router that never saw the
/// first chunk appends to the same file and imports it whole.
#[tokio::test]
#[serial]
async fn a_session_is_readable_by_a_router_that_never_saw_its_first_chunk() {
    let (db, app, token) = setup().await;

    let (status, opened) = chunk(
        &app,
        &token,
        &serde_json::json!({ "chunk": "DateTime,Dissolved_O2\n2025-08-01 01:00:00,25" }),
    )
    .await;
    assert_eq!(status, 200, "first chunk ({status}): {opened}");
    let session_id = opened["session_id"]
        .as_str()
        .expect("a session id")
        .to_string();

    // A second router over the same database is what a second replica is.
    let other = crate::common::build_test_app(db.clone());
    let (status, appended) = chunk(
        &other,
        &token,
        &serde_json::json!({
            "session_id": session_id,
            "chunk": "0\n2025-08-01 01:10:00,260\n"
        }),
    )
    .await;
    assert_eq!(
        status, 200,
        "the other router knows the session ({status}): {appended}"
    );

    let (status, resp) = crate::common::post_json_parse_with_token(
        &other,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "session_id": session_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "import ({status}): {resp}");
    assert_eq!(
        resp["row_count"].as_u64().unwrap(),
        2,
        "the file is both slices: {resp}"
    );
}

/// An upload that stops part-way leaves rows nothing will read; the janitor's prune is what
/// removes them, and it leaves a session still inside its retention alone.
#[tokio::test]
#[serial]
async fn the_prune_removes_an_abandoned_upload_and_keeps_a_live_one() {
    let (db, app, token) = setup().await;

    let (_, live) = chunk(
        &app,
        &token,
        &serde_json::json!({ "chunk": "DateTime,Dissolved_O2\n2025-08-05 00:00:00,250\n" }),
    )
    .await;
    let (_, abandoned) = chunk(
        &app,
        &token,
        &serde_json::json!({ "chunk": "DateTime,Dissolved_O2\n2025-08-06 00:00:00,250\n" }),
    )
    .await;
    let abandoned_id = abandoned["session_id"].as_str().unwrap().to_string();
    crate::common::exec(
        &db,
        &format!(
            "UPDATE csv_import_chunks SET created_at = now() - interval '2 days' \
             WHERE session_id = '{abandoned_id}'"
        ),
    )
    .await;

    let removed = river_db::routes::private::readings::service::prune_import_sessions(&db)
        .await
        .expect("the prune runs");
    assert_eq!(removed, 1, "only the abandoned upload is removed");

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "session_id": abandoned_id }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "the pruned session is gone ({status}): {body}");

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({
            "site": crate::common::SITE1_ID,
            "session_id": live["session_id"].as_str().unwrap(),
            "dry_run": true,
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "the live session is untouched ({status}): {resp}"
    );
}

/// Scenario: a second caller with `write_data` learns the id of an upload someone else opened.
///
/// Expected behaviour: the session is the opener's alone, so neither an append nor an import
/// naming it reaches the file, and the opener's own upload still imports whole.
#[tokio::test]
#[serial]
async fn a_session_is_refused_to_anyone_but_its_opener() {
    let (db, app, token) = setup().await;
    let stranger = crate::common::seed::seed_token_full(&db).await;

    let (status, opened) = chunk(
        &app,
        &token,
        &serde_json::json!({ "chunk": "DateTime,Dissolved_O2\n2025-08-07 00:00:00,250\n" }),
    )
    .await;
    assert_eq!(status, 200, "first chunk ({status}): {opened}");
    let session_id = opened["session_id"].as_str().unwrap().to_string();

    let (status, body) = chunk(
        &app,
        &stranger,
        &serde_json::json!({
            "session_id": session_id,
            "chunk": "2025-08-07 00:10:00,999\n"
        }),
    )
    .await;
    assert_eq!(
        status, 400,
        "a stranger's append is refused ({status}): {body}"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "session_id": session_id, "dry_run": true }),
        &stranger,
    )
    .await;
    assert_eq!(
        status, 400,
        "a stranger's import is refused ({status}): {body}"
    );

    let (status, resp) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/import_csv",
        &serde_json::json!({ "site": crate::common::SITE1_ID, "session_id": session_id, "dry_run": true }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the opener imports ({status}): {resp}");
    assert_eq!(
        resp["row_count"].as_u64().unwrap(),
        1,
        "the stranger's row never joined the file: {resp}"
    );
}
