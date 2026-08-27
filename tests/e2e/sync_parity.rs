//! The Phase 1 sync-parity acceptance story, driven exactly as the sync service drives the API:
//! a replicate family registers with a pinned spec, a sparse group lands at its source indexes
//! with a disagreeing portal audit, pairing materialises the samples that serve the slot, a
//! resync is a no-op, an upstream column reorder changes nothing, a flag resolution moves the
//! served value by true index, and a portal-side edit converges through an overwrite re-send.
//!
//! Run: cargo test --test e2e sync_parity -- --test-threads=1

use crate::common::e2e;
use crate::common::keycloak as kc;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-01T09:00:00Z";

/// The portals store aggregate cells at 2 decimals, so served-value comparisons are held to the
/// stored quantum rather than to float equality.
const QUANTUM: f64 = 0.005;

const SOURCE: &str = "cnet";
const FAMILY_KEY: &str = "STA:NO3_avg_ppb:reps";

async fn register_family(
    app: &axum::Router,
    token: &str,
    columns: &[&str],
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": SOURCE,
            "source_key": FAMILY_KEY,
            "measurement_type": "spot",
            "replicates": { "source_columns": columns },
        }),
        token,
    )
    .await
}

/// The pinned column-to-index mapping out of a register response.
fn mapping(body: &serde_json::Value) -> Vec<(String, i64)> {
    body["replicates"]
        .as_array()
        .unwrap_or_else(|| panic!("register response carries the mapping: {body}"))
        .iter()
        .map(|a| {
            (
                a["column"].as_str().unwrap().to_string(),
                a["index"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn index_of(mapping: &[(String, i64)], column: &str) -> i64 {
    mapping
        .iter()
        .find(|(c, _)| c == column)
        .unwrap_or_else(|| panic!("column {column} missing from mapping {mapping:?}"))
        .1
}

/// The replicate indexes stored for a stream at one instant, ascending.
async fn stored_indexes(db: &DatabaseConnection, stream_id: &str, time: &str) -> Vec<i64> {
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT replicate_index::bigint AS i FROM readings \
                 WHERE stream_id = '{stream_id}' AND time = '{time}' ORDER BY replicate_index"
            ),
        ))
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.try_get::<i64>("", "i").unwrap())
        .collect()
}

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap_or_else(|| panic!("no row for: {sql}"))
    .try_get::<f64>("", "v")
    .unwrap()
}

/// The one value the site serves for a parameter at one instant, through the spot arm.
async fn served_value(app: &axum::Router, token: &str, site: &str, param: &str, time: &str) -> f64 {
    let uri = format!(
        "/api/sites/{site}/readings?start={time}&end={time}\
         &parameter_ids={param}&measurement_type=spot"
    );
    let (status, series) = crate::common::get_json_with_token(app, &uri, token).await;
    assert_eq!(status, 200, "site series: {series}");
    let values = e2e::values_for(&series, param);
    assert_eq!(values.len(), 1, "one served point at {time}: {series}");
    values[0]
}

