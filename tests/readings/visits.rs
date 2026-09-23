//! The visits table: one row per collection event with the wide per-parameter cells, and the
//! per-event detail grid behind a row.
//!
//! Run: cargo test --test readings visits -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, GLOBAL_PARAM_TEMP_ID, SITE1_ID};

const T1: &str = "2025-06-01T08:00:00Z";
const T2: &str = "2025-06-08T09:30:00Z";

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn save_two_visits(app: &axum::Router, token: &str) {
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 10.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_DO_ID, "value": 12.0, "time": T1 },
                { "parameter_id": GLOBAL_PARAM_TEMP_ID, "value": 4.2, "time": T1 },
            ],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = crate::common::post_json_with_token(
        app,
        "/api/grab_samples",
        &json!({
            "site_id": SITE1_ID,
            "readings": [{ "parameter_id": GLOBAL_PARAM_DO_ID, "value": 9.0, "time": T2 }],
        }),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
}

fn cell<'a>(row: &'a serde_json::Value, parameter_id: &str) -> Option<&'a serde_json::Value> {
    row["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["parameter_id"] == parameter_id)
}

#[tokio::test]
#[serial]
async fn the_list_is_the_wide_portal_row() {
    let (_db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2);
    let columns = body["expected_parameters"].as_array().unwrap();
    assert!(
        columns
            .iter()
            .any(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
            && columns
                .iter()
                .any(|c| c["parameter_id"] == GLOBAL_PARAM_TEMP_ID),
        "both spot parameters are grid columns: {body}"
    );

    let visits = body["visits"].as_array().unwrap();
    assert_eq!(visits[0]["collected_at"], T2, "newest first");
    assert_eq!(visits[0]["parameters_filled"], 1);
    assert_eq!(visits[1]["parameters_filled"], 2);
    let do_cell = cell(&visits[1], GLOBAL_PARAM_DO_ID).expect("DO cell at T1");
    assert_eq!(
        do_cell["value"], 11.0,
        "the served value is the sample mean"
    );
    let temp_cell = cell(&visits[1], GLOBAL_PARAM_TEMP_ID).expect("Temp cell at T1");
    assert_eq!(temp_cell["value"], 4.2);
    assert!(
        cell(&visits[0], GLOBAL_PARAM_TEMP_ID).is_none(),
        "no Temp at T2"
    );
}

/// Expected behaviour: the listing carries the replicates behind each cell, in index order,
/// singletons included, so a parameter's column opens to its repeats without a second fetch, and
/// each one names the stream a correction against it keys on.
#[tokio::test]
#[serial]
async fn a_listed_cell_carries_its_replicates_and_what_a_correction_keys_on() {
    let (_db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let visits = body["visits"].as_array().unwrap();

    let do_cell = cell(&visits[1], GLOBAL_PARAM_DO_ID).expect("DO cell at T1");
    let replicates = do_cell["replicates"].as_array().unwrap();
    assert_eq!(replicates.len(), 2, "the duplicate lists both: {do_cell}");
    assert_eq!(replicates[0]["replicate_index"], 0);
    assert_eq!(replicates[0]["value"], 10.0);
    assert_eq!(replicates[1]["replicate_index"], 1);
    assert_eq!(replicates[1]["value"], 12.0);
    assert_eq!(replicates[0]["flagged"], false);
    assert_eq!(replicates[0]["withdrawn"], false);

    assert_eq!(
        replicates[0]["stream_id"], replicates[1]["stream_id"],
        "both repeats came in on the slot's own feed: {do_cell}"
    );
    assert!(
        replicates[0]["stream_id"].is_string(),
        "a correction keys on the stream, so the listing names it: {do_cell}"
    );
    assert_eq!(
        do_cell["has_provenance"], false,
        "nothing computed this, so Q8 corrects it in place rather than through a tool: {do_cell}"
    );

    let temp_cell = cell(&visits[1], GLOBAL_PARAM_TEMP_ID).expect("Temp cell at T1");
    let single = temp_cell["replicates"].as_array().unwrap();
    assert_eq!(
        single.len(),
        1,
        "a measurement taken once lists one entry, not none: {temp_cell}"
    );
    assert_eq!(single[0]["value"], 4.2);
}

#[tokio::test]
#[serial]
async fn the_detail_grid_shows_replicates_and_sample_stats() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let event_id: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM collection_events \
                 WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["collected_at"], T1);
    let cells = body["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 2);
    let do_cell = cells
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .unwrap();
    assert_eq!(do_cell["served_value"], 11.0);
    assert_eq!(do_cell["sample"]["n"], 2);
    assert_eq!(do_cell["replicates"].as_array().unwrap().len(), 2);
    assert_eq!(do_cell["has_provenance"], false);
    // The cell carries the instant's assembled record, so the point record opened from the
    // grid needs no second round trip.
    let record = &do_cell["record"];
    assert_eq!(record["origin"]["stream_id"], do_cell["stream_id"]);
    assert_eq!(record["origin"]["classification"], "manual");
    assert_eq!(record["readings"].as_array().unwrap().len(), 2);
    assert_eq!(record["readings"][1]["replicate_index"], 1);
    let temp_cell = cells
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_TEMP_ID)
        .unwrap();
    assert_eq!(temp_cell["record"]["readings"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn a_finding_marks_its_cell_and_the_row() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "INSERT INTO replicate_audit_holds \
                 (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status) \
             VALUES ('stale_output', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', 'chain_b', \
                     '{{}}', '{{}}', '{{}}', 'pending')"
        ),
    ))
    .await
    .unwrap();

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let visits = body["visits"].as_array().unwrap();
    assert_eq!(visits[1]["findings_open"], 1);
    assert_eq!(
        cell(&visits[1], GLOBAL_PARAM_DO_ID).unwrap()["finding"],
        "stale_output"
    );
    assert_eq!(visits[0]["findings_open"], 0);
}

