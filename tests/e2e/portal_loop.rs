//! S9, a portal source through the real sync driver to a served value.
//!
//! The two halves of this seam were each proven against a stub: the API's suite wrote the ingest
//! bodies itself, and core's driver test drove a fake API. Here the real `SyncDriver` runs a
//! `SourceBackend` against the real router, and every assertion is read from the database the API
//! wrote. What the fake portal does not exercise is the MariaDB decode, which is deliberate
//! (Q85): `river-data-rshiny/tests/portal_fixture.rs` covers that half against the committed
//! fixture database.
//!
//! Four beats: the source registers and pushes; the operator pairs the discovered streams
//! through a pairing plan and the readings become a served value with sample statistics; once the
//! cycle after the pairing has re-asserted each window, a cycle over unchanged content sends
//! nothing, which is the digest handshake; and a historical value edited at source travels the
//! whole loop to a proposal an operator decides (Q84), which is what moves the served number.
//!
//! Run: cargo test --test e2e portal_loop -- --test-threads=1

use river_data_core::chrono::{TimeZone, Utc};
use river_data_core::client::SyncService;
use serial_test::serial;

use crate::common::e2e::count;
use crate::common::fake_portal::{FAMILY_MEAN_COLUMN, FakePortal, SOURCE_SYSTEM, enrolled_driver};
use crate::common::plans::run_plan_action;

/// The family's sample at the first visit: its replicate count and its mean, which is what the
/// site serves for that instant.
async fn served_sample(db: &sea_orm::DatabaseConnection) -> (i32, f64) {
    use sea_orm::ConnectionTrait;
    let row = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT s.n AS n, s.mean AS mean FROM samples s \
                 JOIN readings r ON r.sample_id = s.id \
                 JOIN data_streams ds ON ds.id = r.stream_id \
                 WHERE ds.source_key = 'S01:{FAMILY_MEAN_COLUMN}:reps' \
                   AND r.time = '{FIRST_VISIT}' LIMIT 1"
            ),
        ))
        .await
        .expect("sample query")
        .expect("the paired family has a sample at the first visit");
    (
        row.try_get::<i32>("", "n").expect("n"),
        row.try_get::<f64>("", "mean").expect("mean"),
    )
}

/// The ingest receipts the portal source's streams have committed, one per pass the server applied.
async fn receipts(db: &sea_orm::DatabaseConnection) -> i64 {
    count(
        db,
        &format!(
            "SELECT COUNT(*)::bigint FROM ingest_receipts ir \
             JOIN data_streams s ON s.id = ir.stream_id \
             WHERE s.source_system = '{SOURCE_SYSTEM}'"
        ),
    )
    .await
}

/// The first visit both stations were seeded at.
const FIRST_VISIT: &str = "2026-06-01T09:00:00Z";

