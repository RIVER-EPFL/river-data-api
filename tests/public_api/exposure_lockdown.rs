//! The unauthenticated public tier must expose ONLY public-flagged projects, sites, and
//! parameters, never private data, never via coercion, and never a write.

use serial_test::serial;

use crate::common::fixtures::{PARAM_S1_DO_ID, PARAM_S1_TEMP_ID, PROJECT_ID, SITE1_ID};

const PUBLIC_CODE: &str = "testpub";

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    // Make the project public with one public site (Upstream) and one public parameter (Temp).
    // Downstream (SITE2) stays private; the DO parameter at Upstream stays private.
    for sql in [
        format!(
            "UPDATE projects SET is_public = true, public_code = '{PUBLIC_CODE}' WHERE id = '{PROJECT_ID}'"
        ),
        format!("UPDATE sites SET public_code = 'upstream' WHERE id = '{SITE1_ID}'"),
        format!("UPDATE site_parameters SET is_public = true WHERE id = '{PARAM_S1_TEMP_ID}'"),
        format!("UPDATE site_parameters SET is_public = false WHERE id = '{PARAM_S1_DO_ID}'"),
    ] {
        crate::common::db::exec(&db, &sql).await;
    }

    let app = crate::common::build_test_app(db.clone());
    (db, app)
}

#[tokio::test]
#[serial]
async fn public_tier_exposes_only_public_flagged_data() {
    let (_db, app) = setup().await;

    // Discovery lists the public project.
    let (s, body) = crate::common::get_json(&app, "/api/public").await;
    assert_eq!(s, 200, "discovery should be reachable");
    let codes: Vec<String> = body
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| e["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        codes.contains(&PUBLIC_CODE.to_string()),
        "public project must be discoverable: {body}"
    );

    // Sites: the public site is listed, the private one is not.
    let (s, body) =
        crate::common::get_json(&app, &format!("/api/public/{PUBLIC_CODE}/sites")).await;
    assert_eq!(s, 200);
    let body_str = body.to_string();
    assert!(
        body_str.contains("upstream"),
        "public site must be listed: {body}"
    );
    assert!(
        !body_str.to_lowercase().contains("downstream"),
        "private site must NOT be listed: {body}"
    );

    // The public site resolves; the private site 404s even by its real name.
    let (s, _) =
        crate::common::get(&app, &format!("/api/public/{PUBLIC_CODE}/sites/upstream")).await;
    assert_eq!(s, 200, "public site is reachable");
    let (s, _) =
        crate::common::get(&app, &format!("/api/public/{PUBLIC_CODE}/sites/downstream")).await;
    assert_eq!(s, 404, "private site must be 404 on the public tier");

    // Parameters: only the public parameter is exposed.
    let (s, body) = crate::common::get_json(
        &app,
        &format!("/api/public/{PUBLIC_CODE}/sites/upstream/parameters"),
    )
    .await;
    assert_eq!(s, 200);
    let pbody = body.to_string();
    assert!(
        pbody.contains("DO_Temperature"),
        "public parameter must be exposed: {body}"
    );
    assert!(
        !pbody.contains("Dissolved_O2"),
        "private parameter must NOT be exposed: {body}"
    );
}

#[tokio::test]
#[serial]
async fn private_project_and_coercion_are_blocked() {
    let (db, app) = setup().await;

    // A private project (one with no public_code) is unreachable through the public tier.
    crate::common::db::exec(
        &db,
        "INSERT INTO projects (id, name, description) VALUES ('00000000-0000-4000-e000-000000000001', 'Secret', 'private')",
    )
    .await;

    // Unknown / non-public codes 404, no leakage of existence.
    for code in ["nonexistent", "Secret"] {
        let (s, _) = crate::common::get(&app, &format!("/api/public/{code}/sites")).await;
        assert_eq!(
            s, 404,
            "non-public code '{code}' must 404 on the public tier, got {s}"
        );
    }

    // The internal UUID can't be used to coerce a private site out of a public project.
    let (s, _) =
        crate::common::get(&app, &format!("/api/public/{PUBLIC_CODE}/sites/{SITE1_ID}")).await;
    assert_eq!(
        s, 404,
        "a site must be addressed by its public code, not its internal UUID"
    );
}

#[tokio::test]
#[serial]
async fn public_tier_refuses_writes() {
    let (_db, app) = setup().await;

    // No write verb is mounted on the public tier; mutations are rejected (405/404), never applied.
    let body = serde_json::json!({ "name": "x" });
    let (s, _) =
        crate::common::post_json(&app, &format!("/api/public/{PUBLIC_CODE}/sites"), &body).await;
    assert!(
        s == 405 || s == 404,
        "POST to a public route must be rejected, got {s}"
    );
    let (s, _) = crate::common::post_json(&app, "/api/public", &body).await;
    assert!(
        s == 405 || s == 404,
        "POST to public discovery must be rejected, got {s}"
    );
}