#[tokio::test]
#[serial]
async fn a_fully_withdrawn_group_empties_its_cell() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    db.execute_raw(Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "UPDATE readings SET withdrawn_at = NOW() \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' \
               AND time = '{T2}'"
        ),
    ))
    .await
    .unwrap();

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let visits = body["visits"].as_array().unwrap();
    assert_eq!(
        visits[0]["parameters_filled"], 0,
        "a withdrawn group no longer fills"
    );
    let do_cell = cell(&visits[0], GLOBAL_PARAM_DO_ID).unwrap();
    assert_eq!(do_cell["withdrawn"], true);
}

/// Scenario: two of a visit's replicates are flagged during review, and a parameter's every
/// replicate is flagged.
///
/// Expected behaviour: the partly curated cell reports what was removed, because the served mean
/// moved and a plain number gives a reviewer no way to tell a curation from a measurement change;
/// and a parameter serving nothing does not count toward the visit's fill, which would otherwise
/// claim a value the same row renders as empty.
#[tokio::test]
#[serial]
async fn curated_replicates_are_counted_and_do_not_count_as_filled() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    // One of the two DO replicates, and the lone TEMP replicate.
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = true, flag_reason = 'outlier' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' \
               AND (parameter_id = '{GLOBAL_PARAM_TEMP_ID}' \
                    OR (parameter_id = '{GLOBAL_PARAM_DO_ID}' AND replicate_index = 1))"
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let row = body["visits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| {
            v["collected_at"]
                .as_str()
                .unwrap()
                .starts_with("2025-06-01")
        })
        .expect("the first visit is listed");

    let dissolved = cell(row, GLOBAL_PARAM_DO_ID).expect("DO cell");
    assert_eq!(dissolved["n_total"], 2, "{dissolved}");
    assert_eq!(dissolved["n_flagged"], 1, "{dissolved}");
    assert_eq!(
        dissolved["flagged"], false,
        "one of two flagged is not a flagged group: {dissolved}"
    );

    assert_eq!(
        row["parameters_filled"], 1,
        "a parameter serving nothing is not filled: {row}"
    );
    assert!(
        row["parameters_filled"].as_i64().unwrap()
            <= body["expected_parameters"].as_array().unwrap().len() as i64,
        "the ratio cannot exceed its denominator: {body}"
    );
}

