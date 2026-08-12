//! Boundary and rejection behaviour of the deployment timeline: what a refused write must leave
//! behind, which overlaps are refused at all, and how a corrected move date moves the neighbouring
//! boundary with it.
//!
//! Run: cargo test --test sensor_deployments -- --test-threads=1

use crate::common::e2e;
use crate::common::sensor_lifecycle::*;
use crate::common::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

async fn deployment_window(
    db: &DatabaseConnection,
    id: &str,
) -> (
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
) {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT deployed_from, deployed_until FROM sensor_deployments WHERE id = '{id}'"
            ),
        ))
        .await
        .expect("query sensor_deployments")
        .unwrap_or_else(|| panic!("deployment {id} row"));
    let from: chrono::DateTime<chrono::FixedOffset> =
        row.try_get("", "deployed_from").expect("deployed_from");
    let until: Option<chrono::DateTime<chrono::FixedOffset>> =
        row.try_get("", "deployed_until").expect("deployed_until");
    (
        from.with_timezone(&chrono::Utc),
        until.map(|t| t.with_timezone(&chrono::Utc)),
    )
}

async fn deployment_count(db: &DatabaseConnection, sensor: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT count(*) AS c FROM sensor_deployments WHERE sensor_id = '{sensor}'"),
        ))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

/// Scenario: a second instrument already holds the slot the operator is deploying into.
/// Expected behaviour: the request is refused AND the instrument being deployed is still deployed
/// where it was. The recall used to run ahead of the check, so a 400 closed the open deployment
/// anyway and the next reprocess un-attributed everything logged after it.
#[tokio::test]
#[serial]
async fn a_refused_create_leaves_the_open_deployment_open() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let mover = create_sensor(&db, "boundary-mover", GLOBAL_PARAM_TEMP_ID).await;
    let incumbent = create_sensor(&db, "boundary-incumbent", GLOBAL_PARAM_TEMP_ID).await;

    let open = e2e::create_deployment(
        &app,
        &token,
        &mover.id.to_string(),
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-03-01T00:00:00Z",
    )
    .await;
    e2e::create_deployment(
        &app,
        &token,
        &incumbent.id.to_string(),
        SITE2_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-03-01T00:00:00Z",
    )
    .await;

    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": mover.id,
            "site_id": SITE2_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-03-05T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "the occupied slot is refused: {body}");

    assert_eq!(
        deployment_window(&db, &open).await.1,
        None,
        "the refused deploy left the instrument exactly where it was"
    );
    assert_eq!(
        deployment_count(&db, &mover.id.to_string()).await,
        1,
        "and wrote nothing"
    );
}

/// The exclusion constraint carries no sensor term, so an instrument collides with its own
/// historical deployment at the same slot. A check filtered on `sensor_id <> incoming` misses it
/// and the write reaches the constraint as a raw 500.
#[tokio::test]
#[serial]
async fn a_same_sensor_historical_overlap_is_refused_not_raised() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "boundary-self", GLOBAL_PARAM_TEMP_ID).await;
    let sid = sensor.id.to_string();

    let first = e2e::create_deployment(
        &app,
        &token,
        &sid,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-03-01T00:00:00Z",
    )
    .await;
    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{first}"),
        &json!({ "deployed_until": "2025-03-10T00:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "close the first window ({status}): {body}"
    );

    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": sensor.id,
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-03-05T00:00:00Z",
            "deployed_until": "2025-03-20T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "overlapping its own closed window is a refusal, not a constraint violation: {body}"
    );
    assert_eq!(
        deployment_count(&db, &sid).await,
        1,
        "and nothing was written"
    );
}

