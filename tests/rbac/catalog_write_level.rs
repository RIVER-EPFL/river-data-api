//! Who writes the lab's catalog: the global parameter list and the instrument inventory.
//!
//! Q24 settles both at MANAGER. A manager records an expected physical range on a parameter and
//! a manufacturer's range or a calibration on an instrument; that is the lab's own work, not an
//! administrative act. Everything below manager still reads and still cannot write.
//!
//! Run: cargo test --test rbac catalog_write_level -- --test-threads=1

use serde_json::json;
use serial_test::serial;

use crate::common::fixtures::GLOBAL_PARAM_TEMP_ID;
use crate::common::keycloak as kc;

async fn jwt(user: &str, role: &str) -> String {
    kc::ensure_realm_user(user, user, &[role]).await;
    kc::get_keycloak_jwt(user, user).await
}

#[tokio::test]
#[serial]
async fn a_manager_writes_the_parameter_catalog_and_the_instrument_inventory() {
    if !kc::keycloak_reachable().await {
        eprintln!("SKIP: keycloak unreachable (start the dev stack, or set TEST_KEYCLOAK_URL)");
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;

    let manager = jwt("manager1", "riverdata-manager").await;
    let river = jwt("river1", "riverdata-river").await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/parameters/{GLOBAL_PARAM_TEMP_ID}"),
        &json!({"default_max": 42.0}),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager edits a parameter's expected range ({status}): {body}"
    );

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensors",
        &json!({"serial_number": "MGR-1", "manufacturer": "Test"}),
        &manager,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a manager adds an instrument ({status}): {body}"
    );

    // One level down is the field worker: they enter measurements and their curves, not the
    // catalog the lab keeps.
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensors",
        &json!({"serial_number": "RIV-1", "manufacturer": "Test"}),
        &river,
    )
    .await;
    assert_eq!(status, 403, "a river member is refused ({status}): {body}");

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/parameters/{GLOBAL_PARAM_TEMP_ID}"),
        &river,
    )
    .await;
    assert_eq!(status, 200, "and still reads it: {body}");
}