/// Scenario: a statistics disagreement on a paired stream at a visit's own instant.
///
/// Expected behaviour: it is counted and marks its cell. A hold is keyed on the slot or on the
/// stream that raised it, and only event-audit findings carry a slot, so reading one key shape
/// makes every replicate, source-modification and brake hold invisible in the grid.
#[tokio::test]
#[serial]
async fn a_stream_keyed_hold_marks_the_visit_it_lands_at() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let stream_id = crate::common::e2e::id_of(
        &crate::common::post_json_parse_with_token(
            &app,
            "/api/streams/register",
            &json!({"source_system": "cnet", "source_key": "visit-hold:reps",
                    "measurement_type": "spot"}),
            &token,
        )
        .await
        .1,
    );
    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/streams/{stream_id}/pair"),
        &json!({"site_parameter_id": crate::common::PARAM_S1_DO_ID}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (stream_id, group_time, kind, expected, computed, delta, status) \
             VALUES ('{stream_id}', '{T1}', 'replicate_stats', '{{}}'::jsonb, '{{}}'::jsonb, \
                     '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let row = body["visits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| {
            v["collected_at"]
                .as_str()
                .unwrap()
                .starts_with("2025-06-01")
        })
        .expect("the first visit is listed");
    assert_eq!(
        row["findings_open"], 1,
        "a stream-keyed hold at the visit's instant is a finding on it: {row}"
    );
    assert_eq!(
        cell(row, GLOBAL_PARAM_DO_ID).and_then(|c| c["finding"].as_str()),
        Some("replicate_stats"),
        "and it marks the slot's own cell: {row}"
    );

    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{}/detail", row["id"].as_str().unwrap()),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    let do_cell = detail["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the detail carries the DO cell");
    assert_eq!(
        do_cell["finding"]["kind"], "replicate_stats",
        "the opened visit names the same finding as its row: {do_cell}"
    );
}