/// The instant a window opens belongs to that window, the instant it closes does not: adjacent
/// deployments meeting at one instant are not an overlap.
#[tokio::test]
#[serial]
async fn adjacent_windows_meeting_at_one_instant_are_not_an_overlap() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let first = create_sensor(&db, "boundary-first", GLOBAL_PARAM_TEMP_ID).await;
    let second = create_sensor(&db, "boundary-second", GLOBAL_PARAM_TEMP_ID).await;

    let opening = e2e::create_deployment(
        &app,
        &token,
        &first.id.to_string(),
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-04-01T00:00:00Z",
    )
    .await;
    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{opening}"),
        &json!({ "deployed_until": "2025-04-10T00:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "close the first window ({status}): {body}"
    );

    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": second.id,
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-04-10T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "a window opening exactly where the previous one closed is accepted ({status}): {body}"
    );

    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": second.id,
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-04-09T23:59:59Z",
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "one second earlier and the windows overlap: {body}"
    );
}

/// A window whose end precedes its start is not a range the database can hold.
#[tokio::test]
#[serial]
async fn an_inverted_window_is_refused() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "boundary-inverted", GLOBAL_PARAM_TEMP_ID).await;
    let (status, body) = post_json_with_token(
        &app,
        "/api/sensor_deployments",
        &json!({
            "sensor_id": sensor.id,
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_TEMP_ID,
            "deployed_from": "2025-05-10T00:00:00Z",
            "deployed_until": "2025-05-01T00:00:00Z",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 400, "an inverted window is refused: {body}");
    assert_eq!(
        deployment_count(&db, &sensor.id.to_string()).await,
        0,
        "and nothing was written"
    );
}

/// Scenario: an instrument moved from site 1 to site 2, and the recorded move date was an hour too
/// early. Correcting it forward must hand the vacated hour back to site 1.
/// Expected behaviour: the predecessor that ended exactly at the old move instant follows the
/// correction, so the timeline has no hole.
#[tokio::test]
#[serial]
async fn a_move_date_corrected_forward_carries_the_previous_windows_end_with_it() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "boundary-forward", GLOBAL_PARAM_TEMP_ID).await;
    let sid = sensor.id.to_string();

    let upstream = e2e::create_deployment(
        &app,
        &token,
        &sid,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;
    let downstream = e2e::create_deployment(
        &app,
        &token,
        &sid,
        SITE2_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T02:00:00Z",
    )
    .await;
    assert_eq!(
        deployment_window(&db, &upstream).await.1,
        Some(dt("2025-06-01T02:00:00Z")),
        "the move closed the upstream window at the move instant"
    );

    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{downstream}"),
        &json!({ "deployed_from": "2025-06-01T05:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "correct the move date ({status}): {body}"
    );

    assert_eq!(
        deployment_window(&db, &upstream).await.1,
        Some(dt("2025-06-01T05:00:00Z")),
        "the upstream window follows the correction, leaving no hole in the timeline"
    );
    assert_eq!(
        deployment_window(&db, &downstream).await,
        (dt("2025-06-01T05:00:00Z"), None),
        "and the corrected deployment starts at the corrected instant, still open"
    );
}

/// The mirror of the forward correction: a window that ended before the next one began had a real
/// gap after it (the instrument sat in the lab), and correcting the later start must not swallow it.
#[tokio::test]
#[serial]
async fn a_deliberate_gap_survives_a_forward_correction() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let sensor = create_sensor(&db, "boundary-gap", GLOBAL_PARAM_TEMP_ID).await;
    let sid = sensor.id.to_string();

    let campaign = e2e::create_deployment(
        &app,
        &token,
        &sid,
        SITE1_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-07-01T00:00:00Z",
    )
    .await;
    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{campaign}"),
        &json!({ "deployed_until": "2025-07-02T00:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "close the first campaign ({status}): {body}"
    );

    let next = e2e::create_deployment(
        &app,
        &token,
        &sid,
        SITE2_ID,
        GLOBAL_PARAM_TEMP_ID,
        "2025-07-10T00:00:00Z",
    )
    .await;

    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/sensor_deployments/{next}"),
        &json!({ "deployed_from": "2025-07-12T00:00:00Z" }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "correct the second start ({status}): {body}"
    );

    assert_eq!(
        deployment_window(&db, &campaign).await.1,
        Some(dt("2025-07-02T00:00:00Z")),
        "the lab period between the campaigns is preserved: only an adjacent window follows"
    );
}
