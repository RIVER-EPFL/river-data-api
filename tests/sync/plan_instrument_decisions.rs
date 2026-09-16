//! The instrument half of a pairing plan: which feeds are asked about, what a proposal survives,
//! and what a device-shaped feed resolves to.
//!
//! Expected behaviour: a feed the source identifies as a device is not offered a lab instrument,
//! because its instrument is minted from the feed itself and pairing opens the site slot's
//! deployment. One instrument serves one (site, parameter): a multi-channel logger is that many
//! instruments, identified by each channel's `(source_system, source_key)` rather than by the
//! logger serial, which is information the source reports and not an identity. A feed with no
//! device is asked once, and the answer is remembered: the proposal survives an attach so a
//! mis-click is undoable, and a later plan reports the instrument the earlier one created rather
//! than proposing it again.

use sea_orm::{ConnectionTrait, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::pairing_plan_apply::job_id_of;

const SOURCE: &str = "instrdec";

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let f = crate::common::seeded_app().await;
    (f.app, f.token, f.db)
}

/// A feed carrying the hierarchy a plan reads plus the device identity the source reports.
///
/// `sensor_id` is set NULL against the suite's fixture default: what these tests are about is the
/// instrument the feed itself resolves to, which a stream already naming one never reaches.
async fn seed_device_stream(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
    source_key: &str,
    parameter: &str,
    serial: &str,
    model: &str,
) {
    let metadata = format!(
        "{{\"hierarchy\": {{\"project\": \"Test River Project\", \"site\": \"Upstream Station\", \
          \"parameter\": \"{parameter}\"}}, \"units\": \"°C\", \
          \"device\": {{\"logger_serial\": \"{serial}\", \"logger_device\": \"{model}\"}}}}"
    );
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO data_streams \
             (id, source_system, source_key, source_name, metadata, is_active, sensor_id) \
             VALUES ('{stream_id}', '{SOURCE}', '{source_key}', '{source_key}', \
                     '{metadata}'::jsonb, true, NULL)"
        ),
    )
    .await;
}

/// A lab-shaped feed carrying no instrument. The suite defaults `data_streams.sensor_id` to the
/// fixture instrument, which would answer the question these tests put to the plan.
async fn seed_lab_stream(
    db: &sea_orm::DatabaseConnection,
    stream_id: Uuid,
    source_key: &str,
    site: &str,
) {
    crate::common::seed_unpaired_stream_with_hierarchy(
        db,
        &stream_id.to_string(),
        SOURCE,
        source_key,
        "Test River Project",
        site,
        "doc",
        "ppb",
        None,
        0,
    )
    .await;
    crate::common::exec(
        db,
        &format!("UPDATE data_streams SET sensor_id = NULL WHERE id = '{stream_id}'"),
    )
    .await;
}

async fn create_plan(app: &axum::Router, token: &str) -> serde_json::Value {
    let (status, plan) = crate::common::post_json_parse_with_token(
        app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": SOURCE }),
        token,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    plan
}

async fn plan_instruments(app: &axum::Router, token: &str, plan_id: &str) -> serde_json::Value {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/instruments"),
        token,
    )
    .await;
    assert_eq!(status, 200, "plan instruments ({status}): {body}");
    body
}

async fn patch_entry(
    app: &axum::Router,
    token: &str,
    plan_id: &str,
    update: serde_json::Value,
) -> (u16, String) {
    crate::common::patch_plan_with_token(
        app,
        &plan_id.to_string(),
        &serde_json::json!({ "updates": [update] }),
        token,
    )
    .await
}

