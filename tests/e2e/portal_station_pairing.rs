//! T13, the acceptance gate for a real portal station: the paths a paired CNET stream runs
//! through have only ever met fabricated families, so this drives one station's real replicate
//! rows, real timestamps and real portal aggregate cells all the way to what the site serves.
//!
//! Scenario: a CNET reach-depth family is registered, its groups ingested as a windowed
//! collection with the portal's own avg and sd as the audit expectation, and the stream paired.
//!
//! Expected behaviour: the readings attribute to the slot, `samples` materialise with the mean the
//! portal itself stored, every instant becomes a `collection_events` row the visits table shows,
//! and re-asserting the same window changes nothing and says so in the receipt.
//!
//! Run: cargo test --test e2e portal_station_pairing -- --test-threads=1

use crate::common::e2e;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

const PORTAL_GRAB_ROWS: &str = include_str!("../fixtures/portal_grab_rows_cnet.csv");

/// The station whose rows this drives. The fixture is pseudonymised; S01 is one real CNET station.
const STATION: &str = "S01";

/// The portal's own reach-depth family: ten replicate columns with an avg and an sd cell beside
/// them, which is the shape `parameter_calculations` declares with a `calcMean` row.
const MEAN_COLUMN: &str = "Reach_depth_avg_cm";
const SD_COLUMN: &str = "Reach_depth_sd_cm";

/// The portals store their aggregate cells at 2 decimals, so a comparison against one is held to
/// the stored quantum rather than to float equality.
const QUANTUM: f64 = 0.005;

fn replicate_columns() -> Vec<String> {
    (1..=10).map(|i| format!("Reach_depth_rep_{i}")).collect()
}

/// One portal visit row, as the connector reads it.
struct PortalGroup {
    /// The instant in UTC: the portal stores local date and time beside the offset to GMT.
    time: String,
    /// `(column, value)` for every replicate cell the row actually holds.
    replicates: Vec<(String, f64)>,
    portal_mean: f64,
    portal_sd: f64,
}

fn parse_groups() -> Vec<PortalGroup> {
    let mut lines = PORTAL_GRAB_ROWS.lines();
    let header: Vec<&str> = lines.next().expect("header").split(',').collect();
    let column = |row: &[&str], name: &str| -> String {
        header
            .iter()
            .position(|h| *h == name)
            .and_then(|i| row.get(i))
            .unwrap_or(&"")
            .to_string()
    };

    let columns = replicate_columns();
    let mut groups = Vec::new();
    for line in lines {
        let row: Vec<&str> = line.split(',').collect();
        if column(&row, "station") != STATION {
            continue;
        }
        let replicates: Vec<(String, f64)> = columns
            .iter()
            .filter_map(|c| {
                column(&row, c)
                    .parse::<f64>()
                    .ok()
                    .map(|v| (c.clone(), v))
            })
            .collect();
        let (Ok(portal_mean), Ok(portal_sd)) = (
            column(&row, MEAN_COLUMN).parse::<f64>(),
            column(&row, SD_COLUMN).parse::<f64>(),
        ) else {
            continue;
        };
        if replicates.len() < 2 {
            continue;
        }
        let date = column(&row, "DATE_reading");
        let time = column(&row, "TIME_reading");
        let offset = column(&row, "Convert_to_GMT");
        let hours: i64 = offset.split(':').next().unwrap_or("0").parse().unwrap_or(0);
        let local = format!("{date}T{time}");
        let naive = chrono::NaiveDateTime::parse_from_str(&local, "%Y-%m-%dT%H:%M:%S")
            .unwrap_or_else(|e| panic!("portal timestamp {local}: {e}"));
        let utc = naive - chrono::Duration::hours(hours);
        groups.push(PortalGroup {
            time: utc.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            replicates,
            portal_mean,
            portal_sd,
        });
    }
    groups
}

async fn scalar_f64(db: &DatabaseConnection, sql: &str) -> f64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap_or_else(|| panic!("no row for: {sql}"))
    .try_get::<f64>("", "v")
    .unwrap()
}

