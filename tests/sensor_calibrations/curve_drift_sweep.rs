//! The janitor's curve-drift sweep: a corrected reading must serve what its own curves produce.
//!
//! Scenario: coefficients move by a route that runs no hook (a bulk edit, a direct statement, an
//! enqueue that never landed), so stored values are left computed from the old ones.
//! Expected behaviour: the sweep recomposes them from the curves each row names, both the windowed
//! calibration and the standard curve, and reports the span it moved so the rollups can follow.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use river_db::routes::private::sensor_calibrations::service::sweep_curve_drift;

use crate::common::sensor_lifecycle::{add_calibration, create_sensor, deploy_sensor, dt};
use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const GRAB_TIME: &str = "2025-06-15T10:00:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn stored(db: &DatabaseConnection) -> (f64, Option<f64>) {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT raw_value, calibrated_value FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                 AND time = '{GRAB_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the grab is stored");
    (
        row.try_get("", "raw_value").unwrap(),
        row.try_get("", "calibrated_value").unwrap(),
    )
}

/// Deploy a lab instrument carrying a windowed calibration, so a grab against it resolves a base
/// curve and may also name a standard curve.
async fn deployed_lab_sensor(db: &DatabaseConnection, slope: f64, intercept: f64) -> (Uuid, Uuid) {
    let sensor = create_sensor(db, "Drift-probe-01", GLOBAL_PARAM_DO_ID).await;
    crate::common::exec(
        db,
        &format!(
            "UPDATE sensors SET is_lab_instrument = true WHERE id = '{}'",
            sensor.id
        ),
    )
    .await;
    let calibration =
        add_calibration(db, sensor.id, slope, intercept, dt("2025-01-01T00:00:00Z")).await;
    deploy_sensor(db, sensor.id, SITE1_ID, dt("2025-01-01T00:00:00Z")).await;
    (sensor.id, calibration)
}

async fn post_grab(
    app: &axum::Router,
    token: &str,
    sensor_id: Uuid,
    curve: Option<Uuid>,
    value: f64,
) {
    let mut reading = serde_json::json!({
        "parameter_id": GLOBAL_PARAM_DO_ID,
        "sensor_id": sensor_id,
        "value": value,
        "time": GRAB_TIME,
    });
    if let Some(curve) = curve {
        reading["standard_curve_id"] = serde_json::json!(curve);
    }
    let (status, body) = crate::common::post_checked_grab(
        app,
        &serde_json::json!({ "site_id": SITE1_ID, "readings": [reading] }),
        token,
    )
    .await;
    assert_eq!(status, 200, "grab entry: {body}");
}

#[tokio::test]
#[serial]
async fn the_sweep_recomposes_a_value_left_behind_by_an_unhooked_coefficient_edit() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    post_grab(&app, &token, sensor, None, 10.0).await;
    assert_eq!(stored(&db).await, (10.0, Some(21.0)), "2 * 10 + 1");

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;
    assert_eq!(
        stored(&db).await.1,
        Some(21.0),
        "the statement moved the curve and nothing recomputed the reading"
    );

    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(drift.moved, 1, "the one drifted reading is recomposed");
    assert_eq!(stored(&db).await, (10.0, Some(51.0)), "5 * 10 + 1");
    assert!(
        drift.span.is_some(),
        "the sweep reports the span it moved, so the rollups can follow"
    );

    let second = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(second.moved, 0, "a settled row is not rewritten again");
}

/// A grab may carry both curves, and the value is the standard curve applied on top of the base.
/// The sweep has to compose them in that order, not pick one.
#[tokio::test]
#[serial]
async fn the_sweep_composes_the_standard_curve_over_the_windowed_calibration() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    let curve = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;

    post_grab(&app, &token, sensor, Some(curve), 10.0).await;
    assert_eq!(
        stored(&db).await,
        (10.0, Some(63.5)),
        "3 * (2 * 10 + 1) + 0.5"
    );

    crate::common::exec(
        &db,
        &format!("UPDATE standard_curves SET slope = 4.0 WHERE id = '{curve}'"),
    )
    .await;
    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(
        drift.moved, 1,
        "the lab curve moved, so the grab follows it"
    );
    assert_eq!(
        stored(&db).await,
        (10.0, Some(84.5)),
        "4 * (2 * 10 + 1) + 0.5: the base is still applied underneath"
    );

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;
    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(drift.moved, 1, "and it follows the base curve too");
    assert_eq!(
        stored(&db).await,
        (10.0, Some(204.5)),
        "4 * (5 * 10 + 1) + 0.5"
    );
}

