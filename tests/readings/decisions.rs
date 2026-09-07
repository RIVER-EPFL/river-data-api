//! The curation record: a flag, a withdrawal, a curve, a pin or a value correction is a decision
//! appended to `reading_decisions`, and the reading's columns are its projection, written in the
//! same transaction. A rollback is itself a decision that restores the state the inverted one
//! recorded.
//!
//! Run with: cargo test --test readings decisions

use river_db::common::bulk_write;
use river_db::routes::private::readings::decisions::{self, Decision, DecisionKey, Kind, Origin};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const AT: &str = "2025-06-15T10:00:00Z";

struct Fixture {
    app: axum::Router,
    token: String,
    db: DatabaseConnection,
    stream: Uuid,
}

async fn setup() -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        &db,
        "decisions-temp",
        crate::common::PARAM_S1_TEMP_ID,
    )
    .await;
    Fixture {
        app,
        token,
        db,
        stream,
    }
}

async fn seed_group(f: &Fixture, values: &[f64]) {
    for (i, v) in values.iter().enumerate() {
        crate::common::exec(
            &f.db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
                 calibrated_value, replicate_index, measurement_type) \
                 VALUES ('{}', '{}', '{}', '{AT}', {v}, {v}, {i}, 'spot')",
                f.stream,
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await;
    }
}

struct Projected {
    is_flagged: bool,
    flag_reason: Option<String>,
    withdrawn: bool,
    raw_value: f64,
    calibrated_value: Option<f64>,
    unverified: bool,
}

async fn projected(f: &Fixture, replicate_index: i16) -> Projected {
    let row =
        f.db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT COALESCE(is_flagged, false) AS is_flagged, flag_reason, \
                        withdrawn_at IS NOT NULL AS withdrawn, raw_value, calibrated_value, \
                        unverified \
                 FROM readings WHERE stream_id = '{}' AND time = '{AT}' \
                   AND replicate_index = {replicate_index}",
                f.stream
            ),
        ))
        .await
        .unwrap()
        .expect("the seeded reading");
    Projected {
        is_flagged: row.try_get("", "is_flagged").unwrap(),
        flag_reason: row.try_get("", "flag_reason").unwrap(),
        withdrawn: row.try_get("", "withdrawn").unwrap(),
        raw_value: row.try_get("", "raw_value").unwrap(),
        calibrated_value: row.try_get("", "calibrated_value").unwrap(),
        unverified: row.try_get("", "unverified").unwrap(),
    }
}

fn key(stream: Uuid, replicate_index: Option<i16>) -> DecisionKey {
    DecisionKey {
        stream_id: stream,
        time: chrono::DateTime::parse_from_rfc3339(AT)
            .unwrap()
            .with_timezone(&chrono::Utc),
        replicate_index,
    }
}

fn decision(
    f: &Fixture,
    kind: Kind,
    replicate_index: Option<i16>,
    new: serde_json::Value,
) -> Decision {
    Decision {
        key: key(f.stream, replicate_index),
        kind,
        new,
        actor: "tester".to_string(),
        reason: Some("test".to_string()),
        origin: Origin::Manual,
        set_id: None,
    }
}

async fn record(db: &DatabaseConnection, d: Decision) -> Uuid {
    bulk_write::guarded(db, async |txn| decisions::record(txn, &d).await)
        .await
        .expect("decision recorded")
}

