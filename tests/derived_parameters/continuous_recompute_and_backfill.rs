//! Scenario: a derived parameter is defined, assigned to a site, then source
//! readings arrive via the ingest path.
//!
//! Expected behaviour: each source reading triggers a derived reading at the
//! same timestamp with `value = formula(source_value)`, without any manual
//! recompute action.
//!
//! Run with: cargo test --test derived_parameters

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const POLL_DEADLINE_SECS: u64 = 30;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn poll_for_derived(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    time: DateTime<Utc>,
    max_seconds: u64,
) -> Option<f64> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_seconds);
    while std::time::Instant::now() < deadline {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
                 WHERE site_id = $1 AND parameter_id = $2 AND time = $3 \
                 LIMIT 1",
                [site_id.into(), parameter_id.into(), time.into()],
            ))
            .await
            .ok()
            .flatten();
        if let Some(r) = row
            && let Ok(v) = r.try_get::<f64>("", "value")
        {
            return Some(v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    None
}

/// The `skipped_output` finding standing on a slot, with the instants it counted.
async fn poll_for_refusal(
    db: &DatabaseConnection,
    site_id: Uuid,
    parameter_id: Uuid,
    max_seconds: u64,
) -> Option<(DateTime<Utc>, i64)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(max_seconds);
    loop {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT group_time, (computed->>'instants')::bigint AS instants                    FROM replicate_audit_holds                   WHERE kind = 'skipped_output' AND status = 'pending'                     AND site_id = $1 AND parameter_id = $2",
                [site_id.into(), parameter_id.into()],
            ))
            .await
            .ok()
            .flatten();
        if let Some(r) = row {
            return Some((
                r.try_get::<DateTime<Utc>>("", "group_time")
                    .expect("group_time"),
                r.try_get::<i64>("", "instants").expect("instants"),
            ));
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
#[serial]
async fn test_continuous_derived_recompute_after_ingest() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).unwrap();

    let derived_name = format!("dom_test_{}", Uuid::new_v4().simple());
    let calculation =
        crate::common::seed_formula_calculation(&db, &format!("{derived_name}_set")).await;
    let create_body = serde_json::json!({
        "code": derived_name,
        "name": "Test DO mg/L",
        "units": "mg/L",
        "formula": "Dissolved_O2 * 0.032",
        "description": "Continuous recompute test fixture",
        "tool_script_id": calculation,
    });
    let (status, def_json) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &create_body,
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create derived ({status}): {def_json}"
    );
    let output_parameter_id = def_json["output_parameter_id"]
        .as_str()
        .expect("derived output parameter_id must be populated by after_create hook")
        .to_string();
    let derived_param_uuid = Uuid::parse_str(&output_parameter_id).unwrap();

    let assign_body = serde_json::json!({
        "site_id": crate::common::SITE1_ID,
        "parameter_id": output_parameter_id,
        "name": derived_name,
        "sensor_type": "derived",
        "entry_mode": "tool",
        "display_units": "mg/L",
    });
    let (status, sp_text) =
        crate::common::post_json_with_token(&app, "/api/site_parameters", &assign_body, &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "assign site_parameter ({status}): {sp_text}"
    );

    let t1: DateTime<Utc> = Utc::now() - Duration::hours(36);
    let t1 = t1.with_timezone(&Utc) - Duration::nanoseconds(i64::from(t1.timestamp_subsec_nanos()));
    let raw_value_1 = 250.0_f64;
    let ingest_body_1 = serde_json::json!({
        "readings": [{
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
            "time": t1.to_rfc3339(),
            "raw_value": raw_value_1,
        }]
    });
    let (status, text) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &ingest_body_1, &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "ingest source #1 ({status}): {text}"
    );

    let expected_1 = raw_value_1 * 0.032;
    let v1 = poll_for_derived(&db, site_id, derived_param_uuid, t1, POLL_DEADLINE_SECS).await;
    assert!(
        v1.is_some(),
        "derived reading at {t1} did not appear within {POLL_DEADLINE_SECS}s of ingest"
    );
    let got_1 = v1.unwrap();
    assert!(
        (got_1 - expected_1).abs() < 1e-6,
        "first derived value: expected {expected_1}, got {got_1}"
    );

    let t2 = t1 + Duration::minutes(10);
    let raw_value_2 = 300.0_f64;
    let ingest_body_2 = serde_json::json!({
        "readings": [{
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
            "time": t2.to_rfc3339(),
            "raw_value": raw_value_2,
        }]
    });
    let (status, text) =
        crate::common::post_json_with_token(&app, "/api/readings/batch", &ingest_body_2, &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "ingest source #2 ({status}): {text}"
    );

    let expected_2 = raw_value_2 * 0.032;
    let v2 = poll_for_derived(&db, site_id, derived_param_uuid, t2, POLL_DEADLINE_SECS).await;
    assert!(
        v2.is_some(),
        "second derived reading at {t2} did not appear within {POLL_DEADLINE_SECS}s of ingest"
    );
    let got_2 = v2.unwrap();
    assert!(
        (got_2 - expected_2).abs() < 1e-6,
        "second derived value: expected {expected_2}, got {got_2}"
    );

    let v1_again = poll_for_derived(&db, site_id, derived_param_uuid, t1, 2).await;
    assert!(
        v1_again.is_some(),
        "first derived reading was lost after second ingest"
    );
}

