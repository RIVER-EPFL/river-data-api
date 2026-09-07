//! `POST /streams/register` accepting the instrument that produces a feed.
//!
//! Expected behaviour: a declared instrument is stored on the stream, so pairing reuses it instead
//! of minting a second, serial-less one; and a caller cannot name an instrument the feed has no
//! relationship to.
//!
//! The confinement rule is asserted against the guard itself rather than over HTTP: the route
//! already refuses project-scoped tokens outright, so the restricted principal the guard exists for
//! cannot be produced by a request.

use sea_orm::TransactionTrait;
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::sensor_lifecycle::{
    create_sensor as create_inventory_sensor, deploy_sensor, dt,
};
use crate::common::{GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};
use river_db::common::authz::AccessScope;
use river_db::error::AppError;
use river_db::routes::private::data_streams::views::validate_declared_sensor;

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let f = crate::common::seeded_app().await;
    (f.app, f.token, f.db)
}

async fn create_sensor(app: &axum::Router, token: &str, serial: &str) -> String {
    let (status, json) = crate::common::post_json_parse_with_token(
        app,
        "/api/sensors",
        &json!({ "serial_number": serial, "manufacturer": "test", "model": "test" }),
        token,
    )
    .await;
    assert_eq!(status, 201, "create sensor ({status}): {json}");
    json["id"].as_str().expect("sensor id").to_string()
}

async fn sensor_count(db: &sea_orm::DatabaseConnection) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT count(*) AS c FROM sensors".to_string(),
    ))
    .await
    .expect("count sensors")
    .expect("one row")
    .try_get::<i64>("", "c")
    .expect("count column")
}

#[tokio::test]
#[serial]
async fn register_attaches_the_declared_instrument_and_pairing_reuses_it() {
    let (app, token, db) = setup().await;
    let sensor_id = create_sensor(&app, &token, "REG-0001").await;
    let before = sensor_count(&db).await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "declare-1",
            "source_name": "Declared feed",
            "sensor_id": sensor_id,
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {stream}");
    assert_eq!(
        stream["sensor_id"].as_str(),
        Some(sensor_id.as_str()),
        "the declared instrument is stored on the stream: {stream}"
    );

    let stream_id = stream["id"].as_str().expect("stream id");
    let (status, paired) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({ "site_parameter_id": crate::common::PARAM_S1_TEMP_ID }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "pair ({status}): {paired}");
    assert_eq!(
        sensor_count(&db).await,
        before,
        "pairing reuses the declared instrument instead of minting a second one"
    );
}

/// Scenario: a source registers a channel and names no instrument.
/// Expected behaviour: the feed is attached to the source's own instrument for the parameter it
/// carries, so nothing it later writes can be a measurement of nothing.
#[tokio::test]
#[serial]
async fn register_omitting_the_instrument_attaches_the_source_parameter_instrument() {
    let (app, token, db) = setup().await;

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "no-sensor",
            "metadata": { "hierarchy": { "site": "S1", "parameter": "Temperature" } },
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {stream}");
    let sensor_id = stream["sensor_id"]
        .as_str()
        .expect("an omitted instrument is minted, not left null");

    let row = {
        use sea_orm::{ConnectionTrait, Statement};
        db.query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT source_key FROM sensors WHERE id = $1::uuid",
            [sensor_id.into()],
        ))
        .await
        .expect("query")
        .expect("the minted instrument exists")
    };
    assert_eq!(
        row.try_get::<String>("", "source_key").expect("source_key"),
        "declare:Temperature",
        "the instrument is keyed on the source and the parameter, one per parameter across sites"
    );

    // A second channel of the same parameter at another station resolves the same instrument.
    let (status, sibling) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "no-sensor-2",
            "metadata": { "hierarchy": { "site": "S2", "parameter": "Temperature" } },
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "register ({status}): {sibling}");
    assert_eq!(
        sibling["sensor_id"].as_str(),
        Some(sensor_id),
        "one instrument per (source, parameter): {sibling}"
    );
}

#[tokio::test]
#[serial]
async fn register_rejects_an_instrument_that_does_not_exist() {
    let (app, token, _db) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "ghost",
            "sensor_id": "00000000-0000-4000-f000-0000000000ff",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 404, "unknown instrument ({status}): {body}");
}

#[tokio::test]
#[serial]
async fn register_rejects_an_instrument_the_metadata_contradicts() {
    let (app, token, _db) = setup().await;
    let sensor_id = create_sensor(&app, &token, "REG-0002").await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "declare",
            "source_key": "mismatch",
            "metadata": { "device": { "logger_serial": "SOMEONE-ELSE" } },
            "sensor_id": sensor_id,
        }),
        &token,
    )
    .await;
    assert_eq!(
        status, 400,
        "a feed reporting another device's serial cannot claim this instrument ({status}): {body}"
    );
}