#[tokio::test]
#[serial]
async fn a_flag_projects_in_the_same_transaction_and_an_unflag_supersedes_it() {
    let f = setup().await;
    seed_group(&f, &[10.0, 20.0]).await;

    // The projection is visible inside the writing transaction, before it commits.
    let flag_id = bulk_write::guarded(&f.db, async |txn| {
        let id = decisions::record(
            txn,
            &decision(
                &f,
                Kind::Flag,
                Some(1),
                serde_json::json!({ "reason": "vial cracked" }),
            ),
        )
        .await?;
        let row = txn
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT is_flagged, flag_reason FROM readings WHERE stream_id = '{}' \
                     AND time = '{AT}' AND replicate_index = 1",
                    f.stream
                ),
            ))
            .await?
            .unwrap();
        assert_eq!(row.try_get::<Option<bool>>("", "is_flagged")?, Some(true));
        assert_eq!(
            row.try_get::<Option<String>>("", "flag_reason")?.as_deref(),
            Some("vial cracked")
        );
        Ok(id)
    })
    .await
    .unwrap();
    assert!(
        !projected(&f, 0).await.is_flagged,
        "the sibling replicate is untouched"
    );

    // The decision recorded what stood before it.
    let old = decisions::load(&f.db, flag_id).await.unwrap().old;
    assert_eq!(old["is_flagged"], false);

    let unflag_id = record(
        &f.db,
        decision(&f, Kind::Unflag, Some(1), serde_json::json!({})),
    )
    .await;
    let after = projected(&f, 1).await;
    assert!(!after.is_flagged);
    assert_eq!(after.flag_reason, None);
    let unflag = decisions::load(&f.db, unflag_id).await.unwrap();
    assert_eq!(
        unflag.supersedes,
        Some(flag_id),
        "the unflag names the flag it replaces"
    );
    assert_eq!(unflag.old["flag_reason"], "vial cracked");

    // Rolling the unflag back restores the flag from what the unflag recorded, and stamps it.
    let rollback_id = bulk_write::guarded(&f.db, async |txn| {
        decisions::rollback(txn, unflag_id, "tester", Some("wrong vial")).await
    })
    .await
    .unwrap();
    let restored = projected(&f, 1).await;
    assert!(restored.is_flagged);
    assert_eq!(restored.flag_reason.as_deref(), Some("vial cracked"));
    let unflag = decisions::load(&f.db, unflag_id).await.unwrap();
    assert_eq!(unflag.rolled_back_by, Some(rollback_id));
    let rollback = decisions::load(&f.db, rollback_id).await.unwrap();
    assert_eq!(rollback.kind, Kind::Rollback);
    assert_eq!(rollback.origin, Origin::Rollback);

    // A rollback cannot be applied twice.
    let again = bulk_write::guarded(&f.db, async |txn| {
        decisions::rollback(txn, unflag_id, "tester", None).await
    })
    .await;
    assert!(again.is_err(), "a decision already rolled back is refused");
}