/// Broken data, repaired: the stored number is overwritten with one no curve produces, and the
/// sweep puts back exactly what the row's curves give.
#[tokio::test]
#[serial]
async fn the_sweep_repairs_a_value_corrupted_in_place() {
    let (db, app, token) = setup().await;
    let (sensor, _) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    post_grab(&app, &token, sensor, None, 10.0).await;
    assert_eq!(stored(&db).await, (10.0, Some(21.0)), "2 * 10 + 1");

    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET calibrated_value = 12345.0 \
             WHERE time = '{GRAB_TIME}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await;
    assert_eq!(stored(&db).await.1, Some(12345.0), "the row is now wrong");

    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(drift.moved, 1);
    assert_eq!(
        stored(&db).await,
        (10.0, Some(21.0)),
        "the curve on the row decides the value, whatever was written over it"
    );
}

/// Clean data, untouched: every stored value already agrees with its curves, so the sweep reports
/// nothing moved and rewrites no row, including the uncorrected and the both-curves cases.
#[tokio::test]
#[serial]
async fn the_sweep_moves_nothing_on_clean_data() {
    let (db, app, token) = setup().await;
    let (sensor, _) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    let curve = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 3.0, 0.5, 'Plate A')"
        ),
    )
    .await;
    post_grab(&app, &token, sensor, Some(curve), 10.0).await;

    let before = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM readings \
             WHERE site_id = '{SITE1_ID}' AND calibrated_value IS NOT NULL"
        ),
    )
    .await;
    let value_before = stored(&db).await;

    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(
        drift.moved, 0,
        "nothing had drifted, so nothing is rewritten"
    );
    assert!(drift.span.is_none(), "and there is no span to refresh");
    assert_eq!(
        stored(&db).await,
        value_before,
        "the value is byte-identical afterwards"
    );
    assert_eq!(
        crate::common::e2e::count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND calibrated_value IS NOT NULL"
            )
        )
        .await,
        before,
        "and no row gained or lost a correction"
    );
}

/// A corrected value no curve on the row accounts for was produced by a method this code cannot
/// recover, so the sweep reports nothing and leaves it exactly as it is.
#[tokio::test]
#[serial]
async fn the_sweep_leaves_a_correction_no_curve_accounts_for() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    post_grab(&app, &token, sensor, None, 10.0).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET calibration_id = NULL, calibrated_value = 99.0 \
             WHERE time = '{GRAB_TIME}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;

    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(
        drift.moved, 0,
        "it names no curve, so there is nothing to recompose from"
    );
    assert_eq!(
        stored(&db).await.1,
        Some(99.0),
        "and the number is untouched"
    );
}