/// Scenario: two paired streams serve one slot at one spot instant, which is what reconciliation
/// produces while a legacy `_avg` stream and its `:reps` sibling are both paired.
///
/// Expected behaviour: one chart point, not two. A `(site, parameter, time)` group is one sample
/// whatever number of feeds contributed, which is already what the materialiser and the samples
/// trigger say; the serving layer was the only place drawing it once per stream, and which of the
/// two values survived was whichever row the plan emitted last.
#[tokio::test]
#[serial]
async fn two_streams_on_one_slot_serve_one_spot_point() {
    let (db, app, token) = setup().await;

    for key in ["dup-a", "dup-b"] {
        let stream_id = crate::common::e2e::id_of(
            &crate::common::post_json_parse_with_token(
                &app,
                "/api/streams/register",
                &json!({"source_system": "cnet", "source_key": key, "measurement_type": "spot"}),
                &token,
            )
            .await
            .1,
        );
        let (status, body) = crate::common::post_json_with_token(
            &app,
            &format!("/api/streams/{stream_id}/pair"),
            &json!({"site_parameter_id": crate::common::PARAM_S1_DO_ID}),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/ingest",
            &json!({
                "stream_id": stream_id,
                "readings": [{ "time": T1, "raw_value": if key == "dup-a" { 3.0 } else { 9.0 } }],
            }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{SITE1_ID}/readings?start=2025-06-01T00:00:00Z&end=2025-06-02T00:00:00Z\
             &parameter_ids={GLOBAL_PARAM_DO_ID}&measurement_type=spot"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["times"].as_array().map(Vec::len),
        Some(1),
        "one slot instant is one point however many feeds reached it: {body}"
    );

    // Both feeds' readings are still on the record, each naming the stream it arrived on.
    let event_id: Uuid = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id FROM collection_events \
                 WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the instant staged an event")
        .try_get("", "id")
        .expect("id");
    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    let named: Vec<&str> = detail["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["source_key"].as_str())
        .collect();
    assert_eq!(named.len(), 2, "each feed's row names its stream: {detail}");
}

/// Scenario: a group that reached the store through the CSV importer or a token batch rather than
/// by hand.
///
/// Expected behaviour: the detail grid can say so. "No tool-run blob" is not the same claim as
/// "a person typed this", and a reviewer deciding whether a suspicious number was entered or
/// imported was being told the wrong one.
#[tokio::test]
#[serial]
async fn a_cell_reports_how_its_readings_reached_the_store() {
    let (_db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let event_id = body["visits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| {
            v["collected_at"]
                .as_str()
                .unwrap()
                .starts_with("2025-06-01")
        })
        .and_then(|v| v["id"].as_str())
        .expect("the visit is listed")
        .to_string();

    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    let origins: Vec<&str> = detail["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["origin"].as_str())
        .collect();
    assert!(
        !origins.is_empty() && origins.iter().all(|o| *o == "manual"),
        "a grab save is manual, and says so rather than being inferred from a missing blob: {detail}"
    );
}

/// Scenario: a value typed into the grid, which the store attributes to the slot's own entry
/// channel because nothing was declared.
///
/// Expected behaviour: each replicate says what kind of instrument it names, so the grid can tell
/// a probe from a bookkeeping row it may not name back on a save.
#[tokio::test]
#[serial]
async fn a_replicate_says_what_kind_of_instrument_it_names() {
    let (_db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let event_id = body["visits"][0]["id"]
        .as_str()
        .expect("a visit is listed")
        .to_string();

    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    let replicate = &detail["cells"][0]["replicates"][0];
    assert!(replicate["sensor_id"].is_string(), "{detail}");
    assert_eq!(
        replicate["sensor_kind"], "entry_channel",
        "a hand entry names the slot's own channel, and the grid is told so: {detail}"
    );
}

/// Fifty-five empty visits on top of the two with readings, so the default page size the list
/// used to apply would truncate the answer.
async fn stage_many_visits(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO collection_events (site_id, collected_at, source) \
             SELECT '{SITE1_ID}', TIMESTAMPTZ '2020-01-01T10:00:00Z' + (n || ' days')::interval, \
                    'portal_sync' \
             FROM generate_series(1, 55) AS n"
        ),
    )
    .await;
}

/// Scenario: a station with more visits than one page, read with and without a date range.
///
/// Expected behaviour: the unbounded call lists every event at the site (the page devoted to
/// them lists them all), and `start`/`end` narrow the rows to the visits inside the range.
#[tokio::test]
#[serial]
async fn a_date_range_narrows_the_list_and_no_range_lists_every_visit() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;
    stage_many_visits(&db).await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 57);
    assert_eq!(
        body["visits"].as_array().map(Vec::len),
        Some(57),
        "an unbounded call lists every visit: {}",
        body["visits"].as_array().map(Vec::len).unwrap_or(0)
    );

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/sites/{SITE1_ID}/visits?start=2025-06-02T00:00:00Z&end=2025-06-30T00:00:00Z"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 1);
    let visits = body["visits"].as_array().unwrap();
    assert_eq!(visits.len(), 1);
    assert_eq!(visits[0]["collected_at"], T2);

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits?page=1&page_size=10"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 57);
    assert_eq!(
        body["visits"].as_array().map(Vec::len),
        Some(10),
        "asking for a page still pages"
    );
}

/// Scenario: the operator downloads the visits grid as displayed.
///
/// Expected behaviour: the CSV is the grid cell for cell, one row per visit newest first, one
/// column per expected parameter headed by its code, the served value in each cell and an empty
/// cell where the grid shows none. The file is named by site and date range.
#[tokio::test]
#[serial]
async fn the_csv_download_reproduces_the_grid() {
    let (_db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let (status, grid) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{grid}");
    let (status, headers, csv) = crate::common::get_with_token_headers(
        &app,
        &format!("/api/sites/{SITE1_ID}/visits?format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{csv}");
    assert!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/csv")),
        "{headers:?}"
    );
    let disposition = headers
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        disposition.contains("visits")
            && disposition.contains("2025-06-01")
            && disposition.contains("2025-06-08"),
        "named by site and date range: {disposition}"
    );

    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().expect("header").split(',').collect();
    let codes: Vec<&str> = grid["expected_parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["code"].as_str().unwrap())
        .collect();
    assert_eq!(header[0], "collected_at");
    assert_eq!(
        &header[header.len() - codes.len()..],
        &codes[..],
        "one column per expected parameter, headed by code: {header:?}"
    );
    let first_code_col = header.len() - codes.len();

    let visits = grid["visits"].as_array().unwrap();
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    assert_eq!(rows.len(), visits.len(), "one line per visit: {csv}");
    for (row, visit) in rows.iter().zip(visits) {
        assert_eq!(row[0], visit["collected_at"].as_str().unwrap());
        for (i, col) in grid["expected_parameters"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let expected = cell(visit, col["parameter_id"].as_str().unwrap())
                .and_then(|c| c["value"].as_f64());
            let got = row[first_code_col + i].parse::<f64>().ok();
            assert_eq!(got, expected, "cell {} at {}: {csv}", col["code"], row[0]);
        }
    }
}

/// Scenario: the cross-site visits list, the way into a site's grid.
///
/// Expected behaviour: each row carries the same fill and open-finding counts the site-scoped
/// list computes, the site's name, and can be narrowed to one site and ordered by findings.
#[tokio::test]
#[serial]
async fn the_cross_site_list_reports_fill_and_findings() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO collection_events (site_id, collected_at, source) \
             VALUES ('{}', '2025-06-03T08:00:00Z', 'portal_sync')",
            crate::common::SITE2_ID
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (kind, site_id, parameter_id, group_time, tool, expected, computed, delta, status) \
             VALUES ('stale_output', '{SITE1_ID}', '{GLOBAL_PARAM_DO_ID}', '{T1}', 'chain_b', \
                     '{{}}', '{{}}', '{{}}', 'pending')"
        ),
    )
    .await;

    let (status, body) = crate::common::get_json_with_token(&app, "/api/visits", &token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 3);
    let visits = body["items"].as_array().unwrap();
    let first = visits
        .iter()
        .find(|v| v["collected_at"] == T1)
        .expect("the first visit is listed");
    assert_eq!(first["site_id"], SITE1_ID);
    assert_eq!(first["site_name"], "Upstream Station");
    assert_eq!(first["parameters_filled"], 2, "{first}");
    assert_eq!(first["findings_open"], 1, "{first}");
    assert_eq!(first["recompute"], "stale", "{first}");

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/visits?site_id={SITE1_ID}&sort=findings_open&order=desc"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2);
    let visits = body["items"].as_array().unwrap();
    assert_eq!(visits[0]["collected_at"], T1, "most findings first: {body}");
    assert_eq!(visits[1]["findings_open"], 0);
}

