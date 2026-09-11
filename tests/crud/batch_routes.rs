//! Every generated batch route answers, and runs the checks the single-row route runs.
//!
//! Scenario: a client posts, patches or deletes a batch on an entity that declares `operations`.
//! Expected behaviour: the request is served (the crudcrate default for such an entity delegates
//! to the resource, which delegates back to the operations struct, so an unguarded batch method
//! recurses until the stack goes and takes the process with it), and the guards and hooks the
//! single-row path applies apply to every row of the batch.

use crate::common::e2e;
use crate::common::sensor_lifecycle::*;
use crate::common::*;
use serial_test::serial;

/// The CRUD routes whose entity declares `operations`, minus `/tokens`, which is admin-only and
/// admits no API token, and `/reprocessing_jobs`, which mounts its read routes only.
const BATCHED: [&str; 12] = [
    "/api/parameters",
    "/api/site_parameters",
    "/api/sensors",
    "/api/sensor_calibrations",
    "/api/sensor_deployments",
    "/api/standard_curves",
    "/api/derived_parameters",
    "/api/parameter_groups",
    "/api/parameter_group_members",
    "/api/alarm_thresholds",
    "/api/data_streams",
    "/api/collection_events",
];

#[tokio::test]
#[serial]
async fn every_batch_route_answers() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let app = build_test_app(db.clone());
    let token = seed_token_full(&db).await;

    for route in BATCHED {
        let batch = format!("{route}/batch");

        let (status, body) =
            post_json_with_token(&app, &batch, &serde_json::json!([]), &token).await;
        assert!(
            status < 400,
            "POST {batch} on an empty batch: {status} {body}"
        );

        let (status, body) =
            patch_json_with_token(&app, &batch, &serde_json::json!([]), &token).await;
        assert!(
            status < 400,
            "PATCH {batch} on an empty batch: {status} {body}"
        );

        let (status, body) =
            delete_json_with_token(&app, &batch, &serde_json::json!([]), &token).await;
        assert!(
            status < 400,
            "DELETE {batch} on an empty batch: {status} {body}"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_deployment_batch_edit_meets_the_slot_overlap_guard() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Batch-overlap-01", GLOBAL_PARAM_TEMP_ID).await;
    let first = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    end_deployment(&db, first, dt("2025-05-01T00:00:00Z")).await;
    let second = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-06-01T00:00:00Z")).await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    // Moving the later deployment back over the earlier one is what `before_update` refuses.
    let (status, body) = patch_json_with_token(
        &app,
        "/api/sensor_deployments/batch",
        &serde_json::json!([{
            "id": second,
            "deployed_from": "2025-02-01T00:00:00Z",
        }]),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a batch edit overlapping the slot is refused: {status} {body}"
    );

    let overlapping = e2e::count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM sensor_deployments \
             WHERE id = '{second}' AND deployed_from = '2025-02-01T00:00:00Z'"
        ),
    )
    .await;
    assert_eq!(overlapping, 0, "the refused edit stored nothing");
    assert!(first != second, "two deployments were seeded");
}

#[tokio::test]
#[serial]
async fn a_deployment_batch_delete_reprocesses_every_slot_it_removed() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Batch-delete-01", GLOBAL_PARAM_TEMP_ID).await;
    let deployment = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let before = e2e::count(
        &db,
        "SELECT count(*) AS c FROM reprocessing_jobs WHERE trigger_type LIKE 'deployment%'",
    )
    .await;

    let (status, body) = delete_json_with_token(
        &app,
        "/api/sensor_deployments/batch",
        &serde_json::json!([deployment]),
        &token,
    )
    .await;
    assert!(
        status < 400,
        "the deployment batch delete is served: {status} {body}"
    );

    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) AS c FROM sensor_deployments WHERE id = '{deployment}'")
        )
        .await,
        0,
        "the deployment is gone"
    );
    assert!(
        e2e::count(
            &db,
            "SELECT count(*) AS c FROM reprocessing_jobs WHERE trigger_type LIKE 'deployment%'",
        )
        .await
            > before,
        "the delete enqueued the slot reprocess its single-row path enqueues"
    );
}

/// Scenario: two deployments are deleted in one batch.
/// Expected behaviour: the delete lifecycle runs exactly once per row, so the slot reprocess is
/// enqueued twice and not three times. A batch method that repeats what the per-row hook already
/// does shows up here as the extra job.
#[tokio::test]
#[serial]
async fn a_batch_delete_runs_the_delete_lifecycle_once_per_row() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_base_entities(&db).await;

    let sensor = create_sensor(&db, "Batch-lifecycle-01", GLOBAL_PARAM_TEMP_ID).await;
    let first = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    end_deployment(&db, first, dt("2025-05-01T00:00:00Z")).await;
    let second = deploy_sensor(&db, sensor.id, SITE1_ID, dt("2025-06-01T00:00:00Z")).await;

    let app = build_test_app(db.clone());
    let token = seed_api_token(&db, full_permissions(), None).await;

    let before = e2e::count(
        &db,
        "SELECT count(*) AS c FROM reprocessing_jobs WHERE trigger_type = 'deployment_delete'",
    )
    .await;

    let (status, body) = delete_json_with_token(
        &app,
        "/api/sensor_deployments/batch",
        &serde_json::json!([first, second]),
        &token,
    )
    .await;
    assert!(status < 400, "the batch delete is served: {status} {body}");

    let after = e2e::count(
        &db,
        "SELECT count(*) AS c FROM reprocessing_jobs WHERE trigger_type = 'deployment_delete'",
    )
    .await;
    assert_eq!(
        after - before,
        2,
        "one slot reprocess per deleted deployment, not one per row plus a batch one"
    );
}