/// The sweep also runs inside the tracked janitor job, and a recomposed replicate set carries its
/// sample statistics with it: `samples.mean` is served as the grab's value, so a repaired reading
/// with stale statistics would still serve the old number.
#[tokio::test]
#[serial]
async fn the_janitor_job_runs_the_sweep_and_the_sample_stats_follow() {
    use river_db::routes::private::reprocessing_jobs::service as jobs;

    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;

    let readings: Vec<serde_json::Value> = [10.0, 20.0]
        .iter()
        .map(|v| {
            serde_json::json!({
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "sensor_id": sensor,
                "value": v,
                "time": GRAB_TIME,
            })
        })
        .collect();
    let (status, body) = crate::common::post_checked_grab(
        &app,
        &serde_json::json!({ "site_id": SITE1_ID, "readings": readings }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "replicated grab entry: {body}");

    let sample_stats = |db: &DatabaseConnection| {
        let db = db.clone();
        async move {
            let row = db
                .query_one_raw(Statement::from_string(
                    sea_orm::DatabaseBackend::Postgres,
                    format!(
                        "SELECT mean, stdev, n FROM samples \
                         WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
                    ),
                ))
                .await
                .unwrap()
                .expect("the grab has a samples row");
            (
                row.try_get::<Option<f64>>("", "mean").unwrap().unwrap(),
                row.try_get::<Option<f64>>("", "stdev").unwrap().unwrap(),
                row.try_get::<i32>("", "n").unwrap(),
            )
        }
    };

    let (mean, _, n) = sample_stats(&db).await;
    assert!((mean - 31.0).abs() < 1e-9, "(21 + 41) / 2: {mean}");
    assert_eq!(n, 2);

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;

    let mut registry = jobs::build_registry();
    jobs::register_scheduled_services(&mut registry, &crate::common::cached_test_config());
    let ev: river_db::common::EventSender = tokio::sync::broadcast::channel(64).0;
    let wid = jobs::worker_id();
    let id = jobs::enqueue(
        &db,
        "janitor_service",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap()
    .expect("enqueue inserts a row");
    jobs::drain(&db, &ev, &registry, &wid).await.unwrap();

    let status_row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status, detail FROM reprocessing_jobs WHERE id = '{id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status_row.try_get::<String>("", "status").unwrap(),
        "completed",
        "the janitor job completes"
    );
    // One report carries every step of the tick: the gap fill's counts stand beside the sweep's.
    let detail: serde_json::Value = status_row.try_get("", "detail").unwrap();
    for key in [
        "gaps_found",
        "filled",
        "refused_slots",
        "recomposed",
        "pruned",
    ] {
        assert!(
            detail["counts"].get(key).is_some(),
            "the janitor reports {key}: {detail}"
        );
    }
    assert_eq!(detail["counts"]["recomposed"], 2, "{detail}");

    let recomposed = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT calibrated_value FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                   AND measurement_type = 'spot' \
                 ORDER BY replicate_index"
            ),
        ))
        .await
        .unwrap();
    let values: Vec<f64> = recomposed
        .iter()
        .map(|r| {
            r.try_get::<Option<f64>>("", "calibrated_value")
                .unwrap()
                .unwrap()
        })
        .collect();
    assert!(
        (values[0] - 51.0).abs() < 1e-9 && (values[1] - 101.0).abs() < 1e-9,
        "5 * raw + 1: {values:?}"
    );

    let (mean, stdev, n) = sample_stats(&db).await;
    assert!((mean - 76.0).abs() < 1e-9, "(51 + 101) / 2: {mean}");
    assert!(
        (stdev - 50.0 / std::f64::consts::SQRT_2).abs() < 1e-9,
        "stddev_samp of two points 50 apart: {stdev}"
    );
    assert_eq!(n, 2);
}

/// Scenario: the sweep rewrites a spot value that a calculation at that visit reads as an input
/// (Q108: everything rewrites, nothing is left stale).
///
/// Expected behaviour: the sweep reports the visits and parameters it moved, so the caller can
/// recompute exactly them rather than sweeping the whole system or leaving them stale.
#[tokio::test]
#[serial]
async fn the_sweep_reports_the_visits_whose_inputs_it_moved() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;
    post_grab(&app, &token, sensor, None, 10.0).await;

    let event_id: Option<Uuid> = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT collection_event_id FROM readings \
                 WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                   AND time = '{GRAB_TIME}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "collection_event_id")
        .unwrap();
    let event_id = event_id.expect("a grab lands on a visit");

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;

    let drift = sweep_curve_drift(&db, None).await.expect("sweep runs");
    assert_eq!(drift.moved, 1);
    assert_eq!(
        drift.touched,
        vec![(event_id, GLOBAL_PARAM_DO_ID.parse::<Uuid>().unwrap())],
        "the visit and parameter whose input moved, so its calculations can be re-run"
    );

    let events =
        river_db::routes::private::collection_events::flows::events_from_pairs(&db, &drift.touched)
            .await
            .expect("the visits resolve");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, event_id);
    assert_eq!(events[0].source, "manual");
}

/// An enabled calculation whose only event input is the dissolved oxygen the grabs here enter.
async fn install_calculation(db: &DatabaseConnection) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = 'reprocessrecompute'",
        "DELETE FROM tool_script_versions v USING tool_scripts s \
          WHERE v.tool_script_id = s.id AND s.name = 'reprocessrecompute'",
        "DELETE FROM tool_scripts WHERE name = 'reprocessrecompute'",
    ] {
        crate::common::exec(db, sql).await;
    }
    let manifest = serde_json::json!({
        "label": "Reprocess recompute",
        "params": [{ "name": "o", "label": "O", "kind": "number", "required": true }],
        "event_inputs": [{ "param": "o", "parameter_code": "Dissolved_O2" }],
        "outputs": [{ "key": "out", "label": "Out", "suggested_parameter_code": "ReprocessRecomputeOut" }],
    });
    for statement in [
        Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "INSERT INTO tool_scripts (name, label, created_by) \
             VALUES ('reprocessrecompute', 'Reprocess recompute', 'test')"
                .to_string(),
        ),
        Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            r"INSERT INTO tool_script_versions
                  (tool_script_id, version_no, script, entry_function, manifest, test_cases,
                   content_hash, created_by, validated_at)
              SELECT s.id, 1, $1, 'tool', $2::jsonb, '{}'::jsonb, md5($1), 'test', now()
              FROM tool_scripts s WHERE s.name = 'reprocessrecompute'",
            [
                "tool <- function(inputs, constants, curves) list(out = 1)".into(),
                manifest.to_string().into(),
            ],
        ),
        Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"UPDATE tool_scripts s SET active_version_id = v.id
              FROM tool_script_versions v
              WHERE v.tool_script_id = s.id AND s.name = 'reprocessrecompute'"
                .to_string(),
        ),
    ] {
        db.execute_raw(statement)
            .await
            .expect("calculation installed");
    }
}