#[tokio::test]
#[serial]
async fn sync_parity_family_lifecycle() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let project = e2e::create_project(&app, &token, "Parity P", "parity-p", false).await;
    let site = e2e::create_site(&app, &token, &project, "STA", "sta").await;
    let param = e2e::create_parameter(&app, &token, "no3lab", "Nitrate", "ppb").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;

    let register_with_spec = async |columns: &[&str]| {
        let (status, body) = register_family(&app, &sync_token, columns).await;
        assert_eq!(status, 200, "register family ({status}): {body}");
        body
    };
    let registered = register_with_spec(&["NO3_1_ppb", "NO3_2_ppb", "NO3_3_ppb"]).await;
    let stream_id = e2e::id_of(&registered);
    let pinned = mapping(&registered);
    assert_eq!(
        pinned,
        vec![
            ("NO3_1_ppb".to_string(), 0),
            ("NO3_2_ppb".to_string(), 1),
            ("NO3_3_ppb".to_string(), 2)
        ],
        "assignments are pinned in declaration order at first registration"
    );

    // The sparse group: the portal row holds values only for columns 2 and 3, and the sync
    // service addresses each by its pinned index, so nothing ever lands at index 0.
    let sparse_group = |v1: f64, v2: f64| {
        json!([
            {"time": T1, "raw_value": v1,
             "replicate_index": index_of(&pinned, "NO3_2_ppb")},
            {"time": T1, "raw_value": v2,
             "replicate_index": index_of(&pinned, "NO3_3_ppb")},
        ])
    };
    // Expected mean 16.0 against a recomputed 15.0: outside every tolerance, so the audit
    // records a hold while the readings are admitted.
    let disagreeing_audit = json!([{"time": T1, "expected_mean": 16.0, "expected_sd": 7.071}]);
    let ingest_body = json!({
        "stream_id": stream_id,
        "readings": sparse_group(10.0, 20.0),
        "collection": true,
        "audit": disagreeing_audit,
    });
    let (status, body) =
        crate::common::post_json_parse_with_token(&app, "/api/ingest", &ingest_body, &sync_token)
            .await;
    assert_eq!(status, 200, "sparse ingest ({status}): {body}");
    assert_eq!(body["inserted"], 2, "both replicates admitted: {body}");
    assert_eq!(body["paired"], false);

    assert_eq!(
        stored_indexes(&db, &stream_id, T1).await,
        vec![1, 2],
        "the group is stored at its source indexes exactly, no index 0"
    );
    let hold = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id, status, computed->'values' AS values \
                 FROM replicate_audit_holds WHERE stream_id = '{stream_id}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the disagreeing group recorded a hold");
    let hold_id: String = hold.try_get("", "id").unwrap();
    assert_eq!(
        hold.try_get::<String>("", "status").unwrap(),
        "deferred",
        "an unpaired stream's hold waits for pairing"
    );
    let recorded: serde_json::Value = hold.try_get("", "values").unwrap();
    assert_eq!(
        recorded,
        json!([{"index": 1, "value": 10.0}, {"index": 2, "value": 20.0}]),
        "the hold records each value at its replicate index"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": sp}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");
    let sample_mean = scalar_f64(
        &db,
        &format!(
            "SELECT mean AS v FROM samples WHERE site_id = '{site}' \
             AND parameter_id = '{param}' AND collected_at = '{T1}'"
        ),
    )
    .await;
    assert!(
        (sample_mean - 15.0).abs() <= QUANTUM,
        "pairing materialised the sample with the trigger-computed mean: {sample_mean}"
    );
    let served = served_value(&app, &token, &site, &param, T1).await;
    assert!(
        (served - sample_mean).abs() <= QUANTUM,
        "the site serves the trigger-computed mean: {served} vs {sample_mean}"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM replicate_audit_holds \
                 WHERE id = '{hold_id}' AND status = 'pending'"
            ),
        )
        .await,
        1,
        "pairing promoted the deferred hold to the review queue"
    );

    let (status, body) =
        crate::common::post_json_parse_with_token(&app, "/api/ingest", &ingest_body, &sync_token)
            .await;
    assert_eq!(status, 200, "resync ({status}): {body}");
    assert_eq!(body["inserted"], 0, "a resync of the same rows is a no-op");
    assert_eq!(stored_indexes(&db, &stream_id, T1).await, vec![1, 2]);
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT COUNT(*) FROM replicate_audit_holds WHERE stream_id = '{stream_id}'"),
        )
        .await,
        1,
        "re-detection refreshes the open hold, never duplicates it"
    );
    let resynced = served_value(&app, &token, &site, &param, T1).await;
    assert!(
        (resynced - 15.0).abs() <= QUANTUM,
        "the served mean is unchanged by the resync: {resynced}"
    );

    let reordered = register_with_spec(&["NO3_3_ppb", "NO3_1_ppb", "NO3_2_ppb"]).await;
    let remapped = mapping(&reordered);
    assert_eq!(
        remapped, pinned,
        "an upstream reorder is a no-op against the pinned assignments"
    );
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "readings": [
                {"time": T2, "raw_value": 30.0,
                 "replicate_index": index_of(&remapped, "NO3_2_ppb")},
                {"time": T2, "raw_value": 40.0,
                 "replicate_index": index_of(&remapped, "NO3_3_ppb")},
            ],
            "collection": true,
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "post-reorder ingest ({status}): {body}");
    assert_eq!(body["inserted"], 2);
    assert_eq!(
        stored_indexes(&db, &stream_id, T2).await,
        vec![1, 2],
        "the same columns land on the same indexes after the reorder"
    );

    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sync/replicate_audit_holds/{hold_id}/resolve"),
        &json!({"mode": "flag", "replicate_indexes": [2]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "flag resolution ({status}): {body}");
    assert_eq!(body["status"], "remediated");
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{stream_id}' \
                 AND time = '{T1}' AND is_flagged IS TRUE"
            ),
        )
        .await,
        1,
        "exactly one reading is flagged"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{stream_id}' \
                 AND time = '{T1}' AND replicate_index = 2 AND is_flagged IS TRUE"
            ),
        )
        .await,
        1,
        "the flag landed on the named index"
    );
    let remediated = served_value(&app, &token, &site, &param, T1).await;
    assert!(
        (remediated - 10.0).abs() <= QUANTUM,
        "the served value is the mean over the surviving replicate: {remediated}"
    );

    // A portal-side edit re-sent by the sync service: overwrite updates the stored value in
    // place, and operator state on the group (the flag on index 2) survives the re-send.
    let (status, body) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &json!({
            "stream_id": stream_id,
            "readings": sparse_group(12.0, 20.0),
            "collection": true,
            "overwrite": true,
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "overwrite re-send ({status}): {body}");
    let edited = scalar_f64(
        &db,
        &format!(
            "SELECT raw_value AS v FROM readings WHERE stream_id = '{stream_id}' \
             AND time = '{T1}' AND replicate_index = 1"
        ),
    )
    .await;
    assert!(
        (edited - 12.0).abs() < 1e-9,
        "the source edit converged onto the stored reading: {edited}"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM readings WHERE stream_id = '{stream_id}' \
                 AND time = '{T1}' AND replicate_index = 2 AND is_flagged IS TRUE"
            ),
        )
        .await,
        1,
        "the overwrite preserved the flag"
    );
    let converged = served_value(&app, &token, &site, &param, T1).await;
    assert!(
        (converged - 12.0).abs() <= QUANTUM,
        "the served mean follows the corrected value: {converged}"
    );

    if kc::require_keycloak_or_skip("sync_parity_family_lifecycle authorization sweep").await {
        let kc_app = kc::build_test_app_with_keycloak(db.clone()).await;
        kc::ensure_realm_user("river1", "river1", &["riverdata-river"]).await;
        kc::grant_project(&db, &kc::keycloak_user_id("river1").await, &project).await;
        let river = kc::get_keycloak_jwt("river1", "river1").await;

        let (status, body) = crate::common::post_json_with_token(
            &kc_app,
            "/api/streams/register",
            &json!({
                "source_system": SOURCE,
                "source_key": FAMILY_KEY,
                "measurement_type": "spot",
                "replicates": { "source_columns": ["NO3_1_ppb", "NO3_2_ppb", "NO3_3_ppb"] },
            }),
            &river,
        )
        .await;
        assert_eq!(
            status, 403,
            "stream registration requires Administrator or a write_metadata token: {body}"
        );
        let (status, body) = crate::common::post_json_with_token(
            &kc_app,
            &format!("/api/sync/replicate_audit_holds/{hold_id}/resolve"),
            &json!({"mode": "ours"}),
            &river,
        )
        .await;
        assert_eq!(
            status, 403,
            "hold resolution requires the manager level: {body}"
        );
        assert_eq!(
            e2e::count(
                &db,
                &format!(
                    "SELECT COUNT(*) FROM replicate_audit_holds \
                     WHERE id = '{hold_id}' AND status = 'remediated'"
                ),
            )
            .await,
            1,
            "the refused attempts changed nothing"
        );
    }
}