fn entry_for(plan: &serde_json::Value, stream_id: Uuid) -> serde_json::Value {
    plan["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["stream_id"] == serde_json::json!(stream_id))
        .cloned()
        .unwrap_or_else(|| panic!("no entry for {stream_id}"))
}

async fn scalar_i64(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<i64>("", "v")
    .expect("value")
}

async fn apply_and_wait(
    app: &axum::Router,
    db: &sea_orm::DatabaseConnection,
    token: &str,
    plan_id: &str,
) {
    // The review gate wants every row ticked; these tests are about the instruments the apply binds.
    crate::common::plans::acknowledge_plan(app, token, plan_id).await;
    let (status, text) =
        crate::common::post_plan_action_with_token(app, &plan_id.to_string(), "apply", token).await;
    assert!((200..300).contains(&status), "apply ({status}): {text}");
    assert_eq!(
        crate::common::jobs::wait_for_job(db, &job_id_of(&text)).await,
        "completed"
    );
}

#[tokio::test]
#[serial]
async fn a_device_feed_is_reported_per_channel_and_never_offered_a_lab_instrument() {
    let (app, token, db) = setup().await;
    let temp = Uuid::new_v4();
    let oxygen = Uuid::new_v4();
    seed_device_stream(&db, temp, "dev-temp", "water temperature", "LOG-1", "AQ600").await;
    seed_device_stream(&db, oxygen, "dev-do", "dissolved oxygen", "LOG-1", "AQ600").await;

    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    assert_eq!(
        entry_for(&plan, temp)["device_serial"],
        serde_json::json!("LOG-1"),
        "the plan carries the device the source names"
    );

    let instruments = plan_instruments(&app, &token, &plan_id).await;
    assert_eq!(
        instruments["unassigned"].as_array().map(Vec::len),
        Some(0),
        "a device feed is not a lab-instrument question: {instruments}"
    );
    let devices = instruments["devices"].as_array().expect("devices");
    assert_eq!(
        devices.len(),
        2,
        "one logger, two channels, an instrument each: {instruments}"
    );
    for device in devices {
        assert_eq!(
            device["serial"],
            serde_json::json!("LOG-1"),
            "the logger serial is reported as information: {instruments}"
        );
        assert_eq!(
            device["parameters"].as_array().map(Vec::len),
            Some(1),
            "a channel serves one parameter: {instruments}"
        );
    }

    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({
            "stream_id": temp,
            "instrument_name": "Water Temperature instrdec",
            "instrument_confirmed": true,
        }),
    )
    .await;
    assert_eq!(
        status, 400,
        "naming a lab instrument for a device feed is refused: {body}"
    );
    assert!(
        body.contains("LOG-1"),
        "the refusal names the device it resolves to instead: {body}"
    );

    apply_and_wait(&app, &db, &token, &plan_id).await;

    let sensors = scalar_i64(
        &db,
        "SELECT count(*) AS v FROM sensors \
         WHERE source_system = 'instrdec' AND source_key IN ('dev-temp', 'dev-do')",
    )
    .await;
    assert_eq!(sensors, 2, "each channel resolves to its own instrument");
    let serials = scalar_i64(
        &db,
        "SELECT count(*) AS v FROM sensors \
         WHERE source_system = 'instrdec' AND source_key IN ('dev-temp', 'dev-do') \
           AND serial_number IS NOT NULL",
    )
    .await;
    assert_eq!(
        serials, 0,
        "the logger serial is not the instrument's serial, so none is claimed"
    );
    let deployments = scalar_i64(
        &db,
        "SELECT count(*) AS v FROM sensor_deployments d JOIN sensors s ON s.id = d.sensor_id \
         WHERE s.source_system = 'instrdec' AND s.source_key IN ('dev-temp', 'dev-do') \
           AND d.deployed_until IS NULL",
    )
    .await;
    assert_eq!(
        deployments, 2,
        "each instrument is stationed at the slot it serves"
    );

    // A further channel on the same logger. The two already paired drop out of the plan, and the
    // new one is its own instrument decision rather than an attachment to the logger's existing
    // row: one instrument serves one (site, parameter).
    seed_device_stream(
        &db,
        Uuid::new_v4(),
        "dev-cond",
        "conductivity",
        "LOG-1",
        "AQ600",
    )
    .await;
    let plan = create_plan(&app, &token).await;
    let instruments = plan_instruments(&app, &token, plan["id"].as_str().expect("plan id")).await;
    assert_eq!(
        instruments["groups"].as_array().map(Vec::len),
        Some(0),
        "a device is not also a lab decision: {instruments}"
    );
    let devices = instruments["devices"].as_array().expect("devices");
    assert_eq!(
        devices.len(),
        1,
        "only the unpaired channel is still to decide: {instruments}"
    );
    assert_eq!(
        devices[0]["parameters"],
        serde_json::json!(["conductivity"]),
        "and it is the new one: {instruments}"
    );
    assert_eq!(
        devices[0]["instrument_id"],
        serde_json::Value::Null,
        "a new channel on a known logger is a new instrument, not the logger's existing row: \
         {instruments}"
    );
}

