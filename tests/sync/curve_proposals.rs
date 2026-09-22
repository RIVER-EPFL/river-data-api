//! A source's standard curves reach the database through the pairing plan, attached to one of the
//! plan's instruments.
//!
//! Scenario: a portal replicates its `standard_curves` table on the first sync of a blank database,
//! before anyone has made a pairing plan. The portal links no curve to an instrument; the label it
//! sends is the curve's parameter cell, which is a guess at the instrument and not an answer
//! (Q195). A curve names one instrument (`standard_curves.sensor_id` is NOT NULL).
//!
//! Expected behaviour: registering a curve creates neither the curve nor an instrument and holds it
//! for the plan; the plan attaches it to one of its instruments, or skips it (Q220); the apply
//! creates the curve under that instrument, and refuses while a held curve is neither attached nor
//! skipped.
//!
//! Run: cargo test --test sync curve_proposals -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use super::pairing_plan_apply::job_id_of;

const SOURCE: &str = "curveprop";
const LABEL: &str = "DOC corr";
const CURVE_KEY: &str = "standard_curves:17";

async fn count(db: &DatabaseConnection, from: &str) -> i64 {
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

async fn register_curve(app: &axum::Router, token: &str, slope: f64) -> serde_json::Value {
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/standard_curves/register",
        &json!({
            "source_system": SOURCE,
            "source_key": CURVE_KEY,
            "instrument_label": LABEL,
            "slope": slope,
            "intercept": 1.0,
            "name": format!("{LABEL} 2025-01-01"),
            "fitted_on": "2025-01-01",
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "register curve ({status}): {body}");
    body
}

/// One DOC stream at one site naming no instrument, so the plan proposes one a curve can be
/// attached to.
async fn seed_stream(db: &DatabaseConnection) {
    let stream_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, metadata, is_active, sensor_id) \
             VALUES ('{stream_id}', '{SOURCE}', 'FP1:DOC', 'FP1 - DOC', \
                     '{{\"hierarchy\": {{\"project\": \"Test Project\", \"site\": \"Site 1\", \"parameter\": \"DOC\"}}, \"units\": \"ppb\"}}'::jsonb, true, NULL)"
        ),
    )
    .await;
}

async fn create_plan(app: &axum::Router, token: &str) -> String {
    let (status, plan) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/pairing-plans",
        &json!({ "source_system": SOURCE }),
        token,
    )
    .await;
    assert_eq!(status, 200, "create plan: {plan}");
    plan["id"].as_str().expect("plan id").to_string()
}

async fn plan_instruments(app: &axum::Router, token: &str, plan_id: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/instruments"),
        token,
    )
    .await;
    assert_eq!(status, 200, "plan instruments: {body}");
    body
}

#[tokio::test]
#[serial]
async fn a_curve_registered_before_any_plan_creates_neither_itself_nor_an_instrument() {
    let f = crate::common::seeded_app().await;

    let body = register_curve(&f.app, &f.token, 2.0).await;
    assert_eq!(body["proposed"], json!(true), "{body}");
    assert_eq!(body["id"], json!(null), "no curve exists yet: {body}");
    assert_eq!(
        body["sensor_id"],
        json!(null),
        "no instrument either: {body}"
    );

    assert_eq!(
        count(&f.db, &format!("sensors WHERE source_system = '{SOURCE}'")).await,
        0,
        "a sync mints no instrument"
    );
    assert_eq!(
        count(
            &f.db,
            &format!("standard_curves WHERE source_system = '{SOURCE}'")
        )
        .await,
        0,
        "and no curve"
    );

    // Every cycle re-offers the same curve; the coefficients follow the portal, the row does not
    // multiply.
    register_curve(&f.app, &f.token, 3.0).await;
    let held =
        f.db.query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT slope FROM standard_curve_proposals WHERE source_system = '{SOURCE}'"),
        ))
        .await
        .unwrap();
    assert_eq!(held.len(), 1, "one held row per curve");
    assert!(
        (held[0].try_get::<f64>("", "slope").unwrap() - 3.0).abs() < f64::EPSILON,
        "the latest coefficients are what waits"
    );
}