/// Scenario: a source reading exists historically but no derived reading was
/// computed for that timestamp. Triggering the manual recompute endpoint must
/// backfill the missing derived value.
#[tokio::test]
#[serial]
async fn test_recompute_endpoint_backfills_historical_gap() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).unwrap();

    let derived_name = format!("dom_backfill_{}", Uuid::new_v4().simple());
    let calculation =
        crate::common::seed_formula_calculation(&db, &format!("{derived_name}_set")).await;
    let create_body = serde_json::json!({
        "code": derived_name,
        "name": "Backfill DO mg/L",
        "units": "mg/L",
        "formula": "Dissolved_O2 * 0.032",
        "tool_script_id": calculation,
    });
    let (status, def_json) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &create_body,
        &token,
    )
    .await;
    assert!((200..300).contains(&status));
    let derived_def_id = def_json["id"].as_str().unwrap().to_string();
    let output_parameter_id = def_json["output_parameter_id"]
        .as_str()
        .unwrap()
        .to_string();
    let derived_param_uuid = Uuid::parse_str(&output_parameter_id).unwrap();

    let seeded_source_time: DateTime<Utc> = "2025-01-15T00:00:00Z".parse().unwrap();
    let pre = poll_for_derived(&db, site_id, derived_param_uuid, seeded_source_time, 1).await;
    assert!(
        pre.is_none(),
        "expected no derived reading before assignment + recompute"
    );

    let assign_body = serde_json::json!({
        "site_id": crate::common::SITE1_ID,
        "parameter_id": output_parameter_id,
        "name": derived_name,
        "sensor_type": "derived",
        "entry_mode": "tool",
        "display_units": "mg/L",
    });
    let (status, _) =
        crate::common::post_json_with_token(&app, "/api/site_parameters", &assign_body, &token)
            .await;
    assert!((200..300).contains(&status));

    let uri = format!("/api/actions/derived_parameters/{derived_def_id}/recompute");
    let (status, _) =
        crate::common::post_json_with_token(&app, &uri, &serde_json::json!({}), &token).await;
    assert!(
        (200..300).contains(&status),
        "recompute endpoint should accept request"
    );

    let v = poll_for_derived(
        &db,
        site_id,
        derived_param_uuid,
        seeded_source_time,
        POLL_DEADLINE_SECS,
    )
    .await;
    assert!(
        v.is_some(),
        "recompute should have backfilled a derived reading at {seeded_source_time}"
    );
}

/// Scenario: a calculation's formula names a constant beside its source parameter, the shape the
/// portal's "constant when the visit holds none" fallback takes.
///
/// Expected behaviour: the constant is bound from the constants table and the derived reading
/// appears, rather than the evaluation failing on an unknown variable and writing nothing.
#[tokio::test]
#[serial]
async fn test_continuous_derived_binds_a_constant_by_name() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).unwrap();

    crate::common::exec(
        &db,
        "INSERT INTO constants (id, name, value, units, description) \
         VALUES (gen_random_uuid(), 'do_scale_factor', 0.032, 'mg/L per uM', \
         'Continuous derived constant fixture') \
         ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value",
    )
    .await;

    let derived_name = format!("do_scaled_{}", Uuid::new_v4().simple());
    let calculation =
        crate::common::seed_formula_calculation(&db, &format!("{derived_name}_set")).await;
    let (status, def_json) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": derived_name,
            "name": "Test DO scaled by constant",
            "units": "mg/L",
            "formula": "coalesce(Dissolved_O2, do_scale_factor) * do_scale_factor",
            "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create derived ({status}): {def_json}"
    );
    let output_parameter_id = def_json["output_parameter_id"]
        .as_str()
        .unwrap()
        .to_string();
    let derived_param_uuid = Uuid::parse_str(&output_parameter_id).unwrap();

    let (status, sp_text) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": output_parameter_id,
            "name": derived_name,
            "sensor_type": "derived",
            "entry_mode": "tool",
            "display_units": "mg/L",
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign site_parameter ({status}): {sp_text}"
    );

    let t: DateTime<Utc> = Utc::now() - Duration::hours(30);
    let t = t - Duration::nanoseconds(i64::from(t.timestamp_subsec_nanos()));
    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": t.to_rfc3339(),
                "raw_value": 250.0,
            }]
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "ingest source ({status}): {text}"
    );

    // 250.0 * 0.032
    let got = poll_for_derived(&db, site_id, derived_param_uuid, t, POLL_DEADLINE_SECS)
        .await
        .expect("derived reading naming a constant did not appear");
    assert!((got - 8.0).abs() < 1e-6, "expected 8.0, got {got}");
}