/// Scenario: the lab opens a CNET station whose history was synced, expands a 2025 visit and
/// presses Recompute tools.
///
/// Expected behaviour: refused, naming where the correction belongs (Q41). The audit takes the
/// request and raises nothing there: it reports on the set the repair can repair (Q175).
#[tokio::test]
#[serial]
async fn the_per_visit_recompute_refuses_a_synced_visit_and_the_audit_does_not() {
    let (db, app, token) = setup().await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO collection_events (site_id, collected_at, source) \
             VALUES ('{SITE1_ID}', '2025-06-04T08:00:00Z', 'portal_sync')"
        ),
    )
    .await;
    let synced: String = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT id::text AS v FROM collection_events \
              WHERE site_id = '{SITE1_ID}' AND collected_at = '2025-06-04T08:00:00Z'"
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/collection_events/{synced}/recompute"),
        &serde_json::json!({}),
        &token,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string().contains("portal sync"),
        "the refusal says why: {body}"
    );
    let queued = crate::common::e2e::scalar(
        &db,
        "SELECT count(*)::text AS v FROM reprocessing_jobs \
          WHERE trigger_type = 'event_recompute'",
    )
    .await;
    assert_eq!(queued, "0", "the refusal queues nothing");

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/actions/event_audit",
        &serde_json::json!({ "collection_event_id": synced }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "the audit takes the request: {body}");
    crate::common::e2e::drain_jobs(&db, 60).await;
    let raised = crate::common::e2e::scalar(
        &db,
        &format!(
            "SELECT count(*)::text AS v FROM replicate_audit_holds \
              WHERE site_id = '{SITE1_ID}' AND group_time = '2025-06-04T08:00:00Z'"
        ),
    )
    .await;
    assert_eq!(
        raised, "0",
        "a synced visit is the portal's, so the audit raises nothing against it"
    );
}

