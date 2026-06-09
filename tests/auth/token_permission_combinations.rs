//! Every one of the 16 permission-bit combinations against a representative endpoint per
//! capability. Proves the four capability bits (read_metadata, read_data, write_metadata,
//! write_data) are independently enforced — including that annotations/samples reads require
//! `read_data`, not `read_metadata` (a metadata-only key must not read time-series data).
//!
//! Tokens here are unscoped so the probes isolate the capability bits from project scope.


use serial_test::serial;

use crate::common::fixtures::{GLOBAL_PARAM_TEMP_ID, SITE1_ID};

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[tokio::test]
#[serial]
async fn all_sixteen_permission_combinations_enforced() {
    let (db, app) = setup().await;
    let readings_range = "start=2025-01-15T00:00:00Z&end=2025-01-15T12:00:00Z";

    for bits in 0u8..16 {
        let read_metadata = bits & 1 != 0;
        let read_data = bits & 2 != 0;
        let write_metadata = bits & 4 != 0;
        let write_data = bits & 8 != 0;

        let token = crate::common::seed_api_token(
            &db,
            crate::common::perms(read_metadata, read_data, write_metadata, write_data),
            None,
        )
        .await;
        let label = format!("rm={read_metadata} rd={read_data} wm={write_metadata} wd={write_data}");

        // read_metadata: list a catalog entity.
        let (s, _) = crate::common::get_with_token(&app, "/api/sites", &token).await;
        assert_eq!(s == 200, read_metadata, "[{label}] GET /api/sites: got {s}");
        assert!(read_metadata || s == 403, "[{label}] GET /api/sites should 403 without read_metadata, got {s}");

        // read_data: time-series read.
        let (s, _) = crate::common::get_with_token(
            &app,
            &format!("/api/sites/{SITE1_ID}/readings?{readings_range}"),
            &token,
        )
        .await;
        assert_eq!(s == 200, read_data, "[{label}] GET readings: got {s}");

        // read_data via a CRUD entity whose rows are data (A2 regression): annotations need
        // read_data, NOT read_metadata. A metadata-only key must be denied here.
        let (s, _) = crate::common::get_with_token(&app, "/api/annotations", &token).await;
        assert_eq!(s == 200, read_data, "[{label}] GET /api/annotations needs read_data: got {s}");
        let (s, _) = crate::common::get_with_token(&app, "/api/samples", &token).await;
        assert_eq!(s == 200, read_data, "[{label}] GET /api/samples needs read_data: got {s}");

        // write_metadata: create a global catalog entity (unique code per combo, no collision).
        let new_param = serde_json::json!({
            "code": format!("permcombo_{bits}"),
            "name": format!("Perm Combo {bits}"),
            "default_units": "x",
            "category": "measurement",
            "aliases": []
        });
        let (s, _) = crate::common::post_json_with_token(&app, "/api/parameters", &new_param, &token).await;
        if write_metadata {
            assert_ne!(s, 403, "[{label}] write_metadata key must not be 403 on parameter create");
        } else {
            assert_eq!(s, 403, "[{label}] non-write_metadata key must be 403 on parameter create, got {s}");
        }

        // write_data: ingest a reading.
        let batch = serde_json::json!({
            "readings": [{ "site_id": SITE1_ID, "parameter_id": GLOBAL_PARAM_TEMP_ID, "time": now_rfc3339(), "raw_value": 1.0 }]
        });
        let (s, _) = crate::common::post_json_with_token(&app, "/api/readings/batch", &batch, &token).await;
        if write_data {
            assert_ne!(s, 403, "[{label}] write_data key must not be 403 on batch ingest");
        } else {
            assert_eq!(s, 403, "[{label}] non-write_data key must be 403 on batch ingest, got {s}");
        }
    }
}
