//! T41, the operator's full sync from the command to the landed rows.
//!
//! The button on the System page is what an operator reaches for when they suspect the portal
//! holds something river-data does not. Its whole value is that it re-asserts content a scheduled
//! cycle will not: a reconciled source re-reads everything every cycle, but the digest handshake
//! means an unchanged source sends nothing, so server-side drift is invisible to it. A command
//! that marks itself complete having re-asserted nothing is worse than no button.
//!
//! Run: cargo test --test e2e portal_full_sync_command -- --test-threads=1

use river_data_core::client::SyncService;
use river_data_core::models::ServiceStatus;
use serial_test::serial;

use crate::common::e2e::count;
use crate::common::fake_portal::{FAMILY_MEAN_COLUMN, FakePortal, SOURCE_SYSTEM, enrolled_service};

/// The readings a source key's stream holds, or zero where no stream carries that key.
async fn readings_at(db: &sea_orm::DatabaseConnection, source_key: &str) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings r \
             JOIN data_streams s ON s.id = r.stream_id \
             WHERE s.source_system = '{SOURCE_SYSTEM}' AND s.source_key = '{source_key}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_full_sync_command_re_asserts_what_a_scheduled_cycle_will_not() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let token = crate::common::seed_token_full(&db).await;
    let (driver, mut control, service_id) =
        enrolled_service(app.clone(), &state, FakePortal::seeded()).await;

    let family = format!("S01:{FAMILY_MEAN_COLUMN}:reps");

    // The source registers and pushes, and a second cycle over unchanged content sends nothing.
    driver.sync(false).await.expect("the first cycle");
    let landed = readings_at(&db, &family).await;
    assert!(landed > 0, "the first cycle landed the family's replicates");
    let second = driver.sync(false).await.expect("the second cycle");
    assert_eq!(second.readings_synced, 0, "unchanged content is not re-sent");

    // Drift the server holds and the source cannot see: the stream and its readings are gone.
    // This is the shape the button exists for, and it is invisible to the digest, which is a
    // statement about the source's content and not about what the store kept.
    use sea_orm::ConnectionTrait;
    for sql in [
        format!(
            "DELETE FROM readings WHERE stream_id IN (SELECT id FROM data_streams \
             WHERE source_system = '{SOURCE_SYSTEM}' AND source_key = '{family}')"
        ),
        format!(
            "DELETE FROM data_streams WHERE source_system = '{SOURCE_SYSTEM}' \
             AND source_key = '{family}'"
        ),
    ] {
        db.execute_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql,
        ))
        .await
        .expect("drop the stream and its readings");
    }
    assert_eq!(readings_at(&db, &family).await, 0, "the drift is in place");

    // A scheduled cycle does not repair it: the descriptor is unchanged, so the client does not
    // re-register it, and there is no stream left to send readings to.
    let scheduled = driver.sync(false).await.expect("a scheduled cycle");
    assert!(scheduled.errors.is_empty(), "{:?}", scheduled.errors);
    assert_eq!(
        readings_at(&db, &family).await,
        0,
        "a scheduled cycle re-asserts nothing the source has not changed"
    );

    // The operator presses the button.
    let (status, issued) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/sync/services/{service_id}/commands"),
        &serde_json::json!({ "command": "trigger_full_sync" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the command is queued: {issued}");
    let command_id = issued["id"].as_str().expect("the command's id").to_string();

    // The service collects it on its next heartbeat, which is how a command reaches a running
    // runner: nothing pushes to it.
    let beat = control
        .heartbeat(service_id, ServiceStatus::Idle, None)
        .await
        .expect("heartbeat");
    let collected = beat
        .pending_commands
        .iter()
        .find(|c| c.id.to_string() == command_id)
        .expect("the full sync command is handed to the service on its heartbeat");
    assert_eq!(collected.command, "trigger_full_sync");

    // What the runner does with it: a cycle that re-asserts everything, descriptors included.
    let full = driver.sync(true).await.expect("the full cycle");
    assert!(full.errors.is_empty(), "{:?}", full.errors);
    assert_eq!(
        readings_at(&db, &family).await,
        landed,
        "the full pass re-registered the stream and re-asserted its rows"
    );
    assert!(
        full.readings_synced > 0,
        "the full pass carried readings where the scheduled one carried none: {full:?}"
    );
}
