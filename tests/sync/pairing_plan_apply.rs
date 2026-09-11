//! Pairing-plan apply/revert now run as tracked background jobs (`plan_apply`/`plan_revert`): the
//! endpoint returns a `job_id` immediately and the heavy backfill runs in the job. This test pins
//! the end state, applying pairs the stream and backfills its readings; reverting unpairs them,
//! so the conversion can't silently change behavior.
//!
//! Run: cargo test --test sync -- --test-threads=1

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

pub fn job_id_of(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text).unwrap()["job_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn count(db: &sea_orm::DatabaseConnection, from: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT COUNT(*)::bigint AS n FROM {from}"),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

async fn scalar_opt_uuid(db: &sea_orm::DatabaseConnection, sql: &str) -> Option<Uuid> {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<Option<Uuid>>("", "v")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn apply_then_revert_pairing_plan_via_jobs() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'loc-apply-1', 'Loc Apply 1', true)"
        ),
    )
    .await;
    // Two unpaired readings on the stream.
    for ts in ["2025-02-01T00:00:00Z", "2025-02-01T00:10:00Z"] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
                 VALUES ('{stream_id}', '{ts}', 1.0, 0)"
            ),
        )
        .await;
    }

    let entries = serde_json::json!([{
        "stream_id": stream_id,
        "source_key": "loc-apply-1",
        "source_name": "Loc Apply 1",
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Site 1", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Temperature", "create": false, "units": "C", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "original_parameter_name": null
    }]);
    let plan_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;

    // Apply, returns a job id immediately; the backfill runs in the job.
    crate::common::plans::acknowledge_plan(&app, &token, &plan_id.to_string()).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "apply should be 2xx, got {status}: {text}"
    );
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    // Plan applied, stream paired, readings backfilled with a site_id.
    let plan_status = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status AS v FROM pairing_plans WHERE id = '{plan_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "v")
        .unwrap();
    assert_eq!(plan_status, "applied");
    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{stream_id}'")
        )
        .await
        .is_some(),
        "stream should be paired"
    );
    assert!(
        scalar_opt_uuid(&db, &format!("SELECT site_id AS v FROM readings WHERE stream_id = '{stream_id}' ORDER BY time LIMIT 1")).await.is_some(),
        "readings should be backfilled with a site_id"
    );

    // Revert, also a job; unpairs the stream and clears the readings' site_id.
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "revert", &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "revert should be 2xx, got {status}: {text}"
    );
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let plan_status = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT status AS v FROM pairing_plans WHERE id = '{plan_id}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<String>("", "v")
        .unwrap();
    assert_eq!(plan_status, "reverted");
    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{stream_id}'")
        )
        .await
        .is_none(),
        "stream should be unpaired"
    );
    assert!(
        scalar_opt_uuid(&db, &format!("SELECT site_id AS v FROM readings WHERE stream_id = '{stream_id}' ORDER BY time LIMIT 1")).await.is_none(),
        "readings site_id should be cleared"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Attribution arriving late is what makes a portal's spot readings addressable as visits, so the
/// plan apply attaches their collection events exactly as the single-stream pairing does.
#[tokio::test]
#[serial]
async fn apply_attaches_collection_events_for_spot_readings() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active, measurement_type) \
             VALUES ('{stream_id}', 'cnet', 'FP1:DOC_avg_ppb:reps', 'FP1 DOC', true, 'spot')"
        ),
    )
    .await;
    for (ts, replicates) in [("2025-03-01T09:00:00Z", 2), ("2025-03-08T09:00:00Z", 1)] {
        for index in 0..replicates {
            crate::common::exec(
                &db,
                &format!(
                    "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
                     VALUES ('{stream_id}', '{ts}', {}, {index})",
                    2.5 + f64::from(index) / 10.0
                ),
            )
            .await;
        }
    }

    let entries = serde_json::json!([{
        "stream_id": stream_id,
        "source_key": "FP1:DOC_avg_ppb:reps",
        "source_name": "FP1 DOC",
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Site 1", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Temperature", "create": false, "units": "C", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "acknowledged": true,
        "original_parameter_name": null
    }]);
    let plan_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'cnet', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;

    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "apply should be 2xx, got {status}: {text}"
    );
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let events = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT source FROM collection_events WHERE site_id = '{}' ORDER BY collected_at",
                crate::common::SITE1_ID
            ),
        ))
        .await
        .unwrap();
    assert_eq!(events.len(), 2, "one event per visited instant");
    for row in &events {
        assert_eq!(
            row.try_get::<String>("", "source").unwrap(),
            "portal_sync",
            "a sync-registered stream's visits are portal_sync"
        );
    }

    let unstamped = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*) AS v FROM readings \
                 WHERE stream_id = '{stream_id}' AND collection_event_id IS NULL"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "v")
        .unwrap();
    assert_eq!(unstamped, 0, "every paired spot reading names its visit");

    let n = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT s.n AS v FROM samples s \
                 JOIN readings r ON r.sample_id = s.id \
                 WHERE r.stream_id = '{stream_id}' AND r.time = '2025-03-01T09:00:00Z' \
                 LIMIT 1"
            ),
        ))
        .await
        .unwrap()
        .map(|row| row.try_get::<i32>("", "v").unwrap());
    assert_eq!(
        n,
        Some(2),
        "the plan apply materialises the replicate group's statistics, as single-stream pairing does"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a `plan_apply` run commits, loses its lease, and the reaper hands the row to another
/// worker.
///
/// Expected behaviour: the replay finds the plan already applied and completes reporting nothing,
/// rather than failing the operator's import over the draft guard that the first run satisfied.
#[tokio::test]
#[serial]
async fn a_replayed_apply_reports_a_replay_instead_of_failing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'loc-replay-1', 'Loc Replay 1', true)"
        ),
    )
    .await;
    let entries = serde_json::json!([{
        "stream_id": stream_id,
        "source_key": "loc-replay-1",
        "source_name": "Loc Replay 1",
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
        "site": { "id": crate::common::SITE1_ID, "name": "Site 1", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Temperature", "create": false, "units": "C", "group_key": null, "original_names": [] },
        "confidence": "exact",
        "warnings": [],
        "original_parameter_name": null
    }]);
    let plan_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;

    crate::common::plans::acknowledge_plan(&app, &token, &plan_id.to_string()).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &token)
            .await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let replay = river_db::routes::private::reprocessing_jobs::service::enqueue(
        &db,
        "plan_apply",
        None,
        None,
        &serde_json::json!({ "plan_id": plan_id }),
        None,
    )
    .await
    .unwrap()
    .expect("the replay is enqueued");

    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &replay.to_string()).await,
        "completed",
        "a run over an applied plan is a replay, not a failure"
    );
    let counts = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT detail->'counts' AS v FROM reprocessing_jobs WHERE id = '{replay}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<serde_json::Value>("", "v")
        .unwrap();
    assert_eq!(
        counts,
        serde_json::json!({}),
        "the replay claims none of the first run's work"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// A large import has to be distinguishable from a stuck one, so the apply reports how far it has
/// got on the job row while its single transaction is still open.
#[tokio::test]
#[serial]
async fn apply_reports_its_progress_over_the_plan_s_entries() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let mut entries = Vec::new();
    for n in 0..3 {
        let stream_id = Uuid::new_v4();
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
                 VALUES ('{stream_id}', 'vaisala', 'loc-progress-{n}', 'Loc Progress {n}', true)"
            ),
        )
        .await;
        entries.push(serde_json::json!({
            "stream_id": stream_id,
            "source_key": format!("loc-progress-{n}"),
            "source_name": format!("Loc Progress {n}"),
            "action": "pair",
            "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
            "site": { "id": crate::common::SITE1_ID, "name": "Site 1", "create": false, "latitude": null, "longitude": null, "altitude_m": null },
            "parameter": { "id": crate::common::GLOBAL_PARAM_TEMP_ID, "name": "Temperature", "create": false, "units": "C", "group_key": null, "original_names": [] },
            "confidence": "exact",
            "warnings": [],
            "original_parameter_name": null
        }));
    }

    let plan_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            serde_json::Value::Array(entries)
                .to_string()
                .replace('\'', "''")
        ),
    )
    .await;

    crate::common::plans::acknowledge_plan(&app, &token, &plan_id.to_string()).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &token)
            .await;
    assert!(
        (200..300).contains(&status),
        "apply should be 2xx, got {status}: {text}"
    );
    let job_id = job_id_of(&text);
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id).await,
        "completed"
    );

    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT progress, total FROM reprocessing_jobs WHERE id = '{job_id}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<Option<i32>>("", "total").unwrap(),
        Some(3),
        "the total is the plan's pairing entries, set before the first one is applied"
    );
    assert_eq!(
        row.try_get::<Option<i32>>("", "progress").unwrap(),
        Some(3),
        "every entry is counted as it is paired"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a source that declares its own category registry on each stream, on a database that
/// holds no parameter groups.
///
/// Expected behaviour: the plan proposes the group, the apply creates it once for every column of
/// that category and places each parameter in it at the position and role the registry gives,
/// and a parameter an operator has already placed keeps the placement it has.
#[tokio::test]
#[serial]
async fn apply_creates_the_group_the_source_registry_names() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    for (key, column, ordinal, role) in [
        ("cat-a", "WTW_pH_1", 3, "measured"),
        ("cat-b", "Field_BP", 8, "output"),
    ] {
        let stream_id = Uuid::new_v4();
        let metadata = serde_json::json!({
            "hierarchy": { "project": "Test Project", "site": "Site 1", "parameter": column },
            "units": "-",
            "parameter": {
                "column_name": column,
                "category": "Field data",
                "category_ordinal": ordinal,
                "role": role,
                "description": "from field sheet",
            },
        });
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO data_streams (id, source_system, source_key, source_name, metadata, is_active) \
                 VALUES ('{stream_id}', 'catsrc', '{key}', 'Site 1 - {column}', '{}'::jsonb, true)",
                metadata.to_string().replace('\'', "''")
            ),
        )
        .await;
    }

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "catsrc" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan: {plan}");
    assert_eq!(
        plan["summary"]["groups_to_create"],
        serde_json::json!(1),
        "one category behind both columns: {}",
        plan["summary"]
    );
    let group = &plan["entries"][0]["parameter"]["group"];
    assert_eq!(group["code"], serde_json::json!("field_data"), "{group}");
    assert_eq!(group["label"], serde_json::json!("Field data"), "{group}");
    assert_eq!(group["create"], serde_json::json!(true), "{group}");

    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    crate::common::plans::acknowledge_plan(&app, &token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id, "apply", &token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let placed = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.code AS code, m.ordinal AS ordinal, m.role AS role, g.code AS group_code \
             FROM parameter_group_members m \
             JOIN parameter_groups g ON g.id = m.group_id \
             JOIN parameters p ON p.id = m.parameter_id \
             ORDER BY m.ordinal"
                .to_string(),
        ))
        .await
        .unwrap();
    let placed: Vec<(String, i32, String, String)> = placed
        .iter()
        .map(|row| {
            (
                row.try_get::<String>("", "code").unwrap(),
                row.try_get::<i32>("", "ordinal").unwrap(),
                row.try_get::<String>("", "role").unwrap(),
                row.try_get::<String>("", "group_code").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        placed,
        vec![
            ("WTW_pH_1".into(), 3, "measured".into(), "field_data".into()),
            ("Field_BP".into(), 8, "output".into(), "field_data".into()),
        ],
        "both columns land in the one group, at the registry's positions and roles"
    );

    let groups = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COUNT(*)::bigint AS n FROM parameter_groups".to_string(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get::<i64>("", "n")
        .unwrap();
    assert_eq!(groups, 1, "the group is created once, not once per column");
}

/// Scenario: a portal's own instrument register, offered by the connector and admitted by a plan.
///
/// Expected behaviour: nothing exists until the apply runs, the plan carries every offered row, a
/// row the review declines is left behind as a proposal, and the admitted one becomes an instrument
/// under the source's own key with its serial.
#[tokio::test]
#[serial]
async fn the_source_register_becomes_instruments_only_when_a_plan_admits_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, metadata, is_active) \
             VALUES ('{stream_id}', 'regsrc', 'FP1:Depth', 'FP1 - Depth', \
                     '{{\"hierarchy\": {{\"project\": \"Test Project\", \"site\": \"Site 1\", \"parameter\": \"Depth\"}}, \"units\": \"mm\"}}'::jsonb, true)"
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/sensors/proposals",
        &serde_json::json!({
            "source_system": "regsrc",
            "instruments": [
                { "source_key": "sensor_inventory:62", "name": "Turbidity probe FP1",
                  "serial_number": "919402", "model": "OBS-3+", "is_lab_instrument": false,
                  "metadata": { "station": "FP1", "installed_on": "2019-06-01" } },
                { "source_key": "sensor_inventory:63", "name": "Retired probe",
                  "is_lab_instrument": false }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the register is offered: {body}");
    assert!(body.contains("\"stored\":2"), "{body}");

    // Offered is not created.
    let sensors_now = count(&db, "sensors WHERE source_system = 'regsrc'").await;
    assert_eq!(sensors_now, 0, "a proposal creates no instrument");

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "regsrc" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan: {plan}");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let offered = plan["instrument_proposals"].as_array().expect("proposals");
    assert_eq!(offered.len(), 2, "the plan carries the register: {plan}");
    assert!(
        offered
            .iter()
            .all(|p| p["admit"] == serde_json::json!(true)),
        "proposed admitted, since the register is the lab's own record: {plan}"
    );

    // The review leaves one behind.
    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "instruments": [{ "source_key": "sensor_inventory:63", "admit": false }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "decline ({status}): {body}");

    crate::common::plans::acknowledge_plan(&app, &token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id, "apply", &token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let admitted = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_key, name, serial_number, model FROM sensors \
              WHERE source_system = 'regsrc' AND source_key LIKE 'sensor_inventory:%' \
              ORDER BY source_key"
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(
        admitted.len(),
        1,
        "only the admitted row became an instrument"
    );
    assert_eq!(
        admitted[0].try_get::<String>("", "source_key").unwrap(),
        "sensor_inventory:62"
    );
    assert_eq!(
        admitted[0]
            .try_get::<Option<String>>("", "serial_number")
            .unwrap()
            .as_deref(),
        Some("919402"),
        "the register's serial travels with it"
    );

    let left = count(&db, "instrument_proposals WHERE source_system = 'regsrc'").await;
    assert_eq!(
        left, 1,
        "the declined row stays a proposal for the next plan"
    );
}