#[tokio::test]
#[serial]
async fn a_device_instrument_is_named_after_the_slot_it_serves() {
    let (app, token, db) = setup().await;
    seed_device_stream(
        &db,
        Uuid::new_v4(),
        "dev-temp",
        "water temperature",
        "LOG-2",
        "AQ600",
    )
    .await;
    seed_device_stream(
        &db,
        Uuid::new_v4(),
        "dev-do",
        "dissolved oxygen",
        "LOG-2",
        "AQ600",
    )
    .await;

    let plan = create_plan(&app, &token).await;
    apply_and_wait(&app, &db, &token, plan["id"].as_str().expect("plan id")).await;

    let names = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT name AS v FROM sensors WHERE source_system = 'instrdec' \
               AND source_key IN ('dev-temp', 'dev-do') ORDER BY name"
                .to_owned(),
        ))
        .await
        .expect("query")
        .iter()
        .map(|r| r.try_get::<String>("", "v").expect("name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "Upstream Station Dissolved Oxygen".to_string(),
            "Upstream Station Water Temperature".to_string(),
        ],
        "each channel of a multi-channel logger is named by the slot it serves, not by the logger"
    );
}

/// Scenario: a lab feed that reached the database the way production feeds do, through
/// `/streams/register`, which mints nothing (M172).
///
/// Expected behaviour: the plan is where the instrument is decided. The entry carries the
/// proposal the apply will mint, waiting for a person to confirm it (Q195), rather than an
/// instrument registration had already chosen. Every other test in this suite seeds by hand, so
/// this is the only one that exercises what the production path actually leaves behind.
#[tokio::test]
#[serial]
async fn a_registered_feed_reaches_the_plan_as_a_proposal() {
    let (app, token, db) = setup().await;

    let (status, registered) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &serde_json::json!({
            "source_system": SOURCE,
            "source_key": "registered-doc",
            "source_name": "registered-doc",
            "measurement_type": "spot",
            "metadata": {
                "hierarchy": {
                    "project": "Test River Project",
                    "site": "Upstream Station",
                    "parameter": "doc",
                },
                "units": "ppb",
            },
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register ({status}): {registered}"
    );
    let stream = registered["id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .unwrap_or_else(|| panic!("no stream id: {registered}"));

    // Registration attached none: the plan is the only writer of instruments.
    let attached = scalar_i64(
        &db,
        &format!(
            "SELECT count(*)::bigint AS v FROM data_streams \
              WHERE id = '{stream}' AND sensor_id IS NOT NULL"
        ),
    )
    .await;
    assert_eq!(attached, 0, "registration attached no instrument");

    let plan = create_plan(&app, &token).await;
    let entry = entry_for(&plan, stream);
    assert_eq!(
        entry["instrument"]["confirmed"],
        serde_json::json!(false),
        "the plan suggests; nobody has confirmed it yet: {entry}"
    );
    // Proposed, not resolved: nothing has minted one, so the entry names what the apply will
    // create, keyed on the source's parameter.
    assert_eq!(
        entry["instrument"]["resolved_by"],
        serde_json::json!("parameter"),
        "the instrument is the plan's own proposal: {entry}"
    );
    assert_eq!(entry["instrument"]["create"], serde_json::json!(true));
    assert!(
        entry["instrument"]["id"].is_null() && entry["instrument"]["proposed_name"].is_string(),
        "it names no existing row and carries the name it proposes: {entry}"
    );

    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let instruments = plan_instruments(&app, &token, &plan_id).await;
    assert_eq!(
        instruments["unassigned"].as_array().map(Vec::len),
        Some(0),
        "nothing is left unassigned, which is Option A: {instruments}"
    );
}

#[tokio::test]
#[serial]
async fn an_attached_instrument_returns_to_the_plan_s_own_proposal() {
    let (app, token, db) = setup().await;
    let stream = Uuid::new_v4();
    seed_lab_stream(&db, stream, "lab-doc", "Upstream Station").await;

    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();

    let instruments = plan_instruments(&app, &token, &plan_id).await;
    assert_eq!(
        instruments["unassigned"].as_array().map(Vec::len),
        Some(0),
        "a parameter with no instrument is proposed, not left unassigned: {instruments}"
    );
    let proposed = entry_for(&plan, stream);
    assert_eq!(
        proposed["instrument"]["resolved_by"],
        serde_json::json!("parameter")
    );
    assert_eq!(proposed["instrument"]["create"], serde_json::json!(true));
    assert_eq!(
        proposed["instrument"]["confirmed"],
        serde_json::json!(false)
    );
    assert_eq!(
        proposed["instrument"]["proposed_name"],
        serde_json::json!("doc"),
        "the proposal is the analyte; the source is provenance and lives in source_key (M130)"
    );

    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({
            "stream_id": stream,
            "instrument_name": "doc instrdec",
            "instrument_confirmed": true,
        }),
    )
    .await;
    assert!((200..300).contains(&status), "propose ({status}): {body}");

    let existing = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument) \
             VALUES ('{existing}', 'Shimadzu TOC-L', true, true)"
        ),
    )
    .await;

    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({ "stream_id": stream, "instrument_id": existing }),
    )
    .await;
    assert!((200..300).contains(&status), "attach ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).expect("plan json");
    let attached = entry_for(&plan, stream);
    assert_eq!(attached["instrument"]["id"], serde_json::json!(existing));
    assert_eq!(
        attached["instrument"]["proposed_name"],
        serde_json::json!("doc instrdec"),
        "attaching an instrument keeps what the plan proposed: {attached}"
    );

    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({
            "stream_id": stream,
            "instrument_name": "doc instrdec",
            "instrument_confirmed": true,
        }),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "re-propose ({status}): {body}"
    );
    let plan: serde_json::Value = serde_json::from_str(&body).expect("plan json");
    let reverted = entry_for(&plan, stream);
    assert!(
        reverted["instrument"]["id"].is_null(),
        "naming an instrument returns the entry to a proposal: {reverted}"
    );
    assert_eq!(
        reverted["instrument"]["create"],
        serde_json::json!(true),
        "and that proposal is what the apply will create: {reverted}"
    );
    assert_eq!(
        reverted["instrument"]["name"],
        serde_json::json!("doc instrdec")
    );
}

