//! `POST /sync/bulk-pair` resolves the request's parameters against the catalog the same way the
//! pairing plan does: code, then display name, then alias, all case-insensitive, and only a
//! genuine miss creates a row.

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

const SOURCE: &str = "bulksrc";

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (app, token, db)
}

async fn seed_parameter(
    db: &sea_orm::DatabaseConnection,
    code: &str,
    name: &str,
    aliases: &[&str],
) -> Uuid {
    let id = Uuid::new_v4();
    let aliases = aliases
        .iter()
        .map(|a| format!("'{a}'"))
        .collect::<Vec<_>>()
        .join(",");
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category, aliases) \
             VALUES ('{id}', '{code}', '{name}', 'ppb', 'measurement', ARRAY[{aliases}]::text[])"
        ),
    )
    .await;
    id
}

async fn seed_stream(db: &sea_orm::DatabaseConnection, source_key: &str, parameter: &str) -> Uuid {
    let id = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        db,
        &id.to_string(),
        SOURCE,
        source_key,
        "Test River Project",
        "Upstream Station",
        parameter,
        "ppb",
        None,
        0,
    )
    .await;
    id
}

async fn paired_parameter(db: &sea_orm::DatabaseConnection, stream_id: Uuid) -> Option<Uuid> {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT sp.parameter_id AS v FROM data_streams ds \
         JOIN site_parameters sp ON sp.id = ds.site_parameter_id WHERE ds.id = $1",
        [stream_id.into()],
    ))
    .await
    .expect("query")
    .map(|r| r.try_get::<Uuid>("", "v").expect("parameter_id"))
}

async fn parameter_count(db: &sea_orm::DatabaseConnection) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT count(*) AS v FROM parameters".to_owned(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<i64>("", "v")
    .expect("count")
}

async fn bulk_pair(
    app: &axum::Router,
    token: &str,
    parameters: serde_json::Value,
) -> serde_json::Value {
    let (status, resp) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/bulk-pair",
        &serde_json::json!({
            "source_system": SOURCE,
            "project_name": "Test River Project",
            "sites": [{ "name": "Upstream Station", "existing_id": crate::common::SITE1_ID }],
            "parameters": parameters,
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "bulk-pair ({status}): {resp}");
    resp
}

/// Scenario: the catalog holds `DOC` named "Dissolved organic carbon" with alias "DOC_ppb", and
/// the request names the parameter by its display name and by its alias, each without an
/// `existing_id`.
///
/// Expected behaviour: both resolve to the seeded row, nothing is created, and the streams
/// naming either form pair onto it.
#[tokio::test]
#[serial]
async fn a_parameter_named_by_display_name_or_alias_resolves_to_the_catalog_row() {
    let (app, token, db) = setup().await;
    let doc = seed_parameter(&db, "DOC", "Dissolved organic carbon", &["DOC_ppb"]).await;
    let by_name = seed_stream(&db, "sta:by-name", "Dissolved organic carbon").await;
    let by_alias = seed_stream(&db, "sta:by-alias", "doc_ppb").await;
    let before = parameter_count(&db).await;

    let resp = bulk_pair(
        &app,
        &token,
        serde_json::json!([
            { "code": "Dissolved organic carbon", "name": "Dissolved organic carbon", "units": "ppb", "existing_id": null },
            { "code": "doc_ppb", "name": "doc_ppb", "units": "ppb", "existing_id": null }
        ]),
    )
    .await;

    assert_eq!(resp["parameters_created"], 0, "{resp}");
    assert_eq!(parameter_count(&db).await, before, "no sibling row minted");
    assert_eq!(paired_parameter(&db, by_name).await, Some(doc));
    assert_eq!(paired_parameter(&db, by_alias).await, Some(doc));
    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the request carries two parameters, and the second one's display name is the first
/// one's code.
///
/// Expected behaviour: a stream naming that code pairs onto the first parameter. Code outranks
/// display name, the order the plan's lookup uses, so a later entry cannot shadow an earlier
/// one's code.
#[tokio::test]
#[serial]
async fn a_request_parameter_s_name_cannot_shadow_another_s_code() {
    let (app, token, db) = setup().await;
    let by_code = seed_stream(&db, "sta:shadow", "Turb").await;

    let resp = bulk_pair(
        &app,
        &token,
        serde_json::json!([
            { "code": "Turb", "name": "Turbidity raw", "units": "ppb", "existing_id": null },
            { "code": "TurbX", "name": "Turb", "units": "ppb", "existing_id": null }
        ]),
    )
    .await;
    assert_eq!(resp["parameters_created"], 2, "{resp}");

    let paired = paired_parameter(&db, by_code).await.expect("stream paired");
    let code = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT code AS v FROM parameters WHERE id = $1",
            [paired.into()],
        ))
        .await
        .expect("query")
        .expect("row")
        .try_get::<String>("", "v")
        .expect("code");
    assert_eq!(code, "Turb", "the stream's name is a code first");
    crate::common::cleanup_test_db(&db).await;
}
