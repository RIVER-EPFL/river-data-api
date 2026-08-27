//! `/ingest` materialising `samples` for spot replicate groups on a paired stream: groups of two
//! or more form on their own, a sync service declaring `collection` forms them from the first
//! reading, unpaired streams defer to the pairing backfill, a group whose replicate indices start
//! above zero keeps them and is still served, and an overwrite re-sync neither duplicates the
//! sample nor detaches it.
//!
//! Run: cargo test --test readings ingest_forms_samples -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-01T09:00:00Z";

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    Fixture { db, app, token }
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<f64>("", "v")
    .unwrap()
}

async fn register_spot_stream(fx: &Fixture, key: &str) -> String {
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "replform", "source_key": key, "measurement_type": "spot"}),
        &fx.token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {stream}"
    );
    crate::common::e2e::id_of(&stream)
}

async fn pair_to_temp_slot(fx: &Fixture, stream_id: &str) {
    let (status, body) = crate::common::post_json_with_token(
        &fx.app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_TEMP_ID}),
        &fx.token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
}

fn replicate_batch(time: &str, values: &[f64], start_index: i16) -> Vec<serde_json::Value> {
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({"time": time, "raw_value": v, "replicate_index": start_index + i as i16})
        })
        .collect()
}

async fn ingest(
    fx: &Fixture,
    token: &str,
    stream_id: &str,
    readings: Vec<serde_json::Value>,
    extra: serde_json::Value,
) -> serde_json::Value {
    let mut payload = json!({"stream_id": stream_id, "readings": readings});
    if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
        for (k, v) in source {
            target.insert(k.clone(), v.clone());
        }
    }
    let (status, body) =
        crate::common::post_json_parse_with_token(&fx.app, "/api/ingest", &payload, token).await;
    assert_eq!(status, 200, "ingest ({status}): {body}");
    body
}

fn temp_sample_where(time: &str) -> String {
    format!(
        "samples WHERE site_id = '{}' AND parameter_id = '{}' AND collected_at = '{time}'",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID
    )
}

async fn fetch_temp_series(fx: &Fixture, extra: &str) -> serde_json::Value {
    let uri = format!(
        "/api/sites/{}/readings?start=2025-06-01T00:00:00Z&end=2025-06-02T00:00:00Z\
         &parameter_ids={}&measurement_type=spot{extra}",
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
    );
    let (status, body) = crate::common::get_with_token(&fx.app, &uri, &fx.token).await;
    assert_eq!(status, 200, "readings fetch ({status}): {body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
#[serial]
async fn ingest_replicates_form_sample_on_paired_stream() {
    let fx = setup().await;
    let stream = register_spot_stream(&fx, "form-1").await;
    pair_to_temp_slot(&fx, &stream).await;

    let body = ingest(
        &fx,
        &fx.token,
        &stream,
        replicate_batch(T1, &[10.0, 20.0, 30.0], 0),
        json!({}),
    )
    .await;
    assert_eq!(body["inserted"], 3);

    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        1,
        "the replicate group formed one sample"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT n::bigint AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        3
    );
    let stdev = scalar_f64(
        &fx.db,
        &format!("SELECT stdev AS v FROM {}", temp_sample_where(T1)),
    )
    .await;
    assert!(
        (stdev - 10.0).abs() < 1e-9,
        "sample stdev of 10/20/30: {stdev}"
    );

    let series = fetch_temp_series(&fx, "&include_sample_stats=true").await;
    let values = series["parameters"][0]["values"].as_array().unwrap();
    assert_eq!(values.len(), 1, "one point per replicate group: {series}");
    assert!(
        (values[0].as_f64().unwrap() - 20.0).abs() < 1e-9,
        "the served value is the sample mean: {values:?}"
    );
    let stats = &series["parameters"][0]["samples"][0];
    assert_eq!(stats["n"], 3, "sample stats attached: {series}");
    assert_eq!(
        stats["replicates"].as_array().unwrap().len(),
        3,
        "all replicates listed: {stats}"
    );
}

#[tokio::test]
#[serial]
async fn late_replicate_joins_existing_sample() {
    let fx = setup().await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&fx.db).await;
    let stream = register_spot_stream(&fx, "form-late").await;
    pair_to_temp_slot(&fx, &stream).await;

    let body = ingest(
        &fx,
        &sync_token,
        &stream,
        replicate_batch(T1, &[10.0, 20.0], 0),
        json!({"collection": true}),
    )
    .await;
    assert_eq!(body["inserted"], 2);
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT n::bigint AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        2
    );

    let body = ingest(
        &fx,
        &sync_token,
        &stream,
        replicate_batch(T1, &[30.0], 2),
        json!({"collection": true}),
    )
    .await;
    assert_eq!(body["inserted"], 1);

    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        1,
        "the late replicate joined the existing sample rather than minting a second"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT n::bigint AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        3
    );
}