#[tokio::test]
#[serial]
async fn a_later_plan_reports_the_instrument_an_earlier_one_created() {
    let (app, token, db) = setup().await;
    let first = Uuid::new_v4();
    seed_lab_stream(&db, first, "lab-doc-1", "Upstream Station").await;

    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({
            "stream_id": first,
            "instrument_name": "doc instrdec",
            "instrument_confirmed": true,
        }),
    )
    .await;
    assert!((200..300).contains(&status), "propose ({status}): {body}");
    apply_and_wait(&app, &db, &token, &plan_id).await;

    let second = Uuid::new_v4();
    seed_lab_stream(&db, second, "lab-doc-2", "Downstream Station").await;

    let plan = create_plan(&app, &token).await;
    let entry = entry_for(&plan, second);
    assert_eq!(
        entry["instrument"]["create"],
        serde_json::json!(false),
        "the instrument the first plan created is the answer, not a new question: {entry}"
    );
    assert_eq!(
        entry["instrument"]["name"],
        serde_json::json!("doc instrdec")
    );
    assert_eq!(
        plan["summary"]["instruments_to_create"],
        serde_json::json!(0),
        "and the consequence the confirm step states is what the apply will do: {}",
        plan["summary"]
    );
}

/// Scenario: a plan proposes an instrument, and a replicated curve sits on the wrong instrument.
/// The review assigns the curve to the proposed one before the plan is applied.
///
/// Expected behaviour: the plan carries the assignment, the instruments view reports it as
/// pending, and apply moves the curve in the same transaction that mints the instrument, so the
/// curve lands on the row the plan created. A curve readings already name is refused, as the
/// curve's own update route refuses it.
#[tokio::test]
#[serial]
async fn a_curve_assigned_to_a_proposed_instrument_moves_when_the_plan_is_applied() {
    let (app, token, db) = setup().await;
    let stream = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream.to_string(),
        SOURCE,
        "lab-doc-curve",
        "Test River Project",
        "Upstream Station",
        "doc",
        "ppb",
        None,
        0,
    )
    .await;
    let wrong_home = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, source_system, source_key) \
             VALUES ('{wrong_home}', 'Placeholder', true, true, '{SOURCE}', '{SOURCE}:placeholder')"
        ),
    )
    .await;
    let curve = Uuid::new_v4();
    let used_curve = Uuid::new_v4();
    for (id, name) in [(curve, "DOC 2024-03-01"), (used_curve, "DOC 2023-11-01")] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO standard_curves (id, sensor_id, name, slope, intercept, source_system, source_key) \
                 VALUES ('{id}', '{wrong_home}', '{name}', 1.5, 0.2, '{SOURCE}', '{SOURCE}:{name}')"
            ),
        )
        .await;
    }
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO readings (stream_id, time, replicate_index, raw_value, standard_curve_id) \
             VALUES ('{stream}', now() - interval '1 day', 0, 4.0, '{used_curve}')"
        ),
    )
    .await;

    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({
            "stream_id": stream,
            "instrument_name": "doc curvehome",
            "instrument_confirmed": true,
        }),
    )
    .await;
    assert!((200..300).contains(&status), "propose ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).expect("plan json");
    let proposed = entry_for(&plan, stream);
    assert_eq!(proposed["instrument"]["create"], serde_json::json!(true));
    let source_key = proposed["instrument"]["source_key"]
        .as_str()
        .expect("proposed instrument source_key")
        .to_string();

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({
            "updates": [],
            "curves": [{ "curve_id": used_curve, "instrument_source_key": source_key }],
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a curve readings already name keeps its instrument: {body}"
    );

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id.to_string(),
        &serde_json::json!({
            "updates": [],
            "curves": [{ "curve_id": curve, "instrument_source_key": source_key }],
        }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "assign curve ({status}): {body}"
    );

    let instruments = plan_instruments(&app, &token, &plan_id).await;
    let listed = instruments["curves"]
        .as_array()
        .expect("curves")
        .iter()
        .find(|c| c["id"] == serde_json::json!(curve))
        .cloned()
        .expect("the curve is listed");
    assert_eq!(
        listed["pending_source_key"],
        serde_json::json!(source_key),
        "the review shows where the curve will go: {listed}"
    );
    assert_eq!(
        listed["pending_instrument_name"],
        serde_json::json!("doc curvehome"),
        "{listed}"
    );
    let used = instruments["curves"]
        .as_array()
        .expect("curves")
        .iter()
        .find(|c| c["id"] == serde_json::json!(used_curve))
        .cloned()
        .expect("the used curve is listed");
    assert_eq!(
        (
            &used["corrected_parameters"],
            &used["corrected_sites"],
            &used["reading_count"]
        ),
        (
            &serde_json::json!(["doc"]),
            &serde_json::json!(["Upstream Station"]),
            &serde_json::json!(1)
        ),
        "the review says which readings the curve corrects: {used}"
    );
    assert!(used["first_corrected"].is_string(), "{used}");

    apply_and_wait(&app, &db, &token, &plan_id).await;

    let minted = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id FROM sensors WHERE source_system = $1 AND source_key = $2",
            [SOURCE.into(), source_key.clone().into()],
        ))
        .await
        .expect("query")
        .expect("the plan minted the instrument")
        .try_get::<Uuid>("", "id")
        .expect("id");
    let home = db
        .query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT sensor_id FROM standard_curves WHERE id = $1",
            [curve.into()],
        ))
        .await
        .expect("query")
        .expect("curve row")
        .try_get::<Uuid>("", "sensor_id")
        .expect("sensor_id");
    assert_eq!(
        home, minted,
        "the curve moved onto the instrument the apply minted"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!("SELECT count(*) AS v FROM standard_curves WHERE sensor_id = '{wrong_home}'")
        )
        .await,
        1,
        "the used curve stayed where it was"
    );
}