/// Scenario: a reprocess moves a grab's corrected value onto its curve's current coefficients,
/// and a calculation at the grab's visit reads that value (Q108).
///
/// Expected behaviour: the reprocess queues the visit's recompute, as the drift sweep does, rather
/// than leaving the calculation on the old number with nothing left for the sweep to find.
#[tokio::test]
#[serial]
async fn a_reprocess_that_moves_a_grab_recomputes_the_visit_that_reads_it() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;
    post_grab(&app, &token, sensor, None, 10.0).await;
    install_calculation(&db).await;

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;
    river_db::routes::private::sensor_calibrations::service::reprocess_sensor_readings(
        &db, sensor, None, None,
    )
    .await
    .expect("reprocess runs");
    assert_eq!(stored(&db).await, (10.0, Some(51.0)), "5 * 10 + 1");

    let queued = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reprocessing_jobs j \
               JOIN readings r ON r.collection_event_id = j.trigger_id \
              WHERE j.trigger_type = 'event_recompute' \
                AND r.site_id = '{SITE1_ID}' AND r.parameter_id = '{GLOBAL_PARAM_DO_ID}' \
                AND r.time = '{GRAB_TIME}'"
        ),
    )
    .await;
    assert_eq!(queued, 1, "the visit whose input moved is recomputed");
}

/// Scenario: the sweep changes a stored value, and the ledger is the one place a value's history
/// is read from (Q118).
/// Expected behaviour: the move is recorded as a `curve_recompose` decision naming both numbers,
/// the janitor origin and the run that made it, and a repeat sweep records nothing.
#[tokio::test]
#[serial]
async fn a_recomposed_value_records_the_move_against_the_run_that_made_it() {
    let (db, app, token) = setup().await;
    let (sensor, calibration) = deployed_lab_sensor(&db, 2.0, 1.0).await;
    post_grab(&app, &token, sensor, None, 10.0).await;

    crate::common::exec(
        &db,
        &format!("UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{calibration}'"),
    )
    .await;

    let job = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO reprocessing_jobs (id, trigger_type, status) \
             VALUES ('{job}', 'maintenance', 'running')"
        ),
    )
    .await;

    let drift = sweep_curve_drift(&db, Some(job)).await.expect("sweep runs");
    assert_eq!(drift.moved, 1);

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT d.old ->> 'calibrated_value' AS was, d.new ->> 'calibrated_value' AS became, \
                        d.actor, d.origin, d.job_id, d.supersedes \
                   FROM reading_decisions d JOIN readings r \
                     ON r.stream_id = d.stream_id AND r.time = d.time \
                    AND r.replicate_index = d.replicate_index \
                  WHERE d.kind = 'curve_recompose' AND r.site_id = '{SITE1_ID}' \
                    AND r.parameter_id = '{GLOBAL_PARAM_DO_ID}'"
            ),
        ))
        .await
        .unwrap()
        .expect("the move is recorded");
    assert_eq!(row.try_get::<String>("", "was").unwrap(), "21");
    assert_eq!(row.try_get::<String>("", "became").unwrap(), "51");
    assert_eq!(row.try_get::<String>("", "origin").unwrap(), "janitor");
    assert_eq!(row.try_get::<Uuid>("", "job_id").unwrap(), job);
    assert!(
        row.try_get::<Option<Uuid>>("", "supersedes")
            .unwrap()
            .is_none(),
        "the first move on the reading supersedes nothing"
    );

    let second = sweep_curve_drift(&db, Some(job)).await.expect("sweep runs");
    assert_eq!(second.moved, 0, "a settled row records nothing further");
    let count: i64 = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT count(*) AS n FROM reading_decisions WHERE kind = 'curve_recompose'"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "n")
        .unwrap();
    assert_eq!(count, 1);
}