#[tokio::test]
#[serial]
async fn re_registering_is_idempotent_but_will_not_move_the_instrument() {
    let (app, token, _db) = setup().await;
    let first = create_sensor(&app, &token, "REG-0003").await;
    let second = create_sensor(&app, &token, "REG-0004").await;

    let body = |sensor: &str| {
        json!({
            "source_system": "declare",
            "source_key": "stable",
            "sensor_id": sensor,
        })
    };

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &body(&first),
        &token,
    )
    .await;
    assert_eq!(status, 200, "first register ({status}): {stream}");

    let (status, stream) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &body(&first),
        &token,
    )
    .await;
    assert_eq!(
        status, 200,
        "re-register with the same instrument: {stream}"
    );
    assert_eq!(stream["sensor_id"].as_str(), Some(first.as_str()));

    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/streams/register", &body(&second), &token)
            .await;
    assert_eq!(
        status, 409,
        "reattributing an established feed is refused ({status}): {body}"
    );
}

const OTHER_PROJECT_ID: &str = "00000000-0000-4000-a000-0000000000d1";
const OTHER_SITE_ID: &str = "00000000-0000-4000-a000-0000000000d2";

/// A project the caller below holds no grant for, to deploy an instrument into.
async fn seed_other_project(db: &sea_orm::DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, description, data_source) \
             VALUES ('{OTHER_PROJECT_ID}', 'Declared Other', 'second project', 'test')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
             VALUES ('{OTHER_SITE_ID}', '{OTHER_PROJECT_ID}', 'Other Station', 46.0, 7.0, 500.0)"
        ),
    )
    .await;
}

fn feed_without_a_serial() -> serde_json::Value {
    json!({})
}

/// A feed that describes no device cannot contradict anything, so the serial cross-check never
/// fires and the project confinement is the whole rule. An instrument deployed nowhere belongs to
/// no project, and attaching one would resolve its calibration windows onto everything the feed
/// writes.
#[tokio::test]
#[serial]
async fn a_confined_caller_may_only_name_an_instrument_its_own_projects_deploy() {
    let (_app, _token, db) = setup().await;
    seed_other_project(&db).await;
    let scope = AccessScope::one(PROJECT_ID.parse::<Uuid>().expect("project id is a uuid"));
    let metadata = feed_without_a_serial();
    let from = dt("2025-01-01T00:00:00Z");

    let inventory = create_inventory_sensor(&db, "declared-inventory", GLOBAL_PARAM_TEMP_ID).await;
    assert!(
        matches!(
            validate_declared_sensor(&db, &scope, inventory.id, &metadata).await,
            Err(AppError::Forbidden(_))
        ),
        "an instrument deployed nowhere is not this caller's to claim"
    );

    let elsewhere = create_inventory_sensor(&db, "declared-elsewhere", GLOBAL_PARAM_TEMP_ID).await;
    deploy_sensor(&db, elsewhere.id, OTHER_SITE_ID, from).await;
    assert!(
        matches!(
            validate_declared_sensor(&db, &scope, elsewhere.id, &metadata).await,
            Err(AppError::Forbidden(_))
        ),
        "another project's instrument is refused even though the feed names no serial"
    );

    let own = create_inventory_sensor(&db, "declared-own", GLOBAL_PARAM_TEMP_ID).await;
    deploy_sensor(&db, own.id, SITE1_ID, from).await;
    assert!(
        validate_declared_sensor(&db, &scope, own.id, &metadata)
            .await
            .is_ok(),
        "an instrument deployed into the caller's own project is claimable"
    );
}

/// Wiring inventory to its first feed is the discovery case, and it stays open to the callers that
/// span projects: an administrator and an unscoped sync service.
#[tokio::test]
#[serial]
async fn an_unrestricted_caller_still_claims_undeployed_inventory() {
    let (_app, _token, db) = setup().await;
    let inventory =
        create_inventory_sensor(&db, "declared-unrestricted", GLOBAL_PARAM_TEMP_ID).await;

    assert!(
        validate_declared_sensor(
            &db,
            &AccessScope::Unrestricted,
            inventory.id,
            &feed_without_a_serial()
        )
        .await
        .is_ok(),
        "an unconfined caller reaches inventory that is deployed nowhere yet"
    );
}

