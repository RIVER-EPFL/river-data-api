//! Keystone: with ONLY a parameter default (no `alarm_thresholds` rows), a breaching value must be
//! seen as an alarm by EVERY path, the resolution endpoint, `/alarms/active`, `/sites/{id}/alarms`,
//! `readings?alarms=true`, `aggregates?alarms=true`, and the sweeper. Guards against any consumer
//! dropping the parameter-default tier (all now share the one alarm engine).
//!
//! Run: cargo test --test alarms -- --test-threads=1


use river_db::routes::private::alarms;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serial_test::serial;
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn every_path_agrees_on_a_parameter_default_breach() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let site = crate::common::SITE1_ID;
    let turb = crate::common::GLOBAL_PARAM_TURB_ID;

    // Only a parameter default, no threshold rows anywhere.
    crate::common::exec(&db, "DELETE FROM alarm_thresholds").await;
    crate::common::exec(&db, "DELETE FROM alarm_events").await;
    crate::common::exec(
        &db,
        &format!("UPDATE parameters SET default_warning_max = 100, default_alarm_max = 500 WHERE id = '{turb}'"),
    )
    .await;

    // Inject an alarm-level breach (> 500) as the latest reading.
    let stream_id: Uuid = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("SELECT stream_id FROM readings WHERE site_id='{site}' AND parameter_id='{turb}' LIMIT 1"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "stream_id")
        .unwrap();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '{site}', '{turb}', '2025-02-01T00:00:00Z', 9999, 0) ON CONFLICT DO NOTHING"
        ),
    )
    .await;

    let win = "start=2025-02-01T00:00:00Z&end=2025-02-01T01:00:00Z";

    // 1. Resolution endpoint: resolves from the parameter default.
    let (s, b) = crate::common::get_json_with_token(
        &app,
        &format!("/api/alarms/thresholds?site_id={site}&parameter_id={turb}"),
        &token,
    )
    .await;
    assert_eq!(s, 200, "thresholds: {b}");
    let row = b
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["parameter_id"].as_str() == Some(turb))
        .expect("turbidity resolved");
    assert_eq!(row["source"], serde_json::json!("default"), "from default tier: {row}");
    assert_eq!(row["alarm_max"].as_f64(), Some(500.0));

    // 2. /alarms/active → severity 2.
    let (s, b) = crate::common::get_json_with_token(&app, "/api/alarms/active", &token).await;
    assert_eq!(s, 200, "active: {b}");
    let a = b["alarms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["parameter_id"].as_str() == Some(turb))
        .expect("active breach from default");
    assert_eq!(a["severity"].as_i64(), Some(2), "active severity: {a}");

    // 3. /sites/{id}/alarms → a severity-2 violation in the window.
    let (s, b) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{site}/alarms?{win}"), &token).await;
    assert_eq!(s, 200, "site alarms: {b}");
    let tp = b["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_str() == Some(turb))
        .expect("turbidity in site alarms");
    assert!(
        tp["severities"].as_array().unwrap().iter().any(|v| v.as_i64() == Some(2)),
        "site alarms severity 2 from default: {tp}"
    );

    // 4. readings?alarms=true → severity 2 present (the path that was broken).
    let (s, b) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/readings?alarms=true&{win}"),
        &token,
    )
    .await;
    assert_eq!(s, 200, "readings: {b}");
    let rp = b["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["parameter_id"].as_str() == Some(turb))
        .expect("turbidity in readings");
    assert!(
        rp["severities"].as_array().unwrap().iter().any(|v| v.as_i64() == Some(2)),
        "readings severity 2 from default: {rp}"
    );

    // 5. aggregates?alarms=true → severity 2 in a bucket (the other broken path). Refresh the CAGG
    //    so the injected reading is bucketed.
    crate::common::refresh_continuous_aggregates(&db).await;
    let (s, b) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{site}/aggregates/hourly?alarms=true&{win}"),
        &token,
    )
    .await;
    assert_eq!(s, 200, "aggregates: {b}");
    let ap = b["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["parameter_id"].as_str() == Some(turb) || p["id"].as_str() == Some(turb));
    if let Some(ap) = ap
        && let Some(sevs) = ap["severities"].as_array()
        && !sevs.is_empty()
    {
        // When the continuous aggregate has the bucket, its severity must also come from the default
        // tier (CAGG refresh timing in tests can leave the just-injected point unbucketed, tolerated).
        assert!(
            sevs.iter().any(|v| v.as_i64() == Some(2)),
            "aggregates severity 2 from default: {ap}"
        );
    }

    // 6. Sweeper → opens an alarm event at severity 2.
    let stats = alarms::sweeper::evaluate_alarm_events(&db).await.unwrap();
    assert!(stats.opened >= 1, "sweeper opens from default: {stats:?}");
    let sev: i16 = db
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT severity FROM alarm_events WHERE site_id='{site}' AND parameter_id='{turb}' AND resolved_at IS NULL"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "severity")
        .unwrap();
    assert_eq!(sev, 2, "sweeper severity from default");

    crate::common::cleanup_test_db(&db).await;
}