/// Scenario: a continuous derived slot holds a value, then its input is corrected so the formula
/// evaluates to NA at that instant.
///
/// Expected behaviour: the stored number stops being served, rather than standing as a value the
/// formula no longer produces (Q172). A divide by zero is the other arm: it refuses, and the value
/// that stands is left alone.
#[tokio::test]
#[serial]
async fn na_clears_the_value_the_formula_no_longer_produces_and_a_divide_by_zero_does_not() {
    let (db, app, token) = setup().await;
    let site_id = Uuid::parse_str(crate::common::SITE1_ID).unwrap();

    let assign = |code: &str, formula: &str| {
        let code = code.to_string();
        let formula = formula.to_string();
        let app = app.clone();
        let token = token.clone();
        let db = db.clone();
        async move {
            let calculation =
                crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
            let (status, def) = crate::common::post_json_parse_with_token(
                &app,
                "/api/derived_parameters",
                &serde_json::json!({
                    "code": code,
                    "name": code,
                    "units": "1/mg",
                    "formula": formula,
                    "description": "Non-finite arms fixture",
                    "tool_script_id": calculation,
                }),
                &token,
            )
            .await;
            assert!(
                (200..300).contains(&status),
                "create derived ({status}): {def}"
            );
            let parameter_id = def["output_parameter_id"]
                .as_str()
                .expect("derived output parameter_id")
                .to_string();
            let (status, text) = crate::common::post_json_with_token(
                &app,
                "/api/site_parameters",
                &serde_json::json!({
                    "site_id": crate::common::SITE1_ID,
                    "parameter_id": parameter_id,
                    "name": code,
                    "sensor_type": "derived",
                    "entry_mode": "tool",
                    "display_units": "1/mg",
                }),
                &token,
            )
            .await;
            assert!((200..300).contains(&status), "assign ({status}): {text}");
            Uuid::parse_str(&parameter_id).unwrap()
        }
    };

    let suffix = Uuid::new_v4().simple().to_string();
    // At DO = 300 both are finite; at DO = 250 the first is 0/0 and the second is 1/0.
    let na_param = assign(
        &format!("na_arm_{suffix}"),
        "(Dissolved_O2 - 250) / (Dissolved_O2 - 250)",
    )
    .await;
    let inf_param = assign(&format!("inf_arm_{suffix}"), "1 / (Dissolved_O2 - 250)").await;

    let t: DateTime<Utc> = Utc::now() - Duration::hours(36);
    let t = t - Duration::nanoseconds(i64::from(t.timestamp_subsec_nanos()));
    let ingest = |value: f64, conflict: Option<&str>| {
        let mut body = serde_json::json!({
            "readings": [{
                "site_id": crate::common::SITE1_ID,
                "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                "time": t.to_rfc3339(),
                "raw_value": value,
            }]
        });
        if let Some(c) = conflict {
            body["conflict"] = serde_json::json!(c);
        }
        body
    };

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &ingest(300.0, None),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {text}");
    assert_eq!(
        poll_for_derived(&db, site_id, na_param, t, POLL_DEADLINE_SECS).await,
        Some(1.0),
        "50 / 50"
    );
    assert_eq!(
        poll_for_derived(&db, site_id, inf_param, t, POLL_DEADLINE_SECS).await,
        Some(0.02),
        "1 / 50"
    );

    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &ingest(250.0, Some("overwrite")),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "correction ({status}): {text}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_DEADLINE_SECS);
    let mut na_served = Some(1.0);
    while std::time::Instant::now() < deadline {
        na_served = poll_for_derived(&db, site_id, na_param, t, 1).await;
        if na_served.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(
        na_served, None,
        "0 / 0 is NA, so the slot no longer serves 1.0"
    );
    assert_eq!(
        poll_for_derived(&db, site_id, inf_param, t, 2).await,
        Some(0.02),
        "1 / 0 refuses, so the value that stands is left alone"
    );

    // The value stands, so the finding is the only thing that says the formula stopped computing.
    assert_eq!(
        poll_for_refusal(&db, site_id, inf_param, POLL_DEADLINE_SECS).await,
        Some((t, 1)),
        "the refusal is reported once for the slot, naming the instant it began at"
    );
    assert_eq!(
        poll_for_refusal(&db, site_id, na_param, 1).await,
        None,
        "an NA cleared its value and is not a refusal"
    );

    // The divisor is corrected: the slot computes again, so the finding is the run's to close
    // rather than a person's.
    let (status, text) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &ingest(200.0, Some("overwrite")),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "repair ({status}): {text}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_DEADLINE_SECS);
    let mut served = poll_for_derived(&db, site_id, inf_param, t, 1).await;
    let mut standing = poll_for_refusal(&db, site_id, inf_param, 1).await;
    while (served != Some(-0.02) || standing.is_some()) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        served = poll_for_derived(&db, site_id, inf_param, t, 1).await;
        standing = poll_for_refusal(&db, site_id, inf_param, 1).await;
    }
    assert_eq!(served, Some(-0.02), "1 / -50");
    assert_eq!(standing, None, "the repaired slot's finding is closed");
}