/// Scenario: a probe is replaced on a logger, so the feed re-registers reporting a different
/// `probe_serial` on the same channel.
///
/// Expected behaviour: nothing forks. The channel is the instrument's identity and an upstream
/// metadata correction is indistinguishable from a physical change, so an automatic fork would
/// silently re-attribute history. The serials on the instrument are refreshed, being information
/// rather than identity, and the change is raised in the review queue for an operator to act on.
#[tokio::test]
#[serial]
async fn a_changed_probe_serial_refreshes_the_instrument_and_raises_a_hold() {
    let (app, token, db) = setup().await;
    let sensor_id = create_sensor(&app, &token, "SWAP-0001").await;

    let register = async |probe: &str| {
        let body = json!({
            "source_system": "vaisala",
            "source_key": "swap-1",
            "sensor_id": sensor_id,
            "metadata": {
                "device": { "logger_serial": "SWAP-0001", "probe_serial": probe },
            },
        });
        crate::common::post_json_parse_with_token(&app, "/api/streams/register", &body, &token).await
    };

    let (status, stream) = register("PROBE-A").await;
    assert!((200..300).contains(&status), "register: {stream}");

    // The first registration mints nothing new; the declared instrument carries the reported probe.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE sensors SET metadata = '{{\"source_probe_serial\": \"PROBE-A\", \
              \"source_device_serial\": \"SWAP-0001\"}}'::jsonb WHERE id = '{sensor_id}'"
        ),
    )
    .await;

    let (status, stream) = register("PROBE-B").await;
    assert!((200..300).contains(&status), "re-register: {stream}");
    let stream_id = stream["id"].as_str().expect("stream id");

    let sensors = sensor_count(&db).await;
    let stored = scalar_text(
        &db,
        &format!("SELECT metadata ->> 'source_probe_serial' AS v FROM sensors WHERE id = '{sensor_id}'"),
    )
    .await;
    assert_eq!(
        stored.as_deref(),
        Some("PROBE-B"),
        "the instrument's recorded probe follows the feed, {sensors} sensors exist"
    );

    let kind = scalar_text(
        &db,
        &format!(
            "SELECT kind AS v FROM replicate_audit_holds \
             WHERE stream_id = '{stream_id}' AND status = 'pending'"
        ),
    )
    .await;
    assert_eq!(
        kind.as_deref(),
        Some("source_identity_changed"),
        "the swap is put in front of an operator rather than applied silently"
    );
}

async fn scalar_text(db: &sea_orm::DatabaseConnection, sql: &str) -> Option<String> {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .and_then(|row| row.try_get::<Option<String>>("", "v").ok().flatten())
}

/// Scenario: a discovery cycle and a triggered full sync re-register one channel at the same
/// instant, both reporting the same identity change.
///
/// Expected behaviour: one standing hold. Neither pass sees the other's uncommitted row, so the
/// second must converge on the first rather than adding a second open hold for the same stream.
#[tokio::test]
#[serial]
async fn concurrent_identity_changes_converge_on_one_hold() {
    let (_app, _token, db) = setup().await;

    let stream_id = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, is_active) \
             VALUES ('{stream_id}', 'vaisala', 'race-1', 'Race 1', true)"
        ),
    )
    .await;

    let stored = json!({ "probe_serial": "PROBE-A" });
    let first = db.begin().await.expect("begin");
    river_db::routes::private::sensors::identity::raise_source_identity_hold(
        &first,
        stream_id,
        &["probe_serial"],
        &stored,
        &json!({ "probe_serial": "PROBE-B" }),
    )
    .await
    .expect("first raise");

    // A plain second connection: `setup_test_db` would block on the harness advisory lock this
    // process already holds.
    let second = sea_orm::Database::connect(std::env::var("DATABASE_URL").expect("DATABASE_URL"))
        .await
        .expect("second connection");
    let raise = tokio::spawn(async move {
        river_db::routes::private::sensors::identity::raise_source_identity_hold(
            &second,
            stream_id,
            &["probe_serial"],
            &json!({ "probe_serial": "PROBE-A" }),
            &json!({ "probe_serial": "PROBE-C" }),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    first.commit().await.expect("commit");
    raise.await.expect("join").expect("the second raise waits for the first, then updates it");

    let open = scalar_i64(
        &db,
        &format!(
            "SELECT count(*) AS v FROM replicate_audit_holds \
             WHERE stream_id = '{stream_id}' AND kind = 'source_identity_changed' \
               AND status IN ('pending', 'deferred')"
        ),
    )
    .await;
    assert_eq!(open, 1, "one channel, one standing identity hold");
}

/// A source-parameter instrument serves every station reporting the parameter, so it is named for
/// the parameter and the source and the second station neither mints another nor renames it.
#[tokio::test]
#[serial]
async fn a_parameter_instrument_is_named_for_its_parameter_not_its_first_station() {
    let (app, token, db) = setup().await;

    let mut ids = vec![];
    for station in ["FP1", "FP3"] {
        let (status, stream) = crate::common::post_json_parse_with_token(
            &app,
            "/api/streams/register",
            &json!({
                "source_system": "cnet",
                "source_key": format!("{station}:DOC_avg_ppb"),
                "source_name": format!("{station} DOC"),
                "metadata": {
                    "hierarchy": { "project": "CNET", "site": station, "parameter": "DOC_avg_ppb" }
                },
            }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "register {station} ({status}): {stream}");
        ids.push(
            stream["sensor_id"]
                .as_str()
                .expect("the feed carries an instrument")
                .to_string(),
        );
    }
    assert_eq!(ids[0], ids[1], "one instrument for the parameter");

    let name = scalar_string(&db, &format!("SELECT name AS v FROM sensors WHERE id = '{}'", ids[0]))
        .await;
    assert_eq!(name, "DOC_avg_ppb (cnet)", "no station is in the name");
}

async fn scalar_i64(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<i64>("", "v")
    .expect("value")
}

async fn scalar_string(db: &sea_orm::DatabaseConnection, sql: &str) -> String {
    use sea_orm::{ConnectionTrait, Statement};
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<String>("", "v")
    .expect("value")
}