#[tokio::test]
#[serial]
async fn a_group_decision_projects_onto_every_replicate() {
    let f = setup().await;
    seed_group(&f, &[10.0, 20.0, 30.0]).await;
    record(
        &f.db,
        decision(
            &f,
            Kind::Withdraw,
            None,
            serde_json::json!({ "reason": "wrong station" }),
        ),
    )
    .await;
    for i in 0..3 {
        assert!(projected(&f, i).await.withdrawn, "replicate {i} withdrawn");
    }
    record(
        &f.db,
        decision(&f, Kind::Reassert, None, serde_json::json!({})),
    )
    .await;
    for i in 0..3 {
        assert!(
            !projected(&f, i).await.withdrawn,
            "replicate {i} reasserted"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_value_correction_replaces_the_raw_value_and_leaves_the_correction_to_recompose() {
    let f = setup().await;
    seed_group(&f, &[10.0]).await;
    let id = record(
        &f.db,
        decision(
            &f,
            Kind::ValueCorrection,
            Some(0),
            serde_json::json!({ "raw_value": 12.5 }),
        ),
    )
    .await;
    let after = projected(&f, 0).await;
    assert_eq!(after.raw_value, 12.5);
    assert_eq!(
        after.calibrated_value, None,
        "the stored correction is stale until recomposed"
    );
    let d = decisions::load(&f.db, id).await.unwrap();
    assert_eq!(d.old["raw_value"], 10.0);
    bulk_write::guarded(&f.db, async |txn| {
        decisions::rollback(txn, id, "tester", None).await
    })
    .await
    .unwrap();
    assert_eq!(projected(&f, 0).await.raw_value, 10.0);
}

#[tokio::test]
#[serial]
async fn an_unverified_entry_projects_and_a_verify_clears_it() {
    let f = setup().await;
    seed_group(&f, &[10.0]).await;
    assert!(!projected(&f, 0).await.unverified);
    record(
        &f.db,
        decision(&f, Kind::UnverifiedEntry, Some(0), serde_json::json!({})),
    )
    .await;
    assert!(projected(&f, 0).await.unverified);
    record(
        &f.db,
        decision(&f, Kind::Verify, Some(0), serde_json::json!({})),
    )
    .await;
    assert!(!projected(&f, 0).await.unverified);
}

#[tokio::test]
#[serial]
async fn a_decision_on_a_row_nothing_stores_is_refused() {
    let f = setup().await;
    let result = bulk_write::guarded(&f.db, async |txn| {
        decisions::record(
            txn,
            &decision(
                &f,
                Kind::Flag,
                Some(0),
                serde_json::json!({ "reason": "x" }),
            ),
        )
        .await
    })
    .await;
    assert!(result.is_err());
}

#[tokio::test]
#[serial]
async fn a_decision_reaches_a_row_in_a_compressed_chunk() {
    let f = setup().await;
    seed_group(&f, &[10.0, 20.0]).await;
    let at = chrono::DateTime::parse_from_rfc3339(AT)
        .unwrap()
        .with_timezone(&chrono::Utc);
    let compressed = crate::common::compression::compress_readings_range(
        &f.db,
        at - chrono::Duration::days(1),
        at + chrono::Duration::days(1),
    )
    .await;
    assert!(
        compressed > 0,
        "the seeded instant sits in a compressed chunk"
    );
    record(
        &f.db,
        decision(
            &f,
            Kind::Flag,
            Some(0),
            serde_json::json!({ "reason": "late review" }),
        ),
    )
    .await;
    assert!(projected(&f, 0).await.is_flagged);
}

#[tokio::test]
#[serial]
async fn the_history_lists_a_key_newest_first() {
    let f = setup().await;
    seed_group(&f, &[10.0]).await;
    record(
        &f.db,
        decision(
            &f,
            Kind::Flag,
            Some(0),
            serde_json::json!({ "reason": "a" }),
        ),
    )
    .await;
    record(
        &f.db,
        decision(&f, Kind::Unflag, Some(0), serde_json::json!({})),
    )
    .await;
    let (status, body) = crate::common::get_json_with_token(
        &f.app,
        &format!(
            "/api/readings/decisions?stream_id={}&time={AT}&replicate_index=0",
            f.stream
        ),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let kinds: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["unflag", "flag"]);
    assert_eq!(body[1]["actor"], "tester");
    assert_eq!(body[1]["origin"], "manual");
    assert_eq!(body[1]["reason"], "test");
}

// --- The writers: each curation path appends a decision, each derivation path appends none ---

async fn decision_rows(db: &DatabaseConnection, kind: &str, origin: &str) -> i64 {
    crate::common::e2e::count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM reading_decisions WHERE kind = '{kind}' \
             AND origin = '{origin}'"
        ),
    )
    .await
}

#[tokio::test]
#[serial]
async fn the_flag_routes_append_manual_decisions_and_decide_nothing_twice() {
    let f = setup().await;
    seed_group(&f, &[10.0, 20.0]).await;
    let reading_key = serde_json::json!({
        "site_id": crate::common::SITE1_ID,
        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
        "time": AT,
        "replicate_index": 1,
        "measurement_type": "spot",
    });
    let (status, body) = crate::common::patch_json_with_token(
        &f.app,
        "/api/readings/flag",
        &serde_json::json!({ "readings": [reading_key], "reason": "vial cracked" }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(projected(&f, 1).await.is_flagged);
    assert_eq!(decision_rows(&f.db, "flag", "manual").await, 1);

    // Already flagged: the route decides nothing again.
    let (status, body) = crate::common::patch_json_with_token(
        &f.app,
        "/api/readings/flag",
        &serde_json::json!({ "readings": [reading_key], "reason": "again" }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(decision_rows(&f.db, "flag", "manual").await, 1);
    assert_eq!(
        projected(&f, 1).await.flag_reason.as_deref(),
        Some("vial cracked")
    );

    let (status, body) = crate::common::patch_json_with_token(
        &f.app,
        "/api/readings/unflag",
        &serde_json::json!({ "readings": [reading_key] }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(!projected(&f, 1).await.is_flagged);
    let history = decisions::history(&f.db, &key(f.stream, Some(1)))
        .await
        .unwrap();
    assert_eq!(history[0].kind, Kind::Unflag);
    assert_eq!(history[0].supersedes, Some(history[1].id));

    // A range unflag over an already-clear row decides nothing.
    let (status, body) = crate::common::patch_json_with_token(
        &f.app,
        "/api/readings/unflag_range",
        &serde_json::json!({
            "site_id": crate::common::SITE1_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
            "start_time": "2025-06-15T00:00:00Z",
            "end_time": "2025-06-16T00:00:00Z",
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(decision_rows(&f.db, "unflag", "manual").await, 1);
}

#[tokio::test]
#[serial]
async fn a_grab_replace_records_a_value_correction_only_where_the_value_moved() {
    let f = setup().await;
    let save = |values: Vec<f64>, replace: bool| {
        let app = f.app.clone();
        let token = f.token.clone();
        async move {
            let readings: Vec<serde_json::Value> = values
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                        "value": v,
                        "time": AT,
                    })
                })
                .collect();
            let mut body = serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "readings": readings,
            });
            if replace {
                body["mode"] = serde_json::json!("replace");
            }
            let (status, resp) =
                crate::common::post_json_with_token(&app, "/api/grab_samples", &body, &token).await;
            assert_eq!(status, 200, "{resp}");
        }
    };
    save(vec![10.0, 20.0], false).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "manual").await, 0);
    save(vec![10.0, 25.0], true).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "manual").await, 1);
    let corrected =
        f.db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT replicate_index, old, new FROM reading_decisions \
             WHERE kind = 'value_correction'"
                .to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(corrected.try_get::<i16>("", "replicate_index").unwrap(), 1);
    assert_eq!(
        corrected.try_get::<serde_json::Value>("", "old").unwrap()["raw_value"],
        20.0
    );
    assert_eq!(
        corrected.try_get::<serde_json::Value>("", "new").unwrap()["raw_value"],
        25.0
    );
    // Replaying the same values decides nothing.
    save(vec![10.0, 25.0], true).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "manual").await, 1);
}