/// Scenario: an intern's entry, which lands unverified and which the sample trigger counts none of.
///
/// Expected behaviour: both grids say so. The values are on screen with `n = 0` beside them, and
/// nothing but the ledger would otherwise say why the statistics are empty.
#[tokio::test]
#[serial]
async fn a_pending_replicate_is_reported_as_pending() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET unverified = true \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' \
               AND parameter_id = '{GLOBAL_PARAM_DO_ID}'"
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let row = body["visits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| {
            v["collected_at"]
                .as_str()
                .unwrap()
                .starts_with("2025-06-01")
        })
        .expect("the first visit is listed");
    let dissolved = cell(row, GLOBAL_PARAM_DO_ID).expect("DO cell");
    assert_eq!(dissolved["n_total"], 2, "{dissolved}");
    assert_eq!(
        dissolved["n_unverified"], 2,
        "the cell says how many of its replicates are pending: {dissolved}"
    );
    let temperature = cell(row, GLOBAL_PARAM_TEMP_ID).expect("TEMP cell");
    assert_eq!(
        temperature["n_unverified"], 0,
        "a verified cell says zero rather than nothing: {temperature}"
    );

    let event_id = row["id"].as_str().unwrap();
    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    let replicates = detail["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["parameter_id"] == GLOBAL_PARAM_DO_ID)
        .expect("the DO cell")["replicates"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(replicates.len(), 2, "{detail}");
    assert!(
        replicates.iter().all(|r| r["unverified"] == true),
        "each pending replicate carries its state: {detail}"
    );
}

/// Scenario: one measurement at a visit, which forms no `samples` row.
///
/// Expected behaviour: the visit grid counts it as one, the way the serving arm already does.
/// A cell whose only replicate is excluded counts none, and a cell with no readings at all
/// counts nothing.
#[tokio::test]
#[serial]
async fn a_single_measurement_counts_as_one() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let row = |body: &serde_json::Value| -> serde_json::Value {
        body["visits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| {
                v["collected_at"]
                    .as_str()
                    .unwrap()
                    .starts_with("2025-06-01")
            })
            .expect("the first visit is listed")
            .clone()
    };
    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let first = row(&body);
    let temperature = cell(&first, GLOBAL_PARAM_TEMP_ID).expect("TEMP cell");
    assert_eq!(temperature["n_total"], 1, "{temperature}");
    assert_eq!(
        temperature["n"], 1,
        "a lone measurement is one measurement, not an unknown count: {temperature}"
    );
    let dissolved = cell(&first, GLOBAL_PARAM_DO_ID).expect("DO cell");
    assert_eq!(
        dissolved["n"], 2,
        "the sample's own count still wins: {dissolved}"
    );

    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET is_flagged = true, flag_reason = 'outlier' \
             WHERE site_id = '{SITE1_ID}' AND time = '{T1}' \
               AND parameter_id = '{GLOBAL_PARAM_TEMP_ID}'"
        ),
    )
    .await;
    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let reflagged = row(&body);
    let temperature = cell(&reflagged, GLOBAL_PARAM_TEMP_ID).expect("TEMP cell");
    assert_eq!(
        temperature["n"], 0,
        "the count is what the mean would stand on: {temperature}"
    );
}

/// Scenario: a site whose grid carries a plain measurement beside a parameter a calculation writes.
///
/// Expected behaviour: the column of the computed parameter names the calculation that writes it,
/// and the measured one names none but names the calculation reading it. The grid decides from
/// this whether a cell takes a keystroke, and says what a typed value feeds before it is saved.
#[tokio::test]
#[serial]
async fn a_computed_column_names_the_calculation_that_writes_it() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;

    let group = "00000000-0000-4000-c000-000000000121";
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal) \
             VALUES ('{group}', 'visit_roles', 'Visit roles', 1)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal) \
             VALUES (gen_random_uuid(), '{group}', '{GLOBAL_PARAM_DO_ID}', 1)"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        "INSERT INTO tool_scripts (name, label, engine, created_by) \
         VALUES ('visit_roles', 'Visit roles', 'formula', 'test')",
    )
    .await;
    let script_id = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT id FROM tool_scripts WHERE name = 'visit_roles'".to_string(),
        ))
        .await
        .expect("query")
        .expect("the calculation")
        .try_get::<Uuid>("", "id")
        .expect("id")
        .to_string();
    let (status, text) = crate::common::save_formula_set(
        &app,
        &token,
        &script_id,
        json!([{ "code": "visit_roles_out", "units": "ratio", "formula": "Dissolved_O2 * 2", "ordinal": 1 }]),
    )
    .await;
    assert!((200..300).contains(&status), "save ({status}): {text}");

    // The output is a column of this site's grid once the site declares it.
    let output_id = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT id FROM parameters WHERE code = 'visit_roles_out'".to_string(),
        ))
        .await
        .expect("query")
        .expect("the save minted the output")
        .try_get::<Uuid>("", "id")
        .expect("id");
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, entry_mode) \
             VALUES (gen_random_uuid(), '{SITE1_ID}', '{output_id}', 'visit_roles_out', '', 'tool')"
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let column = |parameter_id: &str| {
        body["expected_parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["parameter_id"] == parameter_id)
            .cloned()
            .unwrap_or_else(|| panic!("no column for {parameter_id}: {body}"))
    };
    assert_eq!(
        column(&output_id.to_string())["written_by"],
        "visit_roles",
        "the computed column names its calculation: {body}"
    );
    assert!(
        column(GLOBAL_PARAM_DO_ID)["written_by"].is_null(),
        "a measured column names none: {body}"
    );
    assert_eq!(
        column(GLOBAL_PARAM_DO_ID)["read_by"],
        json!(["visit_roles"]),
        "the measured column names what reads it: {body}"
    );
    assert!(
        column(&output_id.to_string())["read_by"].is_null(),
        "nothing reads the output: {body}"
    );
}

