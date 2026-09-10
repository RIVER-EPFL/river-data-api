use super::*;

fn table() -> Table {
    let mut t = Table::new(vec!["t0".to_string(), "t1".to_string()]);
    t.column("Depth", Cells::Float(vec![Some(1.5), None]));
    t.column("Depth_parameter_id", Cells::Constant("p-1".to_string()));
    t
}

#[test]
fn test_header_and_rows_align() {
    let t = table();
    assert_eq!(t.header_line(), "time,Depth,Depth_parameter_id");
    assert_eq!(t.csv_line(0), "t0,1.5,p-1");
}

#[test]
fn test_a_missing_float_is_an_empty_cell_not_a_zero() {
    assert_eq!(table().csv_line(1), "t1,,p-1");
}

#[test]
fn test_a_missing_float_is_null_in_ndjson() {
    let line = table().ndjson_line(1);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(v["Depth"].is_null(), "{line}");
    assert_eq!(v["Depth_parameter_id"].as_str(), Some("p-1"));
}

#[test]
fn test_an_empty_table_still_has_a_header() {
    let t = Table::new(Vec::new());
    assert_eq!(t.header_line(), "time");
    assert_eq!(t.row_count(), 0);
}

#[test]
fn test_an_empty_table_with_columns_keeps_them_in_the_header() {
    let mut t = Table::new(Vec::new());
    t.column("Depth", Cells::Float(Vec::new()));
    assert_eq!(t.header_line(), "time,Depth");
    assert_eq!(t.row_count(), 0);
}

#[test]
fn test_a_single_row_renders_once() {
    let mut t = Table::new(vec!["t0".to_string()]);
    t.column("Depth", Cells::Float(vec![Some(2.0)]));
    assert_eq!(t.row_count(), 1);
    assert_eq!(t.csv_line(0), "t0,2");
}

#[test]
fn test_an_all_null_column_renders_empty_everywhere() {
    let mut t = Table::new(vec!["t0".to_string(), "t1".to_string()]);
    t.column("Depth", Cells::Float(vec![None, None]));
    assert_eq!(t.csv_line(0), "t0,");
    assert_eq!(t.csv_line(1), "t1,");
    let v: serde_json::Value = serde_json::from_str(&t.ndjson_line(0)).unwrap();
    assert!(v["Depth"].is_null());
}

#[test]
fn test_a_short_column_reads_as_missing_rather_than_panicking() {
    let mut t = Table::new(vec!["t0".to_string(), "t1".to_string()]);
    t.column("Depth", Cells::Float(vec![Some(1.0)]));
    assert_eq!(t.csv_line(1), "t1,");
}

#[test]
fn test_text_with_a_comma_is_quoted() {
    let mut t = Table::new(vec!["t0".to_string()]);
    t.column(
        "reason",
        Cells::Text(vec![Some("out of water, drifting".to_string())]),
    );
    assert_eq!(t.csv_line(0), "t0,\"out of water, drifting\"");
}

#[test]
fn test_text_with_a_quote_doubles_it() {
    let mut t = Table::new(vec!["t0".to_string()]);
    t.column("reason", Cells::Text(vec![Some("said \"no\"".to_string())]));
    assert_eq!(t.csv_line(0), "t0,\"said \"\"no\"\"\"");
}

#[test]
fn test_plain_text_is_not_quoted() {
    let mut t = Table::new(vec!["t0".to_string()]);
    t.column(
        "measurement_type",
        Cells::Text(vec![Some("spot".to_string())]),
    );
    assert_eq!(t.csv_line(0), "t0,spot");
}

#[test]
fn test_bool_and_int_cells() {
    let mut t = Table::new(vec!["t0".to_string(), "t1".to_string()]);
    t.column("flagged", Cells::Bool(vec![Some(true), None]));
    t.column("count", Cells::Int(vec![Some(3), Some(0)]));
    assert_eq!(t.csv_line(0), "t0,true,3");
    assert_eq!(t.csv_line(1), "t1,,0");
    let v: serde_json::Value = serde_json::from_str(&t.ndjson_line(1)).unwrap();
    assert!(v["flagged"].is_null());
    assert_eq!(v["count"].as_i64(), Some(0));
}

#[tokio::test]
async fn test_respond_runs_only_the_arm_the_format_asked_for() {
    let json = |_data: u8| async { crate::common::cache::json_response(b"{}".to_vec(), false) };
    let response = respond(
        "csv",
        1u8,
        |_| {
            let mut t = Table::new(vec!["t0".to_string()]);
            t.column("Depth", Cells::Float(vec![Some(1.0)]));
            t
        },
        json,
    )
    .await
    .unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/csv")
    );
}

#[tokio::test]
async fn test_respond_falls_through_to_json_for_any_other_format() {
    let response = respond(
        "json",
        1u8,
        |_| Table::new(Vec::new()),
        |_| async { crate::common::cache::json_response(b"{}".to_vec(), false) },
    )
    .await
    .unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
}