#[tokio::test]
#[serial]
async fn a_plan_holding_an_unattached_curve_is_refused_naming_it() {
    let f = crate::common::seeded_app().await;
    seed_stream(&f.db).await;
    register_curve(&f.app, &f.token, 2.0).await;
    let plan_id = create_plan(&f.app, &f.token).await;

    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    let held = instruments["held_curves"].as_array().expect("held curves");
    assert_eq!(
        held.len(),
        1,
        "the plan lists the held curve: {instruments}"
    );
    assert_eq!(held[0]["source_key"], json!(CURVE_KEY), "{instruments}");
    assert_eq!(
        held[0]["attached"],
        json!(null),
        "nothing is attached for the operator: {instruments}"
    );

    crate::common::plans::acknowledge_plan(&f.app, &f.token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&f.app, &plan_id, "apply", &f.token).await;
    assert_eq!(status, 400, "an unattached curve blocks the apply: {text}");
    assert!(
        text.contains(&format!("{LABEL} 2025-01-01")),
        "the refusal names the curve: {text}"
    );
    assert_eq!(
        count(&f.db, &format!("sensors WHERE source_system = '{SOURCE}'")).await,
        0,
        "nothing was created"
    );
}

#[tokio::test]
#[serial]
async fn applying_the_plan_creates_the_held_curve_under_the_instrument_it_was_attached_to() {
    let f = crate::common::seeded_app().await;
    seed_stream(&f.db).await;
    register_curve(&f.app, &f.token, 2.0).await;
    // A second curve the source reports no fit date for.
    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/standard_curves/register",
        &json!({
            "source_system": SOURCE,
            "source_key": "standard_curves:18",
            "instrument_label": LABEL,
            "slope": 4.0,
            "intercept": 0.0,
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "register undated curve ({status}): {body}");
    let plan_id = create_plan(&f.app, &f.token).await;

    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    let held: Vec<serde_json::Value> = instruments["held_curves"]
        .as_array()
        .expect("held curves")
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert_eq!(held.len(), 2, "{instruments}");
    let (status, plan) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    let instrument_key = plan["entries"][0]["instrument"]["source_key"].clone();
    assert!(
        instrument_key.is_string(),
        "the DOC entry proposes an instrument: {plan}"
    );

    let (status, patched) = crate::common::patch_json_parse_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({
            "expected_version": plan["version"],
            "updates": [],
            "held_curves": held
                .iter()
                .map(|id| json!({ "proposal_id": id, "instrument_source_key": instrument_key }))
                .collect::<Vec<_>>(),
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "attach the held curve: {patched}");
    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    assert_eq!(
        instruments["held_curves"][0]["attached"]["instrument_source_key"], instrument_key,
        "the plan shows where the curve will be created: {instruments}"
    );

    crate::common::plans::acknowledge_plan(&f.app, &f.token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&f.app, &plan_id, "apply", &f.token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(&f.db, &job_id_of(&text)).await,
        "completed"
    );

    let stored =
        f.db.query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT c.id, c.slope, s.id AS sensor_id, s.source_key AS instrument_key \
                   FROM standard_curves c JOIN sensors s ON s.id = c.sensor_id \
                  WHERE c.source_system = '{SOURCE}' AND c.source_key = '{CURVE_KEY}'"
            ),
        ))
        .await
        .unwrap();
    assert_eq!(stored.len(), 1, "the apply created the curve");
    assert_eq!(
        json!(stored[0].try_get::<String>("", "instrument_key").unwrap()),
        instrument_key,
        "under the instrument the review attached it to"
    );
    assert_eq!(
        count(
            &f.db,
            &format!("standard_curve_proposals WHERE source_system = '{SOURCE}'")
        )
        .await,
        0,
        "a created curve is no longer held"
    );
    assert_eq!(
        count(
            &f.db,
            &format!(
                "standard_curves WHERE source_system = '{SOURCE}' \
                 AND source_key = 'standard_curves:18' AND fitted_on IS NOT NULL"
            )
        )
        .await,
        1,
        "a curve reported with no fit date is stored on its creation date, never on nothing"
    );

    // The next cycle maps the portal's curve onto the stored row, which is what lets a reading
    // name it.
    let body = register_curve(&f.app, &f.token, 2.0).await;
    assert_eq!(body["proposed"], json!(false), "{body}");
    assert_eq!(
        body["id"],
        json!(stored[0].try_get::<Uuid>("", "id").unwrap()),
        "{body}"
    );
    assert_eq!(
        body["sensor_id"],
        json!(stored[0].try_get::<Uuid>("", "sensor_id").unwrap()),
        "{body}"
    );
}

