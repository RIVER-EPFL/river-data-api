//! T46: a portal import from discovery to attributed readings. Two stations report the same
//! analyte; the identity that carries them through is the source's parameter instrument.
//!
//! Scenario: two CNET-shaped stations register a `DOC_avg_ppb` replicate family, an operator
//! builds a pairing plan over the source, applies it, and the portal sends a group at each site.
//!
//! Expected behaviour: one instrument serves the analyte at both stations, its name says which
//! source and analyte rather than which station, the key the plan proposes is the key registration
//! already minted, and the readings that arrive after the apply are attributed to the paired slots.
//!
//! Run: cargo test --test e2e portal_import_to_paired -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

const SOURCE: &str = "t46portal";

/// The portal's own column header, which is the code the readings are already stored under. The
/// plan suggests the measurand `DOC_ppb` for the catalog; the instrument key must still be the
/// column, because that is what registration minted from.
const MEAN_COLUMN: &str = "DOC_avg_ppb";

/// The two stations, by the names the seeded sites carry.
const STATIONS: [&str; 2] = ["Upstream Station", "Downstream Station"];

async fn register_family(
    app: &axum::Router,
    token: &str,
    station: &str,
) -> Uuid {
    let (status, stream) = crate::common::post_json_parse_with_token(
        app,
        "/api/streams/register",
        &json!({
            "source_system": SOURCE,
            "source_key": format!("{station}:{MEAN_COLUMN}:reps"),
            "source_name": format!("{station} {MEAN_COLUMN}"),
            "measurement_type": "spot",
            "metadata": {
                "hierarchy": {
                    "project": "Test River Project",
                    "site": station,
                    "parameter": MEAN_COLUMN,
                },
                "units": "ppb",
            },
            "replicates": {
                "source_columns": [format!("{station}_DOC_rep_1"), format!("{station}_DOC_rep_2")],
                "portal_mean_column": MEAN_COLUMN,
                "calc": "calcMean",
            },
        }),
        token,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "register {station} ({status}): {stream}"
    );
    stream["id"].as_str().expect("stream id").parse().expect("uuid")
}

async fn instruments_of_source(db: &DatabaseConnection) -> Vec<(Uuid, String, String)> {
    db.query_all_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT id, name, source_key FROM sensors \
             WHERE source_system = '{SOURCE}' ORDER BY source_key"
        ),
    ))
    .await
    .expect("query")
    .iter()
    .map(|r| {
        (
            r.try_get::<Uuid>("", "id").expect("id"),
            r.try_get::<String>("", "name").expect("name"),
            r.try_get::<String>("", "source_key").expect("source_key"),
        )
    })
    .collect()
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .expect("query")
    .expect("row")
    .try_get::<i64>("", "v")
    .expect("value")
}

#[tokio::test]
#[serial]
async fn a_portal_import_reaches_paired_attributed_readings_under_one_instrument() {
    let f = crate::common::seeded_app().await;
    let (app, token, db) = (f.app, f.token, f.db);

    let streams: Vec<Uuid> = {
        let mut ids = Vec::new();
        for station in STATIONS {
            ids.push(register_family(&app, &token, station).await);
        }
        ids
    };

    // Registration mints the source's parameter instrument, and the analyte is one instrument
    // carried between stations rather than one per station.
    let instruments = instruments_of_source(&db).await;
    assert_eq!(
        instruments.len(),
        1,
        "one instrument spans both stations: {instruments:?}"
    );
    let (instrument_id, instrument_name, instrument_key) = instruments[0].clone();
    assert_eq!(
        instrument_key,
        format!("{SOURCE}:{MEAN_COLUMN}"),
        "the key is the source and its column"
    );
    for station in STATIONS {
        assert!(
            !instrument_name.contains(station),
            "a lab instrument is carried to every station, so its name names none: {instrument_name}"
        );
    }

    // The plan proposes what registration already did: same instrument, same key, nothing to create.
    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &json!({ "source_system": SOURCE }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    let entries = plan["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 2, "one entry per registered stream: {plan}");
    for entry in entries {
        let instrument = &entry["instrument"];
        assert_eq!(
            instrument["source_key"],
            json!(format!("{SOURCE}:{MEAN_COLUMN}")),
            "the plan's key is the key registration minted: {entry}"
        );
        assert_eq!(
            instrument["id"],
            json!(instrument_id),
            "the plan reports the existing instrument rather than proposing a second: {entry}"
        );
        assert_eq!(
            instrument["create"],
            json!(false),
            "nothing is created for an instrument that exists: {entry}"
        );
    }

    let (status, applied) =
        crate::common::post_plan_action_parse_with_token(&app, &plan_id, "apply", &token).await;
    assert_eq!(status, 200, "apply ({status}): {applied}");
    let job_id = applied["job_id"].as_str().expect("apply returns a job_id");
    assert_eq!(
        crate::common::e2e::poll_job(&app, &token, job_id, 30).await,
        "completed",
        "the apply job completes"
    );

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS v FROM data_streams \
                 WHERE source_system = '{SOURCE}' AND site_parameter_id IS NOT NULL"
            ),
        )
        .await,
        2,
        "both stations are paired by the apply"
    );

    // The portal sends a group at each station; what it lands on is the paired slot.
    for stream_id in &streams {
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/ingest",
            &json!({
                "stream_id": stream_id,
                "readings": [
                    { "time": "2026-03-04T09:00:00Z", "raw_value": 41.0, "replicate_index": 0 },
                    { "time": "2026-03-04T09:00:00Z", "raw_value": 43.0, "replicate_index": 1 },
                ]
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "ingest ({status}): {body}");
    }

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS v FROM readings r \
                 JOIN data_streams ds ON ds.id = r.stream_id \
                 WHERE ds.source_system = '{SOURCE}' \
                   AND r.site_id IS NOT NULL AND r.parameter_id IS NOT NULL \
                   AND r.sensor_id = '{instrument_id}'"
            ),
        )
        .await,
        4,
        "every replicate lands attributed to its slot and to the source's instrument"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT COUNT(DISTINCT r.site_id)::bigint AS v FROM readings r \
                 JOIN data_streams ds ON ds.id = r.stream_id \
                 WHERE ds.source_system = '{SOURCE}'"
            ),
        )
        .await,
        2,
        "the two stations attribute to two sites, not to whichever registered first"
    );
}