#[tokio::test]
#[serial]
async fn a_real_cnet_station_pairs_through_to_samples_and_visits() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (sync_token, _service_id) = crate::common::seed_sync_session_token(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let groups = parse_groups();
    assert!(
        groups.len() >= 5,
        "the committed CNET fixture holds several full reach-depth groups for {STATION}"
    );

    let project = e2e::create_project(&app, &token, "CNET", "cnet-t13", false).await;
    let site = e2e::create_site(&app, &token, &project, STATION, "s01").await;
    let param = e2e::create_parameter(&app, &token, MEAN_COLUMN, "Reach depth", "cm").await;
    let sp = e2e::assign_site_parameter_minimal(&app, &token, &site, &param).await;

    let columns = replicate_columns();
    let (status, registered) = crate::common::post_json_parse_with_token(
        &app,
        "/api/streams/register",
        &json!({
            "source_system": "cnet",
            "source_key": format!("{STATION}:{MEAN_COLUMN}:reps"),
            "measurement_type": "spot",
            "replicates": {
                "source_columns": columns,
                "portal_mean_column": MEAN_COLUMN,
                "portal_sd_column": SD_COLUMN,
                "calc": "calcMean",
            },
        }),
        &sync_token,
    )
    .await;
    assert_eq!(status, 200, "register the family ({status}): {registered}");
    let stream_id = e2e::id_of(&registered);

    // The pinned mapping is the authority on a column's replicate index, exactly as the connector
    // treats it; nothing here derives an index from column order.
    let pinned: Vec<(String, i64)> = registered["replicates"]
        .as_array()
        .unwrap_or_else(|| panic!("register response carries the mapping: {registered}"))
        .iter()
        .map(|a| {
            (
                a["column"].as_str().unwrap().to_string(),
                a["index"].as_i64().unwrap(),
            )
        })
        .collect();
    let index_of = |column: &str| -> i64 {
        pinned
            .iter()
            .find(|(c, _)| c == column)
            .unwrap_or_else(|| panic!("column {column} missing from {pinned:?}"))
            .1
    };

    let readings: Vec<serde_json::Value> = groups
        .iter()
        .flat_map(|g| {
            g.replicates.iter().map(|(column, value)| {
                json!({
                    "time": g.time,
                    "raw_value": value,
                    "replicate_index": index_of(column),
                })
            })
        })
        .collect();
    let audits: Vec<serde_json::Value> = groups
        .iter()
        .map(|g| {
            json!({
                "time": g.time,
                "expected_mean": g.portal_mean,
                "expected_sd": g.portal_sd,
                "expected_n": g.replicates.len(),
            })
        })
        .collect();
    let submitted = readings.len();
    let window = json!({
        "from": groups.first().unwrap().time,
        "to": "2030-01-01T00:00:00Z",
        "source_rows_read": groups.len(),
    });

    let ingest = json!({
        "stream_id": stream_id,
        "readings": readings,
        "collection": true,
        "audit": audits,
        "window": window,
    });
    let (status, body) =
        crate::common::post_json_parse_with_token(&app, "/api/ingest", &ingest, &sync_token).await;
    assert_eq!(status, 200, "ingest the station's groups ({status}): {body}");
    assert_eq!(body["inserted"], submitted as u64, "every cell lands: {body}");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": sp}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "pair ({status}): {body}");

    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE stream_id = '{stream_id}' \
                 AND site_id = '{site}' AND parameter_id = '{param}'"
            )
        )
        .await,
        submitted as i64,
        "pairing attributes every cell to the slot"
    );

    // The trigger's mean is what the site serves, and it agrees with the cell the portal itself
    // stored for the same visit.
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM samples WHERE site_id = '{site}' AND parameter_id = '{param}'")
        )
        .await,
        groups.len() as i64,
        "one sample per portal visit"
    );
    for group in &groups {
        let mean = scalar_f64(
            &db,
            &format!(
                "SELECT mean AS v FROM samples WHERE site_id = '{site}' \
                 AND parameter_id = '{param}' AND collected_at = '{}'",
                group.time
            ),
        )
        .await;
        assert!(
            (mean - group.portal_mean).abs() <= QUANTUM,
            "our mean {mean} at {} disagrees with the portal's stored {}",
            group.time,
            group.portal_mean
        );
    }

    // Every instant is a visit, and the readings hang off it.
    assert_eq!(
        e2e::count(
            &db,
            &format!("SELECT count(*) FROM collection_events WHERE site_id = '{site}'")
        )
        .await,
        groups.len() as i64,
        "one collection event per (site, collected_at)"
    );
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT count(*) FROM readings WHERE stream_id = '{stream_id}' \
                 AND collection_event_id IS NULL"
            )
        )
        .await,
        0,
        "every attributed spot reading attaches to its visit"
    );

    let (status, visits) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{site}/visits"), &token)
            .await;
    assert_eq!(status, 200, "visits ({status}): {visits}");
    let rows = visits["visits"]
        .as_array()
        .unwrap_or_else(|| panic!("visits payload: {visits}"));
    assert_eq!(rows.len(), groups.len(), "the visits table shows them all");

    // Re-asserting the same window is the portal's every-cycle behaviour: nothing changes, and the
    // receipt is where that is stated.
    let (status, body) =
        crate::common::post_json_parse_with_token(&app, "/api/ingest", &ingest, &sync_token).await;
    assert_eq!(status, 200, "re-assert ({status}): {body}");
    let receipt = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT changed, new_rows, withdrawn, unchanged FROM ingest_receipts \
                 WHERE stream_id = '{stream_id}' ORDER BY at DESC LIMIT 1"
            ),
        ))
        .await
        .unwrap()
        .expect("a windowed pass commits a receipt");
    assert_eq!(receipt.try_get::<i32>("", "changed").unwrap(), 0);
    assert_eq!(receipt.try_get::<i32>("", "new_rows").unwrap(), 0);
    assert_eq!(receipt.try_get::<i32>("", "withdrawn").unwrap(), 0);
    assert_eq!(
        receipt.try_get::<i32>("", "unchanged").unwrap(),
        submitted as i32,
        "an unchanged re-assert accounts for every stored row"
    );
}
