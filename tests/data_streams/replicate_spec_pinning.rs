//! The registered replicate column-to-index mapping is authoritative and append-only: a column
//! keeps its index for the life of the stream, a reorder upstream is a no-op, a new column
//! appends, a removed column's index retires and is never reused, and a re-registration that
//! cannot be resolved (a rename indistinguishable from remove-plus-add) is refused with the
//! columns named.
//!
//! Run: cargo test --test data_streams replicate_spec_pinning -- --test-threads=1

use sea_orm::DatabaseConnection;
use serde_json::json;
use serial_test::serial;

const SOURCE: &str = "pinsrc";
const KEY: &str = "STA:DOC_avg:reps";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn register(app: &axum::Router, token: &str, columns: &[&str]) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": SOURCE,
            "source_key": KEY,
            "measurement_type": "spot",
            "replicates": { "source_columns": columns },
        }),
        token,
    )
    .await
}

/// The response mapping as (column, index, retired) triples, in response order.
fn mapping(body: &serde_json::Value) -> Vec<(String, i64, bool)> {
    body["replicates"]
        .as_array()
        .unwrap_or_else(|| panic!("response carries the mapping: {body}"))
        .iter()
        .map(|a| {
            (
                a["column"].as_str().unwrap().to_string(),
                a["index"].as_i64().unwrap(),
                a["retired"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn entry(column: &str, index: i64, retired: bool) -> (String, i64, bool) {
    (column.to_string(), index, retired)
}

#[tokio::test]
#[serial]
async fn known_columns_keep_their_index_and_a_reorder_is_a_no_op() {
    let (_db, app, token) = setup().await;

    let (status, body) = register(&app, &token, &["A", "B", "C"]).await;
    assert!((200..300).contains(&status), "register: {body}");
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, false)
        ]
    );

    let (status, body) = register(&app, &token, &["C", "A", "B"]).await;
    assert!((200..300).contains(&status), "re-register: {body}");
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, false)
        ],
        "the stored indexes stand regardless of incoming order"
    );
}

#[tokio::test]
#[serial]
async fn a_new_column_appends_and_a_removed_one_retires_without_reuse() {
    let (_db, app, token) = setup().await;

    register(&app, &token, &["A", "B", "C"]).await;
    let (status, body) = register(&app, &token, &["A", "B", "C", "D"]).await;
    assert!((200..300).contains(&status), "append: {body}");
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, false),
            entry("D", 3, false),
        ]
    );

    let (status, body) = register(&app, &token, &["A", "B", "D"]).await;
    assert!((200..300).contains(&status), "retire: {body}");
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, true),
            entry("D", 3, false),
        ],
        "the removed column stays listed with its index reserved"
    );

    // A column added after a retirement lands past the highest index ever assigned, never on
    // the retired one.
    let (status, body) = register(&app, &token, &["A", "B", "D", "E"]).await;
    assert!((200..300).contains(&status), "append after retire: {body}");
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, true),
            entry("D", 3, false),
            entry("E", 4, false),
        ]
    );

    // A retired column that reappears reactivates at its stored index.
    let (status, body) = register(&app, &token, &["A", "B", "C", "D", "E"]).await;
    assert!((200..300).contains(&status), "reactivate: {body}");
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, false),
            entry("D", 3, false),
            entry("E", 4, false),
        ]
    );
}

#[tokio::test]
#[serial]
async fn an_ambiguous_re_registration_is_refused_naming_the_columns() {
    let (db, app, token) = setup().await;

    register(&app, &token, &["A", "B", "C"]).await;
    let (status, body) = register(&app, &token, &["A", "B", "X"]).await;
    assert_eq!(status, 409, "ambiguous re-registration ({status}): {body}");
    let text = body.to_string();
    assert!(
        text.contains('C') && text.contains('X'),
        "names both: {text}"
    );

    // The stored mapping is untouched by the refusal.
    let (status, body) = register(&app, &token, &["A", "B", "C"]).await;
    assert!(
        (200..300).contains(&status),
        "unchanged re-register: {body}"
    );
    assert_eq!(
        mapping(&body),
        vec![
            entry("A", 0, false),
            entry("B", 1, false),
            entry("C", 2, false)
        ]
    );
    let _ = db;
}

/// A spec stored before pinning (no assignments) reads its column positions as the indexes they
/// were, so a later re-registration in a different order preserves them.
#[tokio::test]
#[serial]
async fn a_pre_pinning_spec_derives_its_indexes_from_position() {
    let (db, app, token) = setup().await;

    let stream_id = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams \
                (id, source_system, source_key, is_active, measurement_type, metadata) \
             VALUES ('{stream_id}', '{SOURCE}', '{KEY}', true, 'spot', \
                     '{{\"replicates\": {{\"source_columns\": [\"A\", \"B\"]}}}}')"
        ),
    )
    .await;

    let (status, body) = register(&app, &token, &["B", "A"]).await;
    assert!((200..300).contains(&status), "re-register legacy: {body}");
    assert_eq!(
        mapping(&body),
        vec![entry("A", 0, false), entry("B", 1, false)],
        "positional indexes from the legacy spec are pinned, not re-derived from the new order"
    );
}

/// Scenario: an update through the generated CRUD router carrying a `metadata` body.
///
/// Expected behaviour: the pinned column-to-index assignments are the register handler's to write.
/// A CRUD edit cannot re-point a column at another column's index, nor erase the mapping with an
/// empty object, because both merge two replicate series into one on the next registration.
#[tokio::test]
#[serial]
async fn a_crud_update_cannot_rewrite_the_pinned_assignments() {
    let (db, app, token) = setup().await;

    let (status, body) = register(&app, &token, &["A", "B", "C"]).await;
    assert!((200..300).contains(&status), "register: {body}");
    let id = body["id"].as_str().expect("register returns the stream id");
    let pinned = stored_metadata(&db, id).await;

    for attempt in [
        json!({"metadata": {"replicates": {"assignments": [
            {"column": "A", "index": 0, "retired": false},
            {"column": "B", "index": 0, "retired": false},
            {"column": "C", "index": 0, "retired": false}
        ]}}}),
        json!({"metadata": {}}),
    ] {
        let (status, text) = crate::common::put_json_with_token(
            &app,
            &format!("/api/data_streams/{id}"),
            &attempt,
            &token,
        )
        .await;
        assert!(
            status != 500,
            "a metadata update must not error, it must be ignored: {status} {text}"
        );
        assert_eq!(
            stored_metadata(&db, id).await,
            pinned,
            "the pinned assignments survive {attempt}"
        );
    }
}

/// The stream's stored `metadata`, as the register handler last wrote it.
async fn stored_metadata(db: &DatabaseConnection, id: &str) -> String {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT metadata::text AS m FROM data_streams WHERE id = '{id}'"),
    ))
    .await
    .expect("query")
    .expect("the registered stream is stored")
    .try_get::<String>("", "m")
    .expect("metadata")
}