/// The plan proposes the instrument the pairing mints, for a replicate family too.
///
/// A family's suggested parameter is the measurand (`DOC_avg_ppb` reads as `DOC_ppb`), which is a
/// label; keying the proposal on it would offer a second instrument for an analyte the pairing
/// already holds one for.
#[tokio::test]
#[serial]
async fn a_family_entry_proposes_the_instrument_key_the_pairing_mints() {
    let (app, token, db) = setup().await;
    let stream_id = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream_id.to_string(),
        SOURCE,
        "FP3:DOC_avg_ppb:reps",
        "Test River Project",
        "Family Station",
        "DOC_avg_ppb",
        "ppb",
        None,
        0,
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET sensor_id = NULL,              metadata = metadata || '{{\"replicates\": {{\"source_columns\":              [\"DOC_rep_A\", \"DOC_rep_B\"]}}}}'::jsonb WHERE id = '{stream_id}'"
        ),
    )
    .await;

    let plan = create_plan(&app, &token).await;
    let entry = entry_for(&plan, stream_id);

    assert_ne!(
        entry["parameter"]["name"],
        serde_json::json!("DOC_avg_ppb"),
        "the family's suggested parameter is the measurand, not the statistic column: {entry}"
    );
    assert_eq!(
        entry["instrument"]["source_key"],
        serde_json::json!(format!("{SOURCE}:DOC_avg_ppb")),
        "the proposal keys on the stream, not on the suggestion: {entry}"
    );
}

