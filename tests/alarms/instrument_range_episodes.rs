//! Scenario: a probe starts reporting values its manufacturer says it cannot measure, while the
//! parameter's own thresholds are breached too.
//!
//! Expected behaviour: two episodes stand open on the slot at once, one for the water and one for
//! the instrument, each saying which it is; the range episode names the instrument that measured
//! the value; and an instrument with no declared range raises nothing at all.
//!
//! Run: cargo test --test alarms instrument_range_episodes -- --test-threads=1

use river_db::routes::private::alarms;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

/// Turbidity's global bounds in the fixtures: warning above 100, alarm above 500.
const OVER_THRESHOLD_AND_RANGE: f64 = 600.0;
const IN_EVERY_RANGE: f64 = 50.0;
const AT: &str = "2025-02-01T00:00:00Z";

async fn setup() -> (DatabaseConnection, Uuid, Uuid) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let stream = turb_stream(&db).await;
    let sensor = stream_sensor(&db, stream).await;
    (db, stream, sensor)
}

/// The instrument the stream's readings name, which is the one a range episode is about.
async fn stream_sensor(db: &DatabaseConnection, stream_id: Uuid) -> Uuid {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!("SELECT sensor_id FROM data_streams WHERE id = '{stream_id}'"),
    ))
    .await
    .unwrap()
    .expect("the stream")
    .try_get("", "sensor_id")
    .unwrap()
}

async fn turb_stream(db: &DatabaseConnection) -> Uuid {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT stream_id FROM readings WHERE site_id='{}' AND parameter_id='{}' LIMIT 1",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .expect("a seeded turbidity stream")
    .try_get("", "stream_id")
    .unwrap()
}

async fn declare_range(db: &DatabaseConnection, sensor: Uuid, min: &str, max: &str) {
    crate::common::exec(
        db,
        &format!("UPDATE sensors SET range_min = {min}, range_max = {max} WHERE id = '{sensor}'"),
    )
    .await;
}

async fn inject(db: &DatabaseConnection, stream_id: Uuid, time: &str, value: f64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
             replicate_index) VALUES ('{stream_id}', '{site}', '{param}', '{time}', {value}, 0) \
             ON CONFLICT (stream_id, time, replicate_index) DO UPDATE SET raw_value = {value}",
            site = crate::common::SITE1_ID,
            param = crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    )
    .await;
}

/// Every open episode on the turbidity slot, as (kind, severity, sensor_id).
async fn open_episodes(db: &DatabaseConnection) -> Vec<(String, i16, Option<Uuid>)> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT kind, severity, sensor_id FROM alarm_events \
             WHERE site_id='{}' AND parameter_id='{}' AND resolved_at IS NULL ORDER BY kind",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get::<String>("", "kind").unwrap(),
            row.try_get::<i16>("", "severity").unwrap(),
            row.try_get::<Option<Uuid>>("", "sensor_id").unwrap(),
        )
    })
    .collect()
}

#[tokio::test]
#[serial]
async fn a_value_outside_the_instrument_range_opens_its_own_episode() {
    let (db, stream, sensor) = setup().await;
    declare_range(&db, sensor, "0", "500").await;
    inject(&db, stream, AT, OVER_THRESHOLD_AND_RANGE).await;

    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs");

    let episodes = open_episodes(&db).await;
    assert_eq!(
        episodes.len(),
        2,
        "the water and the instrument are separate episodes: {episodes:?}"
    );
    let range = episodes
        .iter()
        .find(|(kind, _, _)| kind == "instrument_range")
        .expect("a range episode");
    assert_eq!(range.1, 2, "a range breach is an alarm, not a warning");
    assert_eq!(
        range.2,
        Some(sensor),
        "the range episode names the instrument that measured the value"
    );
    let threshold = episodes
        .iter()
        .find(|(kind, _, _)| kind == "threshold")
        .expect("a threshold episode");
    assert_eq!(
        threshold.2, None,
        "a threshold episode is about the water, so it names no instrument"
    );
}