#[tokio::test]
#[serial]
async fn a_portal_source_reaches_a_served_value_and_changes_only_when_decided() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let token = crate::common::seed_token_full(&db).await;
    let portal = FakePortal::seeded();
    let driver = enrolled_driver(app.clone(), &state, portal.clone()).await;

    // The source registers its streams and pushes its content.
    let first = driver.sync(false).await.expect("the first cycle");
    assert!(
        first.errors.is_empty(),
        "the first cycle reported errors: {:?}",
        first.errors
    );
    assert!(
        first.readings_synced > 0,
        "the first cycle carried readings: {first:?}"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM data_streams \
                 WHERE source_system = '{SOURCE_SYSTEM}' AND site_parameter_id IS NULL"
            )
        )
        .await,
        4,
        "everything arrives unpaired: attribution comes from the pairing, never from the request"
    );

    // The operator pairs what was discovered. The plan proposes the sites and parameters from the
    // descriptors' own hierarchy metadata, and the instruments from the curve columns they name,
    // so agreeing to those proposals is the whole answer here.
    let plan = crate::common::plans::create_plan(&app, &token, SOURCE_SYSTEM).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    crate::common::plans::confirm_plan_instruments(&app, &token, &plan_id).await;
    crate::common::plans::attach_held_curves(&app, &token, &plan_id).await;
    let counts = run_plan_action(&app, &token, &plan_id, "apply").await;
    assert!(
        counts["streams_paired"].as_i64().unwrap_or_default() >= 4,
        "the plan paired every discovered stream: {counts}"
    );

    // Paired, the family's replicates are a served spot value: one sample per station per visit,
    // with the statistics the trigger computed from the replicates that arrived.
    let (n, mean) = served_sample(&db).await;
    assert_eq!(n, 3, "three replicates arrived at the first visit");
    // (310 + 316 + 313) / 3
    assert!(
        (mean - 313.0).abs() < 1e-9,
        "the served mean is the replicates' own: {mean}"
    );

    // The pairing forgets each paired stream's digest, since what the source sent while unpaired
    // was not all applied (annotations are refused on an unpaired stream). The next cycle
    // therefore re-asserts every paired window once: a receipt each, and nothing new.
    let paired_streams = count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM data_streams \
             WHERE source_system = '{SOURCE_SYSTEM}' AND site_parameter_id IS NOT NULL"
        ),
    )
    .await;
    let before_reassert = receipts(&db).await;
    let reassert = driver
        .sync(false)
        .await
        .expect("the cycle after the pairing");
    assert!(
        reassert.errors.is_empty(),
        "the cycle after the pairing reported errors: {:?}",
        reassert.errors
    );
    assert_eq!(
        reassert.readings_synced, 0,
        "the re-asserted windows hold nothing new: {reassert:?}"
    );
    assert_eq!(
        receipts(&db).await,
        before_reassert + paired_streams,
        "every paired stream re-asserted its window once"
    );

    // A further cycle re-reads the same content. The digest the server stored for each clean pass
    // matches what the backend would send, so nothing is sent at all.
    let ingested_before = receipts(&db).await;
    let second = driver.sync(false).await.expect("the second cycle");
    assert!(
        second.errors.is_empty(),
        "the second cycle reported errors: {:?}",
        second.errors
    );
    assert_eq!(
        second.readings_synced, 0,
        "unchanged content is not re-sent: {second:?}"
    );
    assert_eq!(
        receipts(&db).await,
        ingested_before,
        "a cycle that sent nothing committed no receipt"
    );

    // The lab corrects a replicate of a visit synced three cycles ago. The source is re-read whole,
    // so the edit travels as a correction rather than an append, and the digest is what decides
    // the pass is sent at all.
    portal.edit(
        "S01",
        Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0)
            .single()
            .expect("a representable instant"),
        |visit| visit.replicates[0] = Some(322.0),
    );
    let third = driver.sync(false).await.expect("the third cycle");
    assert!(
        third.errors.is_empty(),
        "the third cycle reported errors: {:?}",
        third.errors
    );
    assert_eq!(
        receipts(&db).await,
        ingested_before + 1,
        "an edited source no longer matches the stored digest, so the pass is sent and receipted"
    );

    // Q84: the correction is proposed, not applied. The served number stands until a person
    // decides it.
    assert_eq!(
        served_sample(&db).await,
        (3, 313.0),
        "a proposed correction has not moved the served value"
    );
    let (code, pending) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/reading_change_proposals?filter={}",
            crate::common::e2e::percent_encode(r#"{"status":"pending"}"#)
        ),
        &token,
    )
    .await;
    assert_eq!(code, 200, "{pending}");
    assert_eq!(
        pending.as_array().map(Vec::len),
        Some(1),
        "one replicate moved at source, so one proposal is raised: {pending}"
    );
    let proposal = &pending[0];
    assert!(
        (proposal["proposed_raw_value"].as_f64().unwrap_or_default() - 322.0).abs() < 1e-9,
        "the proposal carries the source's number: {proposal}"
    );
    assert!(
        (proposal["stored_raw_value"].as_f64().unwrap_or_default() - 310.0).abs() < 1e-9,
        "and the one it would replace: {proposal}"
    );

    // Accepted, the correction is written through the curation record and the statistics follow.
    let (code, decided) = crate::common::post_json_with_token(
        &app,
        "/api/sync/change_proposals/decide",
        &serde_json::json!({ "ids": [proposal["id"].as_str().expect("proposal id")],
                             "decision": "accept" }),
        &token,
    )
    .await;
    assert_eq!(code, 200, "{decided}");
    let (n, mean) = served_sample(&db).await;
    assert_eq!(n, 3, "accepting a correction changes no replicate count");
    // (322 + 316 + 313) / 3
    assert!(
        (mean - 317.0).abs() < 1e-9,
        "the accepted correction moves the served mean: {mean}"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM reading_decisions d \
                 JOIN data_streams s ON s.id = d.stream_id \
                 WHERE s.source_system = '{SOURCE_SYSTEM}' \
                   AND d.kind = 'value_correction' AND d.origin = 'sync'"
            )
        )
        .await,
        1,
        "the accepted value names the decision that wrote it"
    );
}