#[tokio::test]
#[serial]
async fn a_batch_overwrite_records_a_manual_value_correction_once() {
    let f = setup().await;
    let batch = |value: f64| {
        let app = f.app.clone();
        let token = f.token.clone();
        async move {
            let (status, resp) = crate::common::post_json_with_token(
                &app,
                "/api/readings/batch",
                &serde_json::json!({
                    "conflict": "overwrite",
                    "readings": [{
                        "site_id": crate::common::SITE1_ID,
                        "parameter_id": crate::common::GLOBAL_PARAM_TEMP_ID,
                        "time": "2025-06-15T11:00:00Z",
                        "raw_value": value,
                    }],
                }),
                &token,
            )
            .await;
            assert_eq!(status, 200, "{resp}");
        }
    };
    batch(5.0).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "manual").await, 0);
    batch(6.0).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "manual").await, 1);
    batch(6.0).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "manual").await, 1);
}

#[tokio::test]
#[serial]
async fn a_merge_moves_every_reading_as_a_slot_move_and_deletes_none() {
    let f = setup().await;
    seed_group(&f, &[10.0, 20.0]).await;
    let before = crate::common::e2e::count(&f.db, "SELECT COUNT(*)::bigint FROM readings").await;
    let at_source = crate::common::e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings WHERE site_id = '{}' AND parameter_id = '{}'",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    let result = river_db::routes::private::admin::merge_services::merge_site_parameters(
        &f.db,
        &river_db::routes::private::admin::merge_services::MergeSiteParametersRequest {
            source_site_parameter_id: crate::common::PARAM_S1_TEMP_ID.parse().unwrap(),
            target_site_parameter_id: crate::common::PARAM_S1_DO_ID.parse().unwrap(),
        },
        "tester",
    )
    .await
    .expect("the merge applies");
    assert_eq!(i64::try_from(result.merged_readings).unwrap(), at_source);
    assert_eq!(
        crate::common::e2e::count(&f.db, "SELECT COUNT(*)::bigint FROM readings").await,
        before,
        "nothing deletes"
    );
    assert_eq!(decision_rows(&f.db, "slot_move", "manual").await, at_source);
    let moved =
        f.db.query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT old, new FROM reading_decisions WHERE kind = 'slot_move' LIMIT 1".to_string(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        moved.try_get::<serde_json::Value>("", "old").unwrap()["parameter_id"],
        crate::common::GLOBAL_PARAM_TEMP_ID
    );
    assert_eq!(
        moved.try_get::<serde_json::Value>("", "new").unwrap()["parameter_id"],
        crate::common::GLOBAL_PARAM_DO_ID
    );
}