/// One instrument serves every station measuring its parameter, so its proposed name is one name.
/// Renaming it on any entry renames it on all of them; a name held per entry would mean the apply
/// mints whichever of the twenty-three the plan happened to read first, and the other twenty-two
/// display a name no row carries (B180).
#[tokio::test]
#[serial]
async fn renaming_a_proposal_renames_every_entry_resolving_to_it() {
    let (app, token, db) = setup().await;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    seed_lab_stream(&db, first, "FP1:DOC", "Station One").await;
    seed_lab_stream(&db, second, "FP2:DOC", "Station Two").await;
    seed_lab_stream(&db, third, "FP3:DOC", "Station Three").await;

    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let key = entry_for(&plan, first)["instrument"]["source_key"].clone();
    for stream in [second, third] {
        assert_eq!(
            entry_for(&plan, stream)["instrument"]["source_key"],
            key,
            "every station's DOC column resolves to one instrument: {plan}"
        );
    }

    let (status, body) = patch_entry(
        &app,
        &token,
        &plan_id,
        serde_json::json!({
            "stream_id": first,
            "instrument_name": "DOC analyser",
            "instrument_confirmed": true,
        }),
    )
    .await;
    assert_eq!(status, 200, "rename ({status}): {body}");

    let renamed: serde_json::Value = serde_json::from_str(&body).expect("the plan comes back");
    for stream in [first, second, third] {
        let instrument = entry_for(&renamed, stream)["instrument"].clone();
        assert_eq!(
            instrument["name"],
            serde_json::json!("DOC analyser"),
            "entry {stream} still shows the old name: {instrument}"
        );
        assert_eq!(
            instrument["proposed_name"],
            serde_json::json!("DOC analyser"),
            "and its proposal is the renamed one: {instrument}"
        );
    }

    let listed = plan_instruments(&app, &token, &plan_id).await;
    let named: Vec<&str> = listed["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .filter_map(|g| g["name"].as_str())
        .collect();
    assert!(
        named.contains(&"DOC analyser"),
        "the tab lists the renamed instrument once: {listed}"
    );
}

/// The lab's analyser is one machine carried to every station, so it is called `DOC`. When an
/// instrument of that name already exists, the plan reports the collision and leaves the proposal
/// unconfirmed: attaching would add these readings to a row that already holds data, and apply
/// refuses an unconfirmed proposal, so the operator has to say which they meant (M130).
#[tokio::test]
#[serial]
async fn a_proposal_colliding_with_an_existing_instrument_is_reported_and_left_undecided() {
    let (app, token, db) = setup().await;
    let existing = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sensors (id, name, is_active, is_lab_instrument, data_frequency, created_at) \
             VALUES ('{existing}', 'doc', true, true, 'low', now())"
        ),
    )
    .await;

    let stream = Uuid::new_v4();
    seed_lab_stream(&db, stream, "FP1:DOC", "Collision Station").await;
    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();

    let instrument = entry_for(&plan, stream)["instrument"].clone();
    assert_eq!(
        instrument["confirmed"],
        serde_json::json!(false),
        "a collision is never agreed to on the operator's behalf: {instrument}"
    );
    assert_eq!(
        instrument["name_conflict"]["id"],
        serde_json::json!(existing),
        "the plan names the instrument it collides with: {instrument}"
    );
    assert_eq!(
        instrument["name_conflict"]["has_readings"],
        serde_json::json!(false),
        "and whether attaching would add to readings it already holds: {instrument}"
    );

    let listed = plan_instruments(&app, &token, &plan_id).await;
    let group = listed["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["name"] == serde_json::json!("doc"))
        .cloned()
        .unwrap_or_else(|| panic!("no group for the proposal: {listed}"));
    assert_eq!(
        group["name_conflict"]["id"],
        serde_json::json!(existing),
        "the tab carries the collision too, so the choice is where the decision is made: {group}"
    );
}