#[tokio::test]
#[serial]
async fn a_sync_service_cannot_declare_the_instrument_a_stream_reports() {
    let f = crate::common::seeded_app().await;
    let (sync_token, _) = crate::common::seed_sync_session_token(&f.db).await;
    let (status, body) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/streams/register",
        &json!({
            "source_system": SOURCE,
            "source_key": "FP1:DOC",
            "metadata": { "hierarchy": { "project": "Test Project", "site": "Site 1", "parameter": "DOC" } },
            "sensor_id": Uuid::new_v4(),
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 403, "the plan decides a feed's instrument: {body}");
}

#[tokio::test]
#[serial]
async fn a_skipped_curve_is_never_stored_and_stops_blocking_the_apply() {
    let f = crate::common::seeded_app().await;
    seed_stream(&f.db).await;
    register_curve(&f.app, &f.token, 2.0).await;
    let plan_id = create_plan(&f.app, &f.token).await;
    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    let proposal_id = instruments["held_curves"][0]["id"].clone();

    let (status, plan) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    let (status, patched) = crate::common::patch_json_parse_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({
            "expected_version": plan["version"],
            "updates": [],
            "held_curves": [{ "proposal_id": proposal_id, "skip": true }],
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "skip the held curve: {patched}");

    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    assert_eq!(
        instruments["held_curves"][0]["skipped"],
        json!(true),
        "the plan shows the curve as left behind: {instruments}"
    );
    assert_eq!(
        instruments["held_curves"][0]["attached"],
        json!(null),
        "and attached to nothing: {instruments}"
    );

    crate::common::plans::acknowledge_plan(&f.app, &f.token, &plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(&f.app, &plan_id, "apply", &f.token).await;
    assert!(
        (200..300).contains(&status),
        "a skipped curve does not block the apply ({status}): {text}"
    );
    assert_eq!(
        crate::common::jobs::wait_for_job(&f.db, &job_id_of(&text)).await,
        "completed"
    );
    assert_eq!(
        count(
            &f.db,
            &format!("standard_curves WHERE source_system = '{SOURCE}'")
        )
        .await,
        0,
        "a skipped curve is not stored"
    );

    // The source keeps offering it, and the answer tells it to stop sending the readings that
    // name it.
    let body = register_curve(&f.app, &f.token, 2.0).await;
    assert_eq!(body["proposed"], json!(true), "{body}");
    assert_eq!(body["skipped"], json!(true), "{body}");
    assert_eq!(body["id"], json!(null), "still nothing stored: {body}");
}

#[tokio::test]
#[serial]
async fn attaching_a_skipped_curve_takes_the_skip_back() {
    let f = crate::common::seeded_app().await;
    seed_stream(&f.db).await;
    register_curve(&f.app, &f.token, 2.0).await;
    let plan_id = create_plan(&f.app, &f.token).await;
    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    let proposal_id = instruments["held_curves"][0]["id"].clone();

    let (status, plan) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    let instrument_key = plan["entries"][0]["instrument"]["source_key"].clone();
    let (status, patched) = crate::common::patch_json_parse_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({
            "expected_version": plan["version"],
            "updates": [],
            "held_curves": [
                { "proposal_id": proposal_id, "skip": true },
                { "proposal_id": proposal_id, "instrument_source_key": instrument_key },
            ],
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "skip then attach: {patched}");

    let instruments = plan_instruments(&f.app, &f.token, &plan_id).await;
    assert_eq!(
        instruments["held_curves"][0]["skipped"],
        json!(false),
        "attaching a curve is the opposite of leaving it behind: {instruments}"
    );
    assert_eq!(
        instruments["held_curves"][0]["attached"]["instrument_source_key"], instrument_key,
        "{instruments}"
    );

    // The two decisions are exclusive in one update too.
    let (status, plan) = crate::common::get_json_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &f.token,
    )
    .await;
    assert_eq!(status, 200, "{plan}");
    let (status, refused) = crate::common::patch_json_with_token(
        &f.app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &json!({
            "expected_version": plan["version"],
            "updates": [],
            "held_curves": [{
                "proposal_id": proposal_id,
                "instrument_source_key": instrument_key,
                "skip": true,
            }],
        }),
        &f.token,
    )
    .await;
    assert_eq!(status, 400, "skipped or attached, not both: {refused}");
}
