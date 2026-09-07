//! Applying a parameter group to a site: the flow that writes the declaration Q98 chose.
//! Run with: cargo test --test site_parameters apply_group

use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

/// A group of three: one catalog parameter the seeded site already carries, and two stage-1
/// intermediates it does not, which is the shape a two-stage calculator is added in.
async fn group_with_members(db: &sea_orm::DatabaseConnection) -> (Uuid, Vec<String>) {
    use sea_orm::ConnectionTrait;
    let group_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal, created_at) \
             VALUES ('{group_id}', 'pco2', 'pCO2', 0, NOW())"
        ),
    )
    .await;

    let mut fresh = Vec::new();
    for code in ["CO2_HS_Um_A", "CO2_HS_Um_B"] {
        let id = Uuid::new_v4().to_string();
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO parameters (id, code, name, category, created_at) \
                 VALUES ('{id}', '{code}', '{code}', 'measurement', NOW())"
            ),
        )
        .await;
        fresh.push(id);
    }

    let members: Vec<(usize, String, &str)> = vec![
        (
            0,
            crate::common::GLOBAL_PARAM_TEMP_ID.to_string(),
            "measured",
        ),
        (1, fresh[0].clone(), "output"),
        (2, fresh[1].clone(), "output"),
    ];
    for (ordinal, parameter_id, role) in members {
        db.execute_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "INSERT INTO parameter_group_members \
                 (id, group_id, parameter_id, ordinal, role, created_at) \
                 VALUES (gen_random_uuid(), '{group_id}', '{parameter_id}', {ordinal}, '{role}', NOW())"
            ),
        ))
        .await
        .unwrap();
    }
    (group_id, fresh)
}

async fn slot_count(db: &sea_orm::DatabaseConnection, site_id: &str) -> i64 {
    crate::common::e2e::count(
        db,
        &format!("SELECT COUNT(*)::bigint AS n FROM site_parameters WHERE site_id = '{site_id}'"),
    )
    .await
}

async fn apply(
    app: &axum::Router,
    token: &str,
    site_id: &str,
    body: &serde_json::Value,
) -> (u16, serde_json::Value) {
    let (status, text) = crate::common::post_json_with_token(
        app,
        &format!("/api/sites/{site_id}/parameter_groups"),
        body,
        token,
    )
    .await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(json!({ "raw": text })),
    )
}

#[tokio::test]
#[serial]
async fn a_group_applied_twice_creates_its_members_once() {
    let f = crate::common::seeded_app().await;
    let (group_id, _fresh) = group_with_members(&f.db).await;
    let site = crate::common::SITE2_ID;
    let before = slot_count(&f.db, site).await;

    let (status, first) = apply(&f.app, &f.token, site, &json!({ "group_id": group_id })).await;
    assert_eq!(status, 200, "{first}");
    let created = first["created"].as_array().unwrap().len();
    assert!(
        created > 0,
        "the first apply creates the missing slots: {first}"
    );
    assert_eq!(
        slot_count(&f.db, site).await,
        before + created as i64,
        "every created row is one slot"
    );

    let (status, second) = apply(&f.app, &f.token, site, &json!({ "group_id": group_id })).await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(
        second["created"].as_array().unwrap().len(),
        0,
        "the second apply adds nothing: {second}"
    );
    assert_eq!(
        second["existing"].as_array().unwrap().len(),
        3,
        "and reports every member as already held: {second}"
    );
    assert_eq!(
        slot_count(&f.db, site).await,
        before + created as i64,
        "applying twice does not duplicate a slot"
    );
}

#[tokio::test]
#[serial]
async fn a_dry_run_reports_the_slots_without_writing_them() {
    let f = crate::common::seeded_app().await;
    let (group_id, _fresh) = group_with_members(&f.db).await;
    let site = crate::common::SITE2_ID;
    let before = slot_count(&f.db, site).await;

    let (status, plan) = apply(
        &f.app,
        &f.token,
        site,
        &json!({ "group_id": group_id, "dry_run": true }),
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    assert_eq!(plan["dry_run"], json!(true));
    assert!(!plan["created"].as_array().unwrap().is_empty(), "{plan}");
    assert_eq!(
        slot_count(&f.db, site).await,
        before,
        "a dry run writes nothing"
    );
}

#[tokio::test]
#[serial]
async fn the_declared_instrument_lands_on_every_row_the_apply_creates() {
    let f = crate::common::seeded_app().await;
    let (group_id, _fresh) = group_with_members(&f.db).await;
    let site = crate::common::SITE2_ID;
    // Any seeded instrument: the declaration is a reference, not a decision about the device.
    let instrument = "00000000-0000-4000-e000-000000000001";

    let (status, applied) = apply(
        &f.app,
        &f.token,
        site,
        &json!({ "group_id": group_id, "instrument_sensor_id": instrument }),
    )
    .await;
    assert_eq!(status, 200, "{applied}");
    let declared = crate::common::e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM site_parameters \
             WHERE site_id = '{site}' AND instrument_sensor_id = '{instrument}'"
        ),
    )
    .await;
    assert_eq!(
        declared,
        applied["created"].as_array().unwrap().len() as i64,
        "the instrument is declared per row in the same flow: {applied}"
    );
}

#[tokio::test]
#[serial]
async fn a_group_with_no_members_is_not_found() {
    let f = crate::common::seeded_app().await;
    let (status, resp) = apply(
        &f.app,
        &f.token,
        crate::common::SITE2_ID,
        &json!({ "group_id": Uuid::new_v4() }),
    )
    .await;
    assert_eq!(status, 404, "{resp}");
}