/// Scenario: a first plan over two lab feeds, every row ticked and no instrument touched.
///
/// Expected behaviour: the plan suggests and a person confirms (Q195). The apply is refused
/// naming the feeds whose suggestion nobody accepted, and one PATCH confirming every asking row,
/// which is what the review's bulk accept sends, lets it through and mints both.
#[tokio::test]
#[serial]
async fn an_untouched_plan_is_refused_until_its_suggestions_are_accepted() {
    let (app, token, db) = setup().await;
    let doc = Uuid::new_v4();
    let tss = Uuid::new_v4();
    seed_lab_stream(&db, doc, "untouched-doc", "Upstream Station").await;
    seed_lab_stream(&db, tss, "untouched-tss", "Downstream Station").await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE data_streams SET metadata = jsonb_set(metadata, '{{hierarchy,parameter}}', '\"tss\"') \
             WHERE id = '{tss}'"
        ),
    )
    .await;

    let plan = create_plan(&app, &token).await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let ticks: Vec<serde_json::Value> = [doc, tss]
        .iter()
        .map(|id| serde_json::json!({ "stream_id": id, "acknowledged": true }))
        .collect();
    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({ "updates": ticks }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "tick ({status}): {body}");

    let (status, refused) =
        crate::common::post_plan_action_with_token(&app, &plan_id, "apply", &token).await;
    assert_eq!(
        status, 400,
        "an unconfirmed suggestion holds the apply: {refused}"
    );
    assert!(
        refused.contains("untouched-doc") && refused.contains("untouched-tss"),
        "the refusal names each feed still asking: {refused}"
    );

    let accept: Vec<serde_json::Value> = [doc, tss]
        .iter()
        .map(|id| serde_json::json!({ "stream_id": id, "instrument_confirmed": true }))
        .collect();
    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({ "updates": accept }),
        &token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "accept all ({status}): {body}"
    );

    let before = scalar_i64(&db, "SELECT count(*)::bigint AS v FROM sensors").await;
    apply_and_wait(&app, &db, &token, &plan_id).await;
    let after = scalar_i64(&db, "SELECT count(*)::bigint AS v FROM sensors").await;
    assert_eq!(after - before, 2, "each accepted suggestion is minted once");
}