#[tokio::test]
#[serial]
async fn a_derivation_writer_appends_no_decision() {
    let f = setup().await;
    seed_group(&f, &[10.0, 20.0]).await;
    // The janitor recompose re-derives corrected values from the row's own curves: a derivation,
    // never a decision.
    river_db::routes::private::sensors::calibrations::service::recompose_from_own_curves(
        &f.db,
        "TRUE",
        "r.stream_id = $1",
        vec![f.stream.into()],
    )
    .await
    .expect("recompose runs");
    assert_eq!(
        crate::common::e2e::count(&f.db, "SELECT COUNT(*)::bigint FROM reading_decisions").await,
        0
    );
}

// --- Pins: a set-level attribution that reprocess honours (M59) ----------------------------

async fn sensor_of(f: &Fixture, at: &str) -> Option<String> {
    f.db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "SELECT sensor_id::text AS sensor_id FROM readings \
                 WHERE stream_id = '{}' AND time = '{at}' AND replicate_index = 0",
            f.stream
        ),
    ))
    .await
    .unwrap()
    .and_then(|r| r.try_get::<Option<String>>("", "sensor_id").ok().flatten())
}

#[tokio::test]
#[serial]
async fn a_pinned_instrument_survives_reprocess_and_a_rolled_back_pin_returns_the_window() {
    let f = setup().await;
    let times = [
        "2025-06-15T10:00:00Z",
        "2025-06-15T11:00:00Z",
        "2025-06-15T12:00:00Z",
    ];
    for (i, t) in times.iter().enumerate() {
        crate::common::exec(
            &f.db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
                 calibrated_value, replicate_index, measurement_type) \
                 VALUES ('{}', '{}', '{}', '{t}', {i}, {i}, 0, 'continuous')",
                f.stream,
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await;
    }
    let a = crate::common::e2e::create_sensor(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "PIN-A",
    )
    .await;
    let b = crate::common::e2e::create_sensor(
        &f.app,
        &f.token,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "PIN-B",
    )
    .await;
    crate::common::e2e::create_deployment(
        &f.app,
        &f.token,
        &a,
        crate::common::SITE1_ID,
        crate::common::GLOBAL_PARAM_TEMP_ID,
        "2025-06-01T00:00:00Z",
    )
    .await;
    assert!(crate::common::e2e::wait_for_jobs_by_trigger(&f.db, "deployment_create", 60).await);
    for t in times {
        assert_eq!(
            sensor_of(&f, t).await.as_deref(),
            Some(a.as_str()),
            "window owns {t}"
        );
    }

    // Two rows turn out to be instrument B's: pinned as one set.
    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/readings/pins",
        &serde_json::json!({
            "kind": "instrument",
            "target_id": b,
            "selection": { "keys": [
                { "stream_id": f.stream, "time": times[1], "replicate_index": 0 },
                { "stream_id": f.stream, "time": times[2], "replicate_index": 0 },
            ]},
            "reason": "logged by the spare probe",
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rows_decided"], 2);
    let set_id = body["set_id"].as_str().unwrap().to_string();
    assert!(crate::common::e2e::wait_for_jobs_by_trigger(&f.db, "attribution_pin", 60).await);
    assert_eq!(sensor_of(&f, times[0]).await.as_deref(), Some(a.as_str()));
    assert_eq!(sensor_of(&f, times[1]).await.as_deref(), Some(b.as_str()));
    assert_eq!(sensor_of(&f, times[2]).await.as_deref(), Some(b.as_str()));

    // A reprocess of the slot re-derives by window and leaves the pinned rows alone.
    river_db::routes::private::sensors::calibrations::service::reprocess_site_parameter_readings(
        &f.db,
        crate::common::SITE1_ID.parse().unwrap(),
        crate::common::GLOBAL_PARAM_TEMP_ID.parse().unwrap(),
    )
    .await
    .expect("reprocess runs");
    assert_eq!(sensor_of(&f, times[0]).await.as_deref(), Some(a.as_str()));
    assert_eq!(
        sensor_of(&f, times[1]).await.as_deref(),
        Some(b.as_str()),
        "the pin holds"
    );
    assert_eq!(
        sensor_of(&f, times[2]).await.as_deref(),
        Some(b.as_str()),
        "the pin holds"
    );

    // The provenance record shows the pin on the pinned instant and none on the other.
    let (status, record) = crate::common::get_json_with_token(
        &f.app,
        &format!(
            "/api/readings/provenance?stream_id={}&time={}",
            f.stream, times[1]
        ),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{record}");
    let pins = &record["records"][0]["chain"]["pins"];
    assert_eq!(pins.as_array().map(Vec::len), Some(1), "{record}");
    assert_eq!(pins[0]["kind"], "instrument_pin");
    assert_eq!(pins[0]["target"]["sensor_id"], b);
    let (_, record) = crate::common::get_json_with_token(
        &f.app,
        &format!(
            "/api/readings/provenance?stream_id={}&time={}",
            f.stream, times[0]
        ),
        &f.token,
    )
    .await;
    assert!(record["records"][0]["chain"]["pins"].is_null(), "{record}");

    // Rolling the set back returns the rows to the window, and a second rollback is refused.
    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        &format!("/api/readings/pins/{set_id}/rollback"),
        &serde_json::json!({}),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["rolled_back"], 2);
    assert!(crate::common::e2e::wait_for_jobs_by_trigger(&f.db, "attribution_pin", 60).await);
    for t in times {
        assert_eq!(
            sensor_of(&f, t).await.as_deref(),
            Some(a.as_str()),
            "window owns {t} again"
        );
    }
    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        &format!("/api/readings/pins/{set_id}/rollback"),
        &serde_json::json!({}),
        &f.token,
    )
    .await;
    assert_eq!(status, 409, "{body}");

    // An empty selection is refused before anything is recorded.
    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/readings/pins",
        &serde_json::json!({ "kind": "instrument", "target_id": b, "selection": {} }),
        &f.token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// Every key the record and the reading disagree about, as the janitor counts them.
async fn drift_keys(db: &DatabaseConnection) -> Vec<(Uuid, i16)> {
    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            decisions::inconsistent_rows_sql(),
        ))
        .await
        .expect("the drift statement runs");
    rows.iter()
        .map(|r| {
            (
                r.try_get("", "stream_id").unwrap(),
                r.try_get("", "replicate_index").unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
#[serial]
async fn the_columns_equal_the_fold_of_the_decisions_that_wrote_them() {
    let f = setup().await;
    seed_group(&f, &[1.0, 2.0, 3.0]).await;
    assert!(
        drift_keys(&f.db).await.is_empty(),
        "rows nothing has decided are born consistent"
    );

    let flag = record(
        &f.db,
        decision(
            &f,
            Kind::Flag,
            Some(0),
            serde_json::json!({ "reason": "spike" }),
        ),
    )
    .await;
    record(
        &f.db,
        decision(&f, Kind::Unflag, Some(1), serde_json::json!({})),
    )
    .await;
    record(
        &f.db,
        decision(
            &f,
            Kind::Withdraw,
            Some(1),
            serde_json::json!({ "reason": "absent from source window" }),
        ),
    )
    .await;
    record(
        &f.db,
        decision(&f, Kind::UnverifiedEntry, Some(2), serde_json::json!({})),
    )
    .await;
    record(
        &f.db,
        decision(&f, Kind::Verify, Some(2), serde_json::json!({})),
    )
    .await;
    // A group decision covers every replicate, so the fold must read it for each of them.
    record(
        &f.db,
        decision(
            &f,
            Kind::Withdraw,
            None,
            serde_json::json!({ "reason": "the whole instant" }),
        ),
    )
    .await;
    record(
        &f.db,
        decision(&f, Kind::Reassert, Some(0), serde_json::json!({})),
    )
    .await;
    assert!(
        drift_keys(&f.db).await.is_empty(),
        "the projection is the fold after every kind that writes an owned column"
    );

    bulk_write::guarded(&f.db, async |txn| {
        decisions::rollback(txn, flag, "tester", Some("undone")).await
    })
    .await
    .expect("the flag rolls back");
    assert!(
        drift_keys(&f.db).await.is_empty(),
        "a rollback restores exactly what the fold then expects"
    );

    // A column moved out of band, which is the projection bug the sweep exists to see.
    crate::common::exec(
        &f.db,
        &format!(
            "UPDATE readings SET is_flagged = TRUE WHERE stream_id = '{}' AND time = '{AT}' \
             AND replicate_index = 2",
            f.stream
        ),
    )
    .await;
    assert_eq!(
        drift_keys(&f.db).await,
        vec![(f.stream, 2)],
        "the flagged row with no live flag decision is the only key reported"
    );
}

/// The hold an intern's entry files, so the resolve route has one to rule on. Written directly
/// because minting an intern JWT needs the Keycloak fixture; the level-to-kind decision is
/// covered inline and the route gate in the capability matrix.
async fn open_unverified_hold(f: &Fixture) -> Uuid {
    let id = Uuid::new_v4();
    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (id, stream_id, site_id, parameter_id, group_time, kind, expected, computed, \
                  delta, status) \
             VALUES ('{id}', NULL, '{}', '{}', '{AT}', 'unverified_entry', \
                     '{{\"state\": \"verified\"}}'::jsonb, \
                     '{{\"state\": \"unverified\"}}'::jsonb, '{{}}'::jsonb, 'pending')",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    id
}

async fn resolve(f: &Fixture, hold: Uuid, mode: &str) -> (u16, String) {
    crate::common::post_json_with_token(
        &f.app,
        &format!("/api/sync/replicate_audit_holds/{hold}/resolve"),
        &serde_json::json!({ "mode": mode }),
        &f.token,
    )
    .await
}

#[tokio::test]
#[serial]
async fn a_verify_accepts_an_intern_entry_as_it_stands() {
    let f = setup().await;
    seed_group(&f, &[1.0, 2.0, 3.0]).await;
    bulk_write::guarded(&f.db, async |txn| {
        decisions::record(
            txn,
            &decision(&f, Kind::UnverifiedEntry, None, serde_json::json!({})),
        )
        .await
    })
    .await
    .expect("the entry is pending");
    assert!(projected(&f, 0).await.unverified, "the entry lands pending");

    let hold = open_unverified_hold(&f).await;
    let (status, body) = resolve(&f, hold, "verify").await;
    assert_eq!(status, 200, "{body}");
    for index in 0..3 {
        let row = projected(&f, index).await;
        assert!(!row.unverified, "replicate {index} is verified");
        assert!(!row.withdrawn, "a verify withdraws nothing");
    }
    // Ruling twice on one entry is refused: the hold is no longer pending.
    let (status, _) = resolve(&f, hold, "verify").await;
    assert_eq!(status, 404);
}

#[tokio::test]
#[serial]
async fn a_reject_withdraws_an_intern_entry_and_a_reassert_restores_it() {
    let f = setup().await;
    seed_group(&f, &[1.0, 2.0, 3.0]).await;
    bulk_write::guarded(&f.db, async |txn| {
        decisions::record(
            txn,
            &decision(&f, Kind::UnverifiedEntry, None, serde_json::json!({})),
        )
        .await
    })
    .await
    .expect("the entry is pending");

    let hold = open_unverified_hold(&f).await;
    let (status, body) = resolve(&f, hold, "reject").await;
    assert_eq!(status, 200, "{body}");
    for index in 0..3 {
        let row = projected(&f, index).await;
        assert!(row.withdrawn, "replicate {index} is withdrawn");
        assert!(!row.unverified, "a rejected entry is no longer pending");
    }

    // Nothing deletes: the rejection is a stamp a reassert lifts.
    record(
        &f.db,
        decision(&f, Kind::Reassert, None, serde_json::json!({})),
    )
    .await;
    for index in 0..3 {
        assert!(
            !projected(&f, index).await.withdrawn,
            "replicate {index} is restored"
        );
    }
    assert!(
        drift_keys(&f.db).await.is_empty(),
        "every ruling is on the record, so nothing drifts"
    );
}

/// Scenario: the same file imported twice with `conflict: overwrite`, the second time with a
/// changed value.
///
/// Expected behaviour: the correction is a decision, the same as the one `/readings/batch` records
/// for the same change, and a re-import of the unchanged values decides nothing.
#[tokio::test]
#[serial]
async fn a_csv_overwrite_records_a_csv_value_correction_once() {
    let f = setup().await;

    let import = |value: f64| {
        let app = f.app.clone();
        let token = f.token.clone();
        async move {
            let (status, resp) = crate::common::post_json_parse_with_token(
                &app,
                "/api/readings/import_csv",
                &serde_json::json!({
                    "site": crate::common::SITE1_ID,
                    "conflict": "overwrite",
                    "csv": format!("DateTime,DO_Temperature\n2025-06-15 11:00:00,{value}\n"),
                }),
                &token,
            )
            .await;
            assert_eq!(status, 200, "import ({status}): {resp}");
        }
    };

    import(5.0).await;
    wait_for_csv_value(&f.db, 5.0).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "csv").await, 0);

    import(6.0).await;
    wait_for_csv_value(&f.db, 6.0).await;
    assert_eq!(decision_rows(&f.db, "value_correction", "csv").await, 1);

    import(6.0).await;
    wait_for_csv_value(&f.db, 6.0).await;
    assert_eq!(
        decision_rows(&f.db, "value_correction", "csv").await,
        1,
        "re-importing the same value corrects nothing"
    );
}

/// The import runs in a tracked job, so the assertion waits for the value to land.
async fn wait_for_csv_value(db: &DatabaseConnection, expected: f64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let row = db
            .query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!(
                    "SELECT raw_value FROM readings \
                     WHERE site_id = '{}' AND parameter_id = '{}' \
                       AND time = '2025-06-15T11:00:00Z'",
                    crate::common::SITE1_ID,
                    crate::common::GLOBAL_PARAM_TEMP_ID
                ),
            ))
            .await
            .unwrap();
        if let Some(row) = row
            && let Ok(Some(value)) = row.try_get::<Option<f64>>("", "raw_value")
            && (value - expected).abs() < 1e-9
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the import worker never stored {expected}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
