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
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
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