#[tokio::test]
#[serial]
async fn undeclared_pair_still_forms() {
    let fx = setup().await;
    let stream = register_spot_stream(&fx, "form-undeclared").await;
    pair_to_temp_slot(&fx, &stream).await;

    let mut readings = replicate_batch(T1, &[10.0, 20.0], 0);
    readings.extend(replicate_batch(T2, &[99.0], 0));
    ingest(&fx, &fx.token, &stream, readings, json!({})).await;

    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        1,
        "two replicates form a sample without a collection declaration"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T2))
        )
        .await,
        0,
        "an undeclared single reading is not a sample"
    );
}

#[tokio::test]
#[serial]
async fn unpaired_ingest_defers_to_pairing() {
    let fx = setup().await;
    let stream = register_spot_stream(&fx, "form-unpaired").await;

    let body = ingest(
        &fx,
        &fx.token,
        &stream,
        replicate_batch(T1, &[10.0, 20.0], 0),
        json!({}),
    )
    .await;
    assert_eq!(body["paired"], false);
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        0,
        "unpaired readings form no sample"
    );

    pair_to_temp_slot(&fx, &stream).await;
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT n::bigint AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        2,
        "the pairing backfill materialises the deferred group"
    );
}

#[tokio::test]
#[serial]
async fn sparse_member_indices_are_preserved_and_served() {
    let fx = setup().await;
    let stream = register_spot_stream(&fx, "form-sparse").await;
    pair_to_temp_slot(&fx, &stream).await;

    ingest(
        &fx,
        &fx.token,
        &stream,
        replicate_batch(T1, &[10.0, 20.0], 1),
        json!({}),
    )
    .await;

    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!(
                "SELECT MIN(replicate_index)::bigint AS n FROM readings \
                 WHERE stream_id = '{stream}' AND time = '{T1}'"
            ),
        )
        .await,
        1,
        "the source's column positions are left alone"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT n::bigint AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        2
    );

    let series = fetch_temp_series(&fx, "").await;
    let values = series["parameters"][0]["values"].as_array().unwrap();
    assert_eq!(
        values.len(),
        1,
        "the group is served without an index-0 row: {series}"
    );
    assert!(
        (values[0].as_f64().unwrap() - 15.0).abs() < 1e-9,
        "served value is the group mean: {values:?}"
    );
}

#[tokio::test]
#[serial]
async fn overwrite_resync_idempotent() {
    let fx = setup().await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&fx.db).await;
    let stream = register_spot_stream(&fx, "form-overwrite").await;
    pair_to_temp_slot(&fx, &stream).await;

    let batch = replicate_batch(T1, &[10.0, 20.0, 30.0], 0);
    ingest(&fx, &sync_token, &stream, batch.clone(), json!({})).await;

    let sample_id = fx
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT id::text AS id FROM {}", temp_sample_where(T1)),
        ))
        .await
        .unwrap()
        .expect("sample formed")
        .try_get::<String>("", "id")
        .unwrap();

    ingest(&fx, &sync_token, &stream, batch, json!({"overwrite": true})).await;
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        1,
        "an identical re-sync accumulates no second sample"
    );
    let mean = scalar_f64(
        &fx.db,
        &format!("SELECT mean AS v FROM {}", temp_sample_where(T1)),
    )
    .await;
    assert!(
        (mean - 20.0).abs() < 1e-9,
        "stats stable on re-sync: {mean}"
    );

    ingest(
        &fx,
        &sync_token,
        &stream,
        replicate_batch(T1, &[10.0, 20.0, 60.0], 0),
        json!({"overwrite": true}),
    )
    .await;
    let mean = scalar_f64(
        &fx.db,
        &format!("SELECT mean AS v FROM {}", temp_sample_where(T1)),
    )
    .await;
    assert!(
        (mean - 30.0).abs() < 1e-9,
        "a corrected replicate moves the mean: {mean}"
    );
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!(
                "SELECT COUNT(*) AS n FROM {} AND id = '{sample_id}'",
                temp_sample_where(T1)
            ),
        )
        .await,
        1,
        "the correction kept the same sample row"
    );
}

#[tokio::test]
#[serial]
async fn non_spot_replicates_never_sample() {
    let fx = setup().await;
    let (status, stream) = crate::common::post_json_parse_with_token(
        &fx.app,
        "/api/streams/register",
        &json!({"source_system": "replform", "source_key": "form-cont",
                "measurement_type": "continuous"}),
        &fx.token,
    )
    .await;
    assert!((200..300).contains(&status), "register: {stream}");
    let stream = crate::common::e2e::id_of(&stream);
    pair_to_temp_slot(&fx, &stream).await;

    let body = ingest(
        &fx,
        &fx.token,
        &stream,
        replicate_batch(T1, &[10.0, 20.0], 0),
        json!({}),
    )
    .await;
    assert_eq!(body["inserted"], 2);
    assert_eq!(
        scalar_i64(
            &fx.db,
            &format!("SELECT COUNT(*) AS n FROM {}", temp_sample_where(T1))
        )
        .await,
        0,
        "continuous readings sharing an instant are not a collection event"
    );
}