/// Expected behaviour: the source grows between cycles and the loop follows it without an
/// operator. A visit added inside a window already reconciled classifies as new rather than being
/// mistaken for a withdrawal, and a station that did not exist at enrolment registers as unpaired
/// streams the pairing plan can propose.
#[tokio::test]
#[serial]
async fn a_visit_added_inside_a_reconciled_window_lands_and_a_new_station_reaches_the_plan() {
    use river_data_core::chrono::{TimeZone, Utc};

    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let token = crate::common::seed_token_full(&db).await;

    // The driver owns its backend, so the content is grown through a second view of it.
    let portal = FakePortal::seeded();
    let driver = enrolled_driver(app.clone(), &state, portal.handle()).await;

    driver.sync(false).await.expect("the first cycle");
    let plan = crate::common::plans::create_plan(&app, &token, SOURCE_SYSTEM).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    crate::common::plans::confirm_plan_instruments(&app, &token, &plan_id).await;
    crate::common::plans::attach_held_curves(&app, &token, &plan_id).await;
    run_plan_action(&app, &token, &plan_id, "apply").await;

    // A visit entered between two the store already holds: inside the window every pass
    // re-asserts, which is where a diff could mistake it for something withdrawn.
    let backdated = Utc
        .with_ymd_and_hms(2026, 6, 4, 7, 0, 0)
        .single()
        .expect("a representable instant");
    portal.add_visit(
        "S01",
        crate::common::fake_portal::Visit {
            at: backdated,
            single: Some(7.9),
            replicates: [Some(330.0), Some(334.0), Some(332.0)],
        },
    );
    // And a station nobody had seen at enrolment.
    portal.add_station(
        "S03",
        vec![crate::common::fake_portal::Visit {
            at: Utc
                .with_ymd_and_hms(2026, 6, 15, 9, 0, 0)
                .single()
                .expect("a representable instant"),
            single: Some(3.3),
            replicates: [Some(90.0), Some(94.0), Some(92.0)],
        }],
    );

    let grown = driver.sync(false).await.expect("the second cycle");
    assert!(
        grown.errors.is_empty(),
        "the growing cycle reported errors: {:?}",
        grown.errors
    );

    // The backdated visit is served at its own instant, and nothing at S01 was withdrawn for it.
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*)::bigint FROM readings r JOIN data_streams s ON s.id = r.stream_id \
             WHERE s.source_key = 'S01:water_temp_degC' AND r.time = '2026-06-04T07:00:00Z' \
               AND r.withdrawn_at IS NULL"
        )
        .await,
        1,
        "the visit added inside the window is stored"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM readings r JOIN data_streams s ON s.id = r.stream_id \
                 WHERE s.source_system = '{SOURCE_SYSTEM}' AND r.withdrawn_at IS NOT NULL"
            )
        )
        .await,
        0,
        "growing the source withdrew nothing"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*)::bigint FROM collection_events \
             WHERE collected_at = '2026-06-04T07:00:00Z'"
        )
        .await,
        1,
        "the new visit is a collection event of its own"
    );

    // The new station registered by itself, unpaired, and the plan proposes it.
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM data_streams \
                 WHERE source_system = '{SOURCE_SYSTEM}' AND source_key LIKE 'S03:%' \
                   AND site_parameter_id IS NULL"
            )
        )
        .await,
        2,
        "a station absent at enrolment discovers itself, and arrives unpaired"
    );
    let next = crate::common::plans::create_plan(&app, &token, SOURCE_SYSTEM).await;
    let entries = next["entries"].as_array().expect("entries");
    let s03: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["site"]["name"] == "S03")
        .collect();
    assert_eq!(
        s03.len(),
        2,
        "the plan proposes the new station's two streams: {next}"
    );
    let family = s03
        .iter()
        .find(|e| e["source_key"] == format!("S03:{FAMILY_MEAN_COLUMN}:reps"))
        .unwrap_or_else(|| panic!("the family is proposed as one entry: {next}"));
    assert_eq!(
        family["replicates"]["n"], 3,
        "proposed as the family it is, not three columns: {family}"
    );
    assert_eq!(family["site"]["create"], true, "S03 is a site to create");
}