/// Scenario: a listed cell whose reading was written by a calculation run, so its provenance names
/// the run.
///
/// Expected behaviour: the listing serves and names the run on the cell.
#[tokio::test]
#[serial]
async fn a_calculated_cell_names_its_run_in_the_listing() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;
    let run_id = Uuid::new_v4();
    db.execute_raw(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE readings SET provenance = jsonb_build_object('tool', 'doc', 'run_id', $1::text) \
         WHERE site_id = $2 AND parameter_id = $3 AND time = $4::timestamptz",
        [
            run_id.to_string().into(),
            Uuid::parse_str(SITE1_ID).unwrap().into(),
            Uuid::parse_str(GLOBAL_PARAM_TEMP_ID).unwrap().into(),
            T1.into(),
        ],
    ))
    .await
    .unwrap();

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let temp = body["visits"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|v| cell(v, GLOBAL_PARAM_TEMP_ID))
        .expect("the temperature cell is listed");
    assert_eq!(temp["tool_run_id"], run_id.to_string(), "{temp}");
}

/// Expected behaviour: a listed cell names each curve its replicates were corrected through, so
/// the grid can say which curve a value was made with before the visit is opened (Q97).
#[tokio::test]
#[serial]
async fn a_corrected_cell_names_its_curve_in_the_listing() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;
    let sensor =
        crate::common::sensor_lifecycle::create_sensor_without_curve(&db, "analyser").await;
    let curve = Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve}', '{sensor}', 2.0, 0.0, 'Curve 2026-03')"
        ),
    )
    .await;
    crate::common::exec(
        &db,
        &format!(
            "UPDATE readings SET standard_curve_id = '{curve}', calibrated_value = raw_value * 2 \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DO_ID}' AND time = '{T1}'"
        ),
    )
    .await;

    let (status, body) =
        crate::common::get_json_with_token(&app, &format!("/api/sites/{SITE1_ID}/visits"), &token)
            .await;
    assert_eq!(status, 200, "{body}");
    let visits = body["visits"].as_array().unwrap();
    let corrected = cell(&visits[1], GLOBAL_PARAM_DO_ID).expect("DO cell at T1");
    assert_eq!(
        corrected["curves"],
        json!([{ "id": curve.to_string(), "name": "Curve 2026-03" }]),
        "both repeats share one curve, named once: {corrected}"
    );
    let uncorrected = cell(&visits[1], GLOBAL_PARAM_TEMP_ID).expect("Temp cell at T1");
    assert_eq!(uncorrected["curves"], json!([]), "{uncorrected}");
}

/// Scenario: a visit waiting on a manager's ruling, whose review-queue hold is keyed on the visit
/// and names no parameter.
///
/// Expected behaviour: the visit still opens, and no cell carries the visit's own hold.
#[tokio::test]
#[serial]
async fn a_pending_visit_opens_with_its_visit_hold_on_no_cell() {
    let (db, app, token) = setup().await;
    save_two_visits(&app, &token).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO replicate_audit_holds \
                 (site_id, group_time, kind, expected, computed, delta, status) \
             VALUES ('{SITE1_ID}', '{T1}', 'unverified_visit', '{{}}'::jsonb, '{{}}'::jsonb, \
                     '{{}}'::jsonb, 'pending')"
        ),
    )
    .await;
    let event_id: String = db
        .query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "SELECT id::text AS id FROM collection_events \
                 WHERE site_id = '{SITE1_ID}' AND collected_at = '{T1}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();

    let (status, body) = crate::common::get_json_with_token(
        &app,
        &format!("/api/collection_events/{event_id}/detail"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c.get("finding").is_none()),
        "{body}"
    );
}
