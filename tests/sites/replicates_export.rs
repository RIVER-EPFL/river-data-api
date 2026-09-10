//! The replicates download beside the site export.
//!
//! Statistics belong to the instant and replicates to the measurement, so they are two files, and
//! `{code}_sample_id` on the instant row is what joins them.
//!
//! Run: cargo test --test sites replicates_export -- --test-threads=1

use serial_test::serial;

const AT: &str = "2025-04-01T09:00:00Z";
const WINDOW: &str = "start=2025-04-01T00:00:00Z&end=2025-04-01T23:59:59Z";

fn column(header: &str, name: &str) -> usize {
    header
        .split(',')
        .position(|c| c == name)
        .unwrap_or_else(|| panic!("column {name} missing from header: {header}"))
}

#[tokio::test]
#[serial]
async fn a_replicate_row_names_the_sample_the_instant_row_carries() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let site = crate::common::SITE1_ID;
    let parameter = crate::common::GLOBAL_PARAM_TURB_ID;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": site,
            "readings": [
                {"parameter_id": parameter, "value": 10.0, "time": AT, "replicate_index": 0},
                {"parameter_id": parameter, "value": 12.0, "time": AT, "replicate_index": 1},
                {"parameter_id": parameter, "value": 14.0, "time": AT, "replicate_index": 2},
            ],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {body}");

    let (status, csv) = crate::common::get_csv_with_token(
        &app,
        &format!(
            "/api/sites/{site}/readings?{WINDOW}&format=csv&measurement_type=spot\
             &include_sample_stats=true"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "site export ({status}): {csv}");
    let mut lines = csv.lines();
    let header = lines.next().expect("header");
    let row = lines.next().expect("one instant row");
    let cells: Vec<&str> = row.split(',').collect();
    assert_eq!(cells[column(header, "Turbidity_n")], "3", "{csv}");
    assert_eq!(cells[column(header, "Turbidity_mean")], "12", "{csv}");
    let sample_id = cells[column(header, "Turbidity_sample_id")].to_string();
    assert!(!sample_id.is_empty(), "the join key is filled: {csv}");

    let (status, csv) = crate::common::get_csv_with_token(
        &app,
        &format!("/api/sites/{site}/export/replicates?{WINDOW}&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "replicates export ({status}): {csv}");
    let mut lines = csv.lines();
    let header = lines.next().expect("header");
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    assert_eq!(rows.len(), 3, "one row per replicate: {csv}");
    let mut values: Vec<&str> = rows.iter().map(|r| r[column(header, "value")]).collect();
    values.sort_unstable();
    assert_eq!(values, ["10", "12", "14"], "{csv}");
    for row in &rows {
        assert_eq!(
            row[column(header, "sample_id")],
            sample_id,
            "every replicate names the instant's sample: {csv}"
        );
        assert_eq!(row[column(header, "parameter")], "Turbidity", "{csv}");
        assert_eq!(row[column(header, "flagged")], "false", "{csv}");
    }
    let mut indices: Vec<&str> = rows
        .iter()
        .map(|r| r[column(header, "replicate_index")])
        .collect();
    indices.sort_unstable();
    assert_eq!(indices, ["0", "1", "2"], "{csv}");
}

/// Expected behaviour: a single measurement forms no sample, so it has no join key and no
/// replicate rows of its own.
#[tokio::test]
#[serial]
async fn a_lone_measurement_has_no_replicate_rows() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let site = crate::common::SITE1_ID;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": site,
            "readings": [{
                "parameter_id": crate::common::GLOBAL_PARAM_TURB_ID,
                "value": 10.0,
                "time": AT,
            }],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grab entry ({status}): {body}");

    let (status, csv) = crate::common::get_csv_with_token(
        &app,
        &format!("/api/sites/{site}/export/replicates?{WINDOW}&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "replicates export ({status}): {csv}");
    assert_eq!(csv.lines().count(), 1, "header only: {csv}");
}

/// Scenario: a parameter code carrying a comma and a quote reaches the replicates export.
/// Expected behaviour: the cell round trips and the row keeps its column count. Codes are
/// operator-authored, so an export that concatenates them shifts every later column silently.
#[tokio::test]
#[serial]
async fn an_operator_authored_code_does_not_shift_the_columns() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());
    let site = crate::common::SITE1_ID;
    let parameter = crate::common::GLOBAL_PARAM_TURB_ID;

    let awkward = "DOC, \"filtered\"";
    crate::common::exec(
        &db,
        &format!(
            "UPDATE parameters SET code = '{}' WHERE id = '{parameter}'",
            awkward.replace('\'', "''")
        ),
    )
    .await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/grab_samples",
        &serde_json::json!({
            "site_id": site,
            "readings": [
                {"parameter_id": parameter, "value": 10.0, "time": AT, "replicate_index": 0},
                {"parameter_id": parameter, "value": 12.0, "time": AT, "replicate_index": 1},
            ],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "grab save ({status}): {body}");

    let (status, csv) = crate::common::get_csv_with_token(
        &app,
        &format!("/api/sites/{site}/export/replicates?{WINDOW}&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "replicates export ({status}): {csv}");

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(csv.as_bytes());
    let headers = rdr.headers().expect("header").clone();
    let width = headers.len();
    let records: Vec<csv::StringRecord> = rdr.records().map(Result::unwrap).collect();
    assert_eq!(records.len(), 2, "one row per replicate: {csv}");
    let index = headers
        .iter()
        .position(|c| c == "parameter")
        .expect("parameter column");
    for record in &records {
        assert_eq!(record.len(), width, "the row keeps its column count: {csv}");
        assert_eq!(&record[index], awkward, "the code round trips: {csv}");
    }
}