#[tokio::test]
#[serial]
async fn a_value_back_inside_the_range_resolves_the_range_episode() {
    let (db, stream, sensor) = setup().await;
    declare_range(&db, sensor, "0", "500").await;
    inject(&db, stream, AT, OVER_THRESHOLD_AND_RANGE).await;
    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs");
    assert_eq!(open_episodes(&db).await.len(), 2);

    inject(&db, stream, "2025-02-01T01:00:00Z", IN_EVERY_RANGE).await;
    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs again");

    assert!(
        open_episodes(&db).await.is_empty(),
        "a value back in range resolves both episodes"
    );
}

#[tokio::test]
#[serial]
async fn an_instrument_declaring_no_range_raises_nothing_of_its_own() {
    let (db, stream, sensor) = setup().await;
    declare_range(&db, sensor, "NULL", "NULL").await;
    inject(&db, stream, AT, OVER_THRESHOLD_AND_RANGE).await;

    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs");

    let episodes = open_episodes(&db).await;
    assert_eq!(
        episodes
            .iter()
            .filter(|(k, _, _)| k == "instrument_range")
            .count(),
        0,
        "no range, no range episode: {episodes:?}"
    );
    assert_eq!(
        episodes.iter().filter(|(k, _, _)| k == "threshold").count(),
        1,
        "the threshold episode is unchanged: {episodes:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_range_breach_inside_the_thresholds_alarms_on_its_own() {
    let (db, stream, sensor) = setup().await;
    // A range narrower than the parameter's warning bound: the water is unremarkable and the
    // instrument is not, which is the distinction the kind exists for.
    declare_range(&db, sensor, "0", "10").await;
    inject(&db, stream, AT, IN_EVERY_RANGE).await;

    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs");

    let episodes = open_episodes(&db).await;
    assert_eq!(
        episodes.len(),
        1,
        "only the instrument is in breach: {episodes:?}"
    );
    assert_eq!(episodes[0].0, "instrument_range");
}

/// Every resolved episode on the turbidity slot, as (id, kind), ordered by kind.
async fn resolved_episodes(db: &DatabaseConnection) -> Vec<(Uuid, String)> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id, kind FROM alarm_events WHERE site_id='{}' AND parameter_id='{}' \
             AND resolved_at IS NOT NULL ORDER BY kind",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TURB_ID,
        ),
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get::<Uuid>("", "id").unwrap(),
            row.try_get::<String>("", "kind").unwrap(),
        )
    })
    .collect()
}

/// Scenario: a range episode opened and resolved by the sweep, then the slot's history is rebuilt
/// over the same window, as a CSV import does.
///
/// Expected behaviour: the rebuild computes threshold episodes only, so the range episode is left
/// as the sweep wrote it.
#[tokio::test]
#[serial]
async fn a_rebuild_leaves_a_resolved_range_episode_alone() {
    let (db, stream, sensor) = setup().await;
    declare_range(&db, sensor, "0", "500").await;
    inject(&db, stream, AT, OVER_THRESHOLD_AND_RANGE).await;
    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs");
    inject(&db, stream, "2025-02-01T01:00:00Z", IN_EVERY_RANGE).await;
    alarms::flows::evaluate_alarm_events(&db)
        .await
        .expect("the sweep runs again");
    let before = resolved_episodes(&db).await;
    assert_eq!(
        before.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>(),
        ["instrument_range", "threshold"],
        "the sweep resolved both episodes"
    );

    alarms::flows::evaluate_alarm_episodes(
        &db,
        crate::common::SITE1_ID.parse().unwrap(),
        crate::common::GLOBAL_PARAM_TURB_ID.parse().unwrap(),
        AT.parse().unwrap(),
        "2025-02-01T02:00:00Z".parse().unwrap(),
    )
    .await
    .expect("the rebuild runs");

    assert_eq!(
        resolved_episodes(&db).await,
        before,
        "the rebuild keeps both episodes, each in its own row"
    );
}