/// Scenario: the sweep recomposes a continuous sensor's corrected value that a stream-arm
/// calculation reads at the same instant, and no visit owns that reading.
///
/// Expected behaviour: the janitor hands the moved slot to a windowed `derived_recompute` under
/// its own run, and the derived value follows the recomposed input rather than keeping the old one
/// (Q108: nothing is left stale).
#[tokio::test]
#[serial]
async fn the_janitor_recomputes_a_stream_calculation_whose_continuous_input_it_recomposed() {
    use river_db::routes::private::reprocessing_jobs::service as jobs;

    let (db, app, token) = setup().await;
    let site = SITE1_ID.parse::<Uuid>().unwrap();
    let time = dt("2025-06-15T12:00:00Z");

    let code = format!("drift_do_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&db, &format!("{code}_set")).await;
    let (status, definition) = crate::common::post_json_parse_with_token(
        &app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": code,
            "name": "Drift DO mg/L",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
            "tool_script_id": calculation,
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "create derived: {definition}");
    crate::common::commit_calculation(&db, calculation).await;
    let output: Uuid = definition["output_parameter_id"]
        .as_str()
        .expect("the output parameter is created with the formula")
        .parse()
        .unwrap();
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &serde_json::json!({
            "site_id": SITE1_ID,
            "parameter_id": output,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "assign the output: {body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/readings/batch",
        &serde_json::json!({ "readings": [{
            "site_id": SITE1_ID,
            "parameter_id": GLOBAL_PARAM_DO_ID,
            "time": time.to_rfc3339(),
            "raw_value": 10.0,
        }]}),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "continuous source: {body}");

    let sensor = create_sensor(&db, "Drift-logger-01", GLOBAL_PARAM_DO_ID).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET calibration_id = '{}', calibrated_value = 10.0 \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{}'",
            sensor.base_calibration_id,
            time.to_rfc3339()
        ),
    )
    .await;
    settle(&db).await;
    river_db::routes::private::sensor_calibrations::service::recalculate_derived_at_timestamp(
        &db, site, time,
    )
    .await
    .expect("the derived value is computed from the corrected input");
    assert_eq!(
        derived_at(&db, output, time).await,
        Some(0.32),
        "10 * 0.032"
    );

    crate::common::exec(
        &db,
        &format!(
            "UPDATE sensor_calibrations SET slope = 5.0 WHERE id = '{}'",
            sensor.base_calibration_id
        ),
    )
    .await;

    let mut registry = jobs::build_registry();
    jobs::register_scheduled_services(&mut registry, &crate::common::cached_test_config());
    let ev: river_db::common::EventSender = tokio::sync::broadcast::channel(64).0;
    let janitor = jobs::enqueue(
        &db,
        "janitor_service",
        None,
        None,
        &serde_json::json!({}),
        None,
    )
    .await
    .unwrap()
    .expect("enqueue inserts a row");
    jobs::drain(&db, &ev, &registry, &jobs::worker_id())
        .await
        .unwrap();
    settle(&db).await;

    let children = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT count(*) AS c FROM reprocessing_jobs \
             WHERE parent_job_id = '{janitor}' AND trigger_type = 'derived_recompute' \
               AND status = 'completed'"
        ),
    )
    .await;
    assert_eq!(children, 1, "one recompute for the one drifted slot");
    assert_eq!(
        derived_at(&db, output, time).await,
        Some(1.6),
        "5 * 10 * 0.032: the derived value follows the recomposed input"
    );
}

/// Wait until no job is queued or running, so a background worker's run cannot land after the
/// assertion it would otherwise race.
async fn settle(db: &DatabaseConnection) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let busy = crate::common::e2e::count(
            db,
            "SELECT count(*) AS c FROM reprocessing_jobs WHERE status IN ('queued', 'running')",
        )
        .await;
        if busy == 0 {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "jobs still in flight after 60s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// The stored derived value at an instant, rounded to the precision the assertions compare at.
async fn derived_at(
    db: &DatabaseConnection,
    parameter_id: Uuid,
    time: chrono::DateTime<chrono::Utc>,
) -> Option<f64> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
             WHERE site_id = $1 AND parameter_id = $2 AND time = $3",
            [
                SITE1_ID.parse::<Uuid>().unwrap().into(),
                parameter_id.into(),
                time.into(),
            ],
        ))
        .await
        .unwrap()?;
    row.try_get::<Option<f64>>("", "value")
        .unwrap()
        .map(|v| (v * 1e9).round() / 1e9)
}
