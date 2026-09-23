//! Pairing-plan workflow (USER_STORIES "Stream Pairing Workflow") over the HTTP surface: the
//! draft → inspect → update → apply → revert lifecycle. A single full-permission API token
//! drives every route. The apply-time entity-resolution rules are
//! single-domain and live in `tests/sync/pairing_plan_resolution.rs`.
//!
//! Apply and revert run as tracked `plan_apply`/`plan_revert` jobs: the endpoint returns a `job_id`
//! and the pairing, backfill, and plan status transition happen in the job. `run_plan_action` posts
//! the action, waits for the job to reach `completed`, and returns its `detail.counts`, so count
//! assertions read the job detail and DB-fact assertions run only after the job has finished.
//!
//! The streams are METALP's: the story is about the lifecycle, and NOMIS pairing is refused
//! outright until its timestamps have a zone (`tests/data_streams/nomis_pairing_refused.rs`).
//!
//! Run: cargo test --test e2e -- --test-threads=1

use serial_test::serial;

use crate::common::e2e::count;
use crate::common::plans::{find_entry, run_plan_action};

#[tokio::test]
#[serial]
async fn create_inspect_update_apply_revert_full_lifecycle() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;

    let cond1 = uuid::Uuid::new_v4().to_string();
    let temp1 = uuid::Uuid::new_v4().to_string();
    let cond2 = uuid::Uuid::new_v4().to_string();
    let temp2 = uuid::Uuid::new_v4().to_string();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &cond1,
        "metalp",
        "c1",
        "METALP",
        "GL1_DN",
        "Conductivity",
        "uS/cm",
        None,
        3,
    )
    .await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &temp1,
        "metalp",
        "t1",
        "METALP",
        "GL1_DN",
        "Temperature",
        "degC",
        None,
        3,
    )
    .await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &cond2,
        "metalp",
        "c2",
        "METALP",
        "GL2_UP",
        "Conductivity",
        "uS/cm",
        None,
        3,
    )
    .await;
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &temp2,
        "metalp",
        "t2",
        "METALP",
        "GL2_UP",
        "Temperature",
        "degC",
        None,
        3,
    )
    .await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    // create
    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({"source_system": "metalp"}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    assert_eq!(plan["status"], "draft");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let s = &plan["summary"];
    assert_eq!(s["total_streams"], 4);
    assert_eq!(s["will_pair"], 4);
    assert_eq!(s["will_skip"], 0);
    assert_eq!(s["sites_to_create"], 2);
    assert_eq!(
        s["parameters_to_create"], 2,
        "Conductivity + Temperature: {plan}"
    );

    // inspect
    let (status, plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &token,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(plan["entries"].as_array().unwrap().len(), 4);
    let e = find_entry(&plan, &cond1);
    assert_eq!(e["action"], "pair");
    assert_eq!(e["project"]["name"], "METALP");
    assert_eq!(e["site"]["create"], true);
    assert_eq!(e["parameter"]["create"], true);
    assert_eq!(e["confidence"], "none");

    // an existing parameter to map the Conductivity streams onto (seeded AFTER create so the plan
    // proposed creating it; the update points the entries at the real row by id)
    let cond_param = uuid::Uuid::new_v4().to_string();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES ('{cond_param}', 'conductivity', 'Conductivity', 'uS/cm', 'measurement')",
        ),
    )
    .await;

    // update: map both Conductivity entries to the existing param
    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({"updates": [
            {"stream_id": cond1, "parameter_id": cond_param},
            {"stream_id": cond2, "parameter_id": cond_param}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "update plan ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).unwrap();
    let e = find_entry(&plan, &cond1);
    assert_eq!(
        e["parameter"]["id"],
        serde_json::json!(cond_param),
        "mapped to existing param"
    );
    assert_eq!(e["parameter"]["create"], false);

    // rename what the plan creates: the project everywhere, one site, and the new parameter
    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({"updates": [
            {"stream_id": cond1, "project_name": "Glacier streams", "site_name": "Glacier 1 downstream"},
            {"stream_id": temp1, "project_name": "Glacier streams", "site_name": "Glacier 1 downstream", "parameter_name": "Water temperature"},
            {"stream_id": cond2, "project_name": "Glacier streams"},
            {"stream_id": temp2, "project_name": "Glacier streams", "parameter_name": "Water temperature"}
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "rename in plan ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).unwrap();
    let e = find_entry(&plan, &temp1);
    assert_eq!(e["project"]["name"], "Glacier streams");
    assert_eq!(e["project"]["create"], true);
    assert_eq!(e["site"]["name"], "Glacier 1 downstream");
    assert_eq!(e["parameter"]["name"], "Water temperature");
    assert_eq!(e["parameter"]["create"], true);

    // apply (runs as a plan_apply job)
    let counts = run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(counts["streams_paired"], 4);
    assert_eq!(counts["sites_created"], 2);
    assert_eq!(counts["projects_created"], 1);
    assert_eq!(
        counts["parameters_created"], 1,
        "only Temperature is new; Conductivity reused: {counts}"
    );
    assert_eq!(counts["site_parameters_created"], 4);
    assert_eq!(counts["readings_backfilled"], 12);
    for (sql, what) in [
        (
            "SELECT count(*) AS c FROM projects WHERE name = 'Glacier streams'",
            "the renamed project",
        ),
        (
            "SELECT count(*) AS c FROM sites WHERE name = 'Glacier 1 downstream'",
            "the renamed site",
        ),
        (
            "SELECT count(*) AS c FROM parameters WHERE name = 'Water temperature'",
            "the renamed parameter",
        ),
    ] {
        assert_eq!(count(&db, sql).await, 1, "apply created {what}");
    }
    for (sql, what) in [
        (
            "SELECT count(*) AS c FROM projects WHERE name = 'METALP'",
            "the source's project name",
        ),
        (
            "SELECT count(*) AS c FROM sites WHERE name = 'GL1_DN'",
            "the source's site name",
        ),
        (
            "SELECT count(*) AS c FROM parameters WHERE name = 'Temperature'",
            "the source's parameter name",
        ),
    ] {
        assert_eq!(
            count(&db, sql).await,
            0,
            "apply created nothing under {what}"
        );
    }

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams WHERE pairing_plan_id = '{plan_id}' AND site_parameter_id IS NOT NULL"
        )).await,
        4, "all four streams paired under the plan"
    );
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM parameters WHERE LOWER(code) = 'conductivity'"
        )
        .await,
        1,
        "the reused parameter was not duplicated"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT count(*) AS c FROM readings r JOIN data_streams ds ON r.stream_id = ds.id \
             WHERE ds.pairing_plan_id = '{plan_id}' AND r.site_id IS NOT NULL"
            )
        )
        .await,
        12,
        "readings backfilled with site_id"
    );
    let (_, plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &token,
    )
    .await;
    assert_eq!(plan["status"], "applied");

    // re-apply rejected
    let (status, _) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "apply", &token)
            .await;
    assert_eq!(status, 409, "cannot re-apply an applied plan");

    // revert (runs as a plan_revert job)
    let counts = run_plan_action(&app, &token, &plan_id, "revert").await;
    assert_eq!(counts["reverted"], 4);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS c FROM readings r JOIN data_streams ds ON r.stream_id = ds.id \
             WHERE ds.source_system = 'metalp' AND r.site_id IS NOT NULL"
        )
        .await,
        0,
        "revert re-NULLed the backfilled readings"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM data_streams WHERE source_system = 'metalp' AND site_parameter_id IS NULL").await,
        4, "all streams unpaired again"
    );
    assert!(
        count(
            &db,
            "SELECT count(*) AS c FROM sites WHERE LOWER(name) IN ('glacier 1 downstream','gl2_up')"
        )
        .await
            >= 2,
        "created catalog sites are retained after revert"
    );
    let (_, plan) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &token,
    )
    .await;
    assert_eq!(plan["status"], "reverted");

    // re-revert rejected
    let (status, _) =
        crate::common::post_plan_action_with_token(&app, &plan_id.to_string(), "revert", &token)
            .await;
    assert_eq!(status, 409, "cannot revert a non-applied plan");
}
