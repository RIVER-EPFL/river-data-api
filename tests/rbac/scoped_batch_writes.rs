//! Scenario: a project-scoped API token writes several rows of its own project through a CRUD
//! `/batch` route.
//!
//! Expected behaviour: the batch is judged by the rows it names, exactly as the single-row routes
//! are. Every element inside the token's project is admitted; one element outside it refuses the
//! whole request. `batch` is a sub-route, so reading it as a row id refuses every scoped batch
//! write whatever the body says, which is what these pin.

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{PROJECT_ID, SITE1_ID};

/// A second project with a site of its own, so a batch can name one row outside the token's scope.
const OTHER_PROJECT: &str = "00000000-0000-4000-a000-0000000009e1";
const OTHER_SITE: &str = "00000000-0000-4000-a000-0000000009e2";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) VALUES \
             ('{OTHER_PROJECT}', 'Batch Other', 'scoped batch suite', 'test')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude) VALUES \
             ('{OTHER_SITE}', '{OTHER_PROJECT}', 'Other Station', 46.0, 7.0)"
        ),
    )
    .await;

    let token = crate::common::seed_api_token(
        &db,
        json!({
            "read_metadata": true,
            "read_data": true,
            "write_metadata": true,
            "write_data": true,
        }),
        Some(PROJECT_ID),
    )
    .await;
    let app = crate::common::build_test_app(db.clone());
    (db, app, token)
}

/// Two catalog parameters the batch can attach, so neither element collides with a seeded slot.
async fn two_parameters(db: &sea_orm::DatabaseConnection) -> (Uuid, Uuid) {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) VALUES \
             ('{a}', 'RdBatchOne', 'Batch One', 'degC', 'measurement'), \
             ('{b}', 'RdBatchTwo', 'Batch Two', 'uS/cm', 'measurement')"
        ),
    )
    .await;
    (a, b)
}

#[tokio::test]
#[serial]
async fn a_scoped_token_batch_creates_rows_of_its_own_project() {
    let (db, app, token) = setup().await;
    let (one, two) = two_parameters(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters/batch",
        &json!([
            { "site_id": SITE1_ID, "parameter_id": one },
            { "site_id": SITE1_ID, "parameter_id": two },
        ]),
        &token,
    )
    .await;
    assert_eq!(
        status, 201,
        "the batch names only rows of the token's project: {body}"
    );

    let landed = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM site_parameters \
             WHERE parameter_id IN ('{one}', '{two}')"
        ),
    )
    .await;
    assert_eq!(landed, 2, "both rows landed");

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_batch_naming_one_row_outside_the_scope_is_refused_whole() {
    let (db, app, token) = setup().await;
    let (one, two) = two_parameters(&db).await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters/batch",
        &json!([
            { "site_id": SITE1_ID, "parameter_id": one },
            { "site_id": OTHER_SITE, "parameter_id": two },
        ]),
        &token,
    )
    .await;
    assert_eq!(
        status, 403,
        "one element outside the token's project refuses the batch: {body}"
    );

    let landed = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT count(*) AS n FROM site_parameters \
             WHERE parameter_id IN ('{one}', '{two}')"
        ),
    )
    .await;
    assert_eq!(landed, 0, "a refused batch writes nothing");

    crate::common::cleanup_test_db(&db).await;
}
