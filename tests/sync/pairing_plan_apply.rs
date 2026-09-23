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
        "instrument": { "id": null, "name": "DOC analyser", "source_key": "cnet:DOC", "resolved_by": "placeholder", "create": true, "confirmed": true, "stamps_readings": false },
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

/// A draft vaisala plan pairing one fresh stream to Site 1 / Temperature, reviewed and ready to
/// apply.
async fn reviewed_single_stream_plan(
    db: &sea_orm::DatabaseConnection,
    app: &axum::Router,
    token: &str,
    source_key: &str,
) -> (Uuid, Uuid) {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', '{source_key}', '{source_key}', true)"
        ),
    )
    .await;
    let entries = serde_json::json!([{
        "stream_id": stream_id,
        "source_key": source_key,
        "source_name": source_key,
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
        db,
        &format!(
            "INSERT INTO pairing_plans (id, source_system, status, summary, entries) \
             VALUES ('{plan_id}', 'vaisala', 'draft', '{{}}'::jsonb, '{}'::jsonb)",
            entries.to_string().replace('\'', "''")
        ),
    )
    .await;
    crate::common::plans::acknowledge_plan(app, token, &plan_id.to_string()).await;
    (plan_id, stream_id)
}

/// Apply a reviewed plan through the route and wait for the apply and the attribution it queued.
async fn apply_and_settle(
    db: &sea_orm::DatabaseConnection,
    app: &axum::Router,
    token: &str,
    plan_id: Uuid,
) {
    let (status, text) =
        crate::common::post_plan_action_with_token(app, &plan_id.to_string(), "apply", token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(db, &job_id_of(&text)).await,
        "completed"
    );
    assert_eq!(
        crate::common::jobs::wait_for_triggered_job(db, "plan_attribution", None).await,
        "completed"
    );
}

async fn attribution_jobs(db: &sea_orm::DatabaseConnection, plan_id: Uuid) -> i64 {
    count(
        db,
        &format!(
            "reprocessing_jobs WHERE trigger_type = 'plan_attribution' \
             AND params->>'plan_id' = '{plan_id}'"
        ),
    )
    .await
}

async fn replay_apply(db: &sea_orm::DatabaseConnection, plan_id: Uuid) -> Uuid {
    let replay = river_db::routes::private::reprocessing_jobs::service::enqueue(
        db,
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
        crate::common::jobs::wait_for_job(db, &replay.to_string()).await,
        "completed",
        "a run over an applied plan is a replay, not a failure"
    );
    replay
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

    let (plan_id, _) = reviewed_single_stream_plan(&db, &app, &token, "loc-replay-1").await;
    apply_and_settle(&db, &app, &token, plan_id).await;

    let replay = replay_apply(&db, plan_id).await;
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
    assert_eq!(
        attribution_jobs(&db, plan_id).await,
        1,
        "the first run's attribution stands, so the replay queues no second one"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the attribution a plan apply hands on cannot be queued.
///
/// Expected behaviour: the apply is not committed without it: the plan stays a draft and its
/// stream unpaired, so a retry applies it whole rather than finding it applied and stopping.
#[tokio::test]
#[serial]
async fn an_apply_whose_attribution_cannot_be_queued_commits_nothing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (plan_id, stream_id) =
        reviewed_single_stream_plan(&db, &app, &token, "loc-lost-attribution").await;
    crate::common::jobs::refuse_enqueue(&db, "plan_attribution").await;
    let applied = river_db::routes::private::sync::service::apply_plan(&db, plan_id, None).await;
    crate::common::jobs::restore_enqueue(&db).await;

    assert!(applied.is_err(), "the apply reports the refused enqueue");
    assert_eq!(
        count(
            &db,
            &format!("pairing_plans WHERE id = '{plan_id}' AND status = 'draft'")
        )
        .await,
        1,
        "the plan is still a draft"
    );
    assert_eq!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{stream_id}'")
        )
        .await,
        None,
        "the stream is still unpaired"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: a plan was applied and its attribution job is gone (lost before this fix, or pruned),
/// and the `plan_apply` row runs again.
///
/// Expected behaviour: the replay queues the attribution under itself, so the paired readings are
/// re-derived by window rather than keeping the pairing's frozen context.
#[tokio::test]
#[serial]
async fn a_replayed_apply_queues_the_attribution_its_plan_lacks() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    let (plan_id, _) = reviewed_single_stream_plan(&db, &app, &token, "loc-replay-2").await;
    apply_and_settle(&db, &app, &token, plan_id).await;
    crate::common::exec(
        &db,
        "DELETE FROM reprocessing_jobs WHERE trigger_type = 'plan_attribution'",
    )
    .await;

    let replay = replay_apply(&db, plan_id).await;
    assert_eq!(
        attribution_jobs(&db, plan_id).await,
        1,
        "the replay queues the attribution the plan lacked"
    );
    assert_eq!(
        scalar_opt_uuid(
            &db,
            "SELECT parent_job_id AS v FROM reprocessing_jobs WHERE trigger_type = 'plan_attribution'"
        )
        .await,
        Some(replay),
        "the attribution is the replay's child"
    );
    assert_eq!(
        crate::common::jobs::wait_for_triggered_job(&db, "plan_attribution", None).await,
        "completed"
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
/// that category and places each parameter in it at the position the registry gives, and a
/// parameter an operator has already placed keeps the placement it has.
#[tokio::test]
#[serial]
async fn apply_creates_the_group_the_source_registry_names() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    for (key, column, ordinal, calculation) in [
        ("cat-a", "WTW_pH_1", 3, serde_json::Value::Null),
        (
            "cat-b",
            "Field_BP",
            8,
            serde_json::json!({ "function": "calcPCO2", "inputs": ["WTW_pH_1"] }),
        ),
    ] {
        let stream_id = Uuid::new_v4();
        let metadata = serde_json::json!({
            "hierarchy": { "project": "Test Project", "site": "Site 1", "parameter": column },
            "units": "-",
            "parameter": {
                "column_name": column,
                "category": "Field data",
                "category_ordinal": ordinal,
                "description": "from field sheet",
                "source_calculation": calculation,
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
    let carried: Vec<&serde_json::Value> = plan["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| &e["parameter"]["calculation"])
        .collect();
    assert!(
        carried.iter().any(|c| {
            c["function"] == serde_json::json!("calcPCO2")
                && c["inputs"] == serde_json::json!(["WTW_pH_1"])
        }),
        "the output column's own calculation rides on the plan: {carried:?}"
    );
    assert!(
        carried.iter().any(|c| c.is_null()),
        "and the measured column declares none: {carried:?}"
    );

    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    crate::common::plans::acknowledge_plan(&app, &token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id, "apply", &token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.code AS code, m.ordinal AS ordinal, g.code AS group_code, \
                    m.source_calculation AS source_calculation \
             FROM parameter_group_members m \
             JOIN parameter_groups g ON g.id = m.group_id \
             JOIN parameters p ON p.id = m.parameter_id \
             ORDER BY m.ordinal"
                .to_string(),
        ))
        .await
        .unwrap();
    let placed: Vec<(String, i32, String)> = rows
        .iter()
        .map(|row| {
            (
                row.try_get::<String>("", "code").unwrap(),
                row.try_get::<i32>("", "ordinal").unwrap(),
                row.try_get::<String>("", "group_code").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        placed,
        vec![
            ("WTW_pH_1".into(), 3, "field_data".into()),
            ("Field_BP".into(), 8, "field_data".into()),
        ],
        "both columns land in the one group, at the registry's positions"
    );
    let recorded: Vec<Option<serde_json::Value>> = rows
        .iter()
        .map(|row| {
            row.try_get::<Option<serde_json::Value>>("", "source_calculation")
                .unwrap()
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            None,
            Some(serde_json::json!({ "function": "calcPCO2", "inputs": ["WTW_pH_1"] })),
        ],
        "the computed member records what the source computed it with, the entered one nothing"
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

/// Scenario: the source's register offers a probe whose serial an instrument in the inventory
/// already carries, which is the shape a plan minting a device by serial leaves behind.
///
/// Expected behaviour: the row is not proposed admitted, it names the instrument it would
/// duplicate, and attaching merges the register's serial, model and metadata onto that instrument
/// rather than leaving two rows for one probe (Q76).
#[tokio::test]
#[serial]
async fn a_register_row_colliding_with_an_instrument_attaches_instead_of_duplicating() {
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

    // The probe the plan's device path already minted, under its own key and serial.
    let existing = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, serial_number, \
                                  data_frequency, source_system, source_key) \
             VALUES ('{existing}', 'FP1 turbidity', true, false, '919402', 'high', 'regsrc', 'device:919402')"
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
                  "metadata": { "station": "FP1", "installed_on": "2019-06-01" } }
            ]
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the register is offered: {body}");

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": "regsrc" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan: {plan}");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let offered = &plan["instrument_proposals"][0];
    assert_eq!(
        offered["admit"],
        serde_json::json!(false),
        "a row that would duplicate an instrument is not taken by default: {plan}"
    );
    assert_eq!(
        offered["conflict"]["id"],
        serde_json::json!(existing.to_string()),
        "the row names the instrument it would duplicate: {plan}"
    );

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "instruments": [{ "source_key": "sensor_inventory:62", "admit": false,
                              "attach_to": existing.to_string() }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "attach ({status}): {body}");

    crate::common::plans::acknowledge_plan(&app, &token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id, "apply", &token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    assert_eq!(
        count(&db, "sensors WHERE serial_number = '919402'").await,
        1,
        "one probe, one row"
    );
    assert_eq!(
        count(
            &db,
            "sensors WHERE source_system = 'regsrc' AND source_key LIKE 'sensor_inventory:%'"
        )
        .await,
        0,
        "attaching mints nothing under the register's own key"
    );
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT model, metadata FROM sensors WHERE id = '{existing}'"),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<Option<String>>("", "model")
            .unwrap()
            .as_deref(),
        Some("OBS-3+"),
        "the register's model is merged onto the instrument"
    );
    let metadata: serde_json::Value = row.try_get("", "metadata").unwrap();
    assert_eq!(metadata["station"], serde_json::json!("FP1"));

    assert_eq!(
        count(&db, "instrument_proposals WHERE source_system = 'regsrc'").await,
        0,
        "the attached row leaves the queue"
    );
}

/// Scenario: a plan whose entries create their own site and parameter is applied and then reverted.
/// Expected behaviour: the revert unpairs the streams and unattributes their readings, and leaves
/// every row the apply created standing, which is what `ConfirmStep` and `ApplyResults` tell the
/// operator. The plan stays `reverted`, so the same entries cannot be applied a second time.
#[tokio::test]
#[serial]
async fn revert_keeps_the_rows_the_apply_created() {
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
             VALUES ('{stream_id}', 'vaisala', 'loc-create-1', 'Loc Create 1', true)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, raw_value, replicate_index) \
             VALUES ('{stream_id}', '2025-04-01T00:00:00Z', 3.0, 0)"
        ),
    )
    .await;

    let sites_before = count(&db, "sites").await;
    let parameters_before = count(&db, "parameters").await;
    let site_parameters_before = count(&db, "site_parameters").await;

    let entries = serde_json::json!([{
        "stream_id": stream_id,
        "source_key": "loc-create-1",
        "source_name": "Loc Create 1",
        "action": "pair",
        "project": { "id": crate::common::PROJECT_ID, "name": "Test Project", "create": false },
        "site": { "id": null, "name": "Revert Site", "create": true, "latitude": null, "longitude": null, "altitude_m": null },
        "parameter": { "id": null, "name": "Revert Parameter", "create": true, "units": "C", "group_key": null, "original_names": [] },
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
    assert!(
        (200..300).contains(&status),
        "apply should be 2xx, got {status}: {text}"
    );
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    assert_eq!(
        count(&db, "sites").await,
        sites_before + 1,
        "apply creates the site"
    );
    assert_eq!(
        count(&db, "parameters").await,
        parameters_before + 1,
        "apply creates the parameter"
    );
    assert_eq!(
        count(&db, "site_parameters").await,
        site_parameters_before + 1,
        "apply creates the slot"
    );

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

    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_parameter_id AS v FROM data_streams WHERE id = '{stream_id}'")
        )
        .await
        .is_none(),
        "revert unpairs the stream"
    );
    assert!(
        scalar_opt_uuid(
            &db,
            &format!("SELECT site_id AS v FROM readings WHERE stream_id = '{stream_id}' LIMIT 1")
        )
        .await
        .is_none(),
        "revert unattributes the readings"
    );

    // What the operator is told stays: the created rows are not rolled back.
    assert_eq!(
        count(&db, "sites").await,
        sites_before + 1,
        "the created site survives the revert"
    );
    assert_eq!(
        count(&db, "parameters").await,
        parameters_before + 1,
        "the created parameter survives the revert"
    );
    assert_eq!(
        count(&db, "site_parameters").await,
        site_parameters_before + 1,
        "the created slot survives the revert"
    );

    // A reverted plan is terminal: the same entries cannot be applied again.
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &token)
            .await;
    assert_eq!(
        status, 409,
        "re-applying a reverted plan is refused: {text}"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Scenario: the review accepts one of the two parameters a plan would create, and applies.
///
/// Expected behaviour: the accepted one lands reviewed, because the acceptance is the review the
/// flag waits for; the one nobody accepted is still mechanical and keeps the flag.
#[tokio::test]
#[serial]
async fn an_accepted_parameter_lands_without_the_review_flag() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());

    for (key, column) in [("rev-a", "Accepted_Param"), ("rev-b", "Unaccepted_Param")] {
        let stream_id = Uuid::new_v4();
        let metadata = serde_json::json!({
            "hierarchy": { "project": "Test Project", "site": "Site 1", "parameter": column },
            "units": "-",
        });
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO data_streams (id, source_system, source_key, source_name, metadata, is_active) \
                 VALUES ('{stream_id}', 'revsrc', '{key}', 'Site 1 - {column}', '{}'::jsonb, true)",
                metadata.to_string().replace('\'', "''")
            ),
        )
        .await;
    }

    let plan = crate::common::plans::create_plan(&app, &token, "revsrc").await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let (status, text) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({
            "expected_version": plan["version"],
            "objects": [{ "key": "parameter:Accepted_Param", "accepted": true }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "accepting one object: {text}");

    crate::common::plans::acknowledge_plan(&app, &token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&app, &plan_id, "apply", &token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&db, &job_id_of(&text)).await,
        "completed"
    );

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT code, needs_review FROM parameters \
             WHERE code IN ('Accepted_Param', 'Unaccepted_Param') ORDER BY code"
                .to_string(),
        ))
        .await
        .unwrap();
    let flagged: Vec<(String, bool)> = rows
        .iter()
        .map(|row| {
            (
                row.try_get::<String>("", "code").unwrap(),
                row.try_get::<bool>("", "needs_review").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        flagged,
        vec![
            ("Accepted_Param".to_string(), false),
            ("Unaccepted_Param".to_string(), true),
        ],
        "the accepted parameter is reviewed, the other is not"
    );
}
