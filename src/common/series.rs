//! One table projection for every series endpoint, and the single point where the response format
//! is decided.
//!
//! Each series endpoint used to encode its rows three times, with the format chosen below the
//! handler's early returns. That is why an annotation the caller opted into appeared in JSON and
//! not in CSV, why one endpoint filled a non-violating cell with `0.0` while its siblings used
//! `null`, and why an empty result answered as JSON whatever `?format` said.
//!
//! A handler now builds one [`Table`] (the shared time axis plus the export columns) and hands it,
//! together with its JSON arm, to [`respond`]. [`respond`] is the only way these handlers return,
//! the empty path included, so format parity is structural rather than remembered.
//!
//! The table is built only when a bulk format was asked for: [`respond`] takes the data once and
//! two builders over it, so a JSON request never pays for the projection.

use axum::http::header::{self, HeaderValue};
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::future::Future;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::{AppError, AppResult};

/// The cells of one export column, aligned with the table's time axis.
///
/// Every variant carries `Option`s: a cell with no value is empty in CSV and `null` in NDJSON,
/// never a stand-in number.
#[derive(Debug, Clone)]
pub enum Cells {
    /// One value repeated on every row, for a per-parameter constant such as its id.
    Constant(String),
    Float(Vec<Option<f64>>),
    Int(Vec<Option<i64>>),
    Bool(Vec<Option<bool>>),
    Text(Vec<Option<String>>),
}

impl Cells {
    fn csv_at(&self, index: usize) -> String {
        match self {
            Self::Constant(v) => csv_cell(v),
            Self::Float(v) => v
                .get(index)
                .and_then(|c| *c)
                .map(|f| f.to_string())
                .unwrap_or_default(),
            Self::Int(v) => v
                .get(index)
                .and_then(|c| *c)
                .map(|i| i.to_string())
                .unwrap_or_default(),
            Self::Bool(v) => v
                .get(index)
                .and_then(|c| *c)
                .map(|b| b.to_string())
                .unwrap_or_default(),
            Self::Text(v) => v
                .get(index)
                .and_then(Option::as_deref)
                .map(csv_cell)
                .unwrap_or_default(),
        }
    }

    fn json_at(&self, index: usize) -> serde_json::Value {
        match self {
            Self::Constant(v) => serde_json::Value::String(v.clone()),
            Self::Float(v) => v
                .get(index)
                .and_then(|c| *c)
                .map_or(serde_json::Value::Null, |f| serde_json::json!(f)),
            Self::Int(v) => v
                .get(index)
                .and_then(|c| *c)
                .map_or(serde_json::Value::Null, |i| serde_json::json!(i)),
            Self::Bool(v) => v
                .get(index)
                .and_then(|c| *c)
                .map_or(serde_json::Value::Null, |b| serde_json::json!(b)),
            Self::Text(v) => v
                .get(index)
                .and_then(Option::as_ref)
                .map_or(serde_json::Value::Null, |s| serde_json::json!(s)),
        }
    }
}

/// A quoted CSV field, per RFC 4180: quotes are doubled and only a field that needs it is quoted,
/// so a plain value renders exactly as it always has.
fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[derive(Debug, Clone)]
struct Column {
    header: String,
    cells: Cells,
}

/// A shared time axis plus the columns exported against it.
///
/// Columns are emitted in the order they were pushed, so a handler controls its own column
/// grouping (the readings export groups by kind across parameters, the aggregates export groups
/// the four statistics per parameter).
#[derive(Debug, Clone, Default)]
pub struct Table {
    times: Vec<String>,
    columns: Vec<Column>,
}

impl Table {
    /// A table over pre-formatted timestamps, for a tier with its own time format.
    #[must_use]
    pub fn new(times: Vec<String>) -> Self {
        Self {
            times,
            columns: Vec::new(),
        }
    }

    /// A table over RFC 3339 timestamps.
    #[must_use]
    pub fn at(times: &[DateTime<Utc>]) -> Self {
        Self::new(times.iter().map(DateTime::to_rfc3339).collect())
    }

    pub fn column(&mut self, header: impl Into<String>, cells: Cells) {
        self.columns.push(Column {
            header: header.into(),
            cells,
        });
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.times.len()
    }

    /// The CSV header row, without its trailing newline.
    #[must_use]
    pub fn header_line(&self) -> String {
        let mut line = "time".to_string();
        for column in &self.columns {
            line.push(',');
            line.push_str(&csv_cell(&column.header));
        }
        line
    }

    /// One CSV data row, without its trailing newline.
    #[must_use]
    pub fn csv_line(&self, index: usize) -> String {
        let mut line = csv_cell(self.times.get(index).map_or("", String::as_str));
        for column in &self.columns {
            line.push(',');
            line.push_str(&column.cells.csv_at(index));
        }
        line
    }

    /// One NDJSON object, without its trailing newline.
    #[must_use]
    pub fn ndjson_line(&self, index: usize) -> String {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "time".to_string(),
            serde_json::json!(self.times.get(index).map_or("", String::as_str)),
        );
        for column in &self.columns {
            obj.insert(column.header.clone(), column.cells.json_at(index));
        }
        serde_json::Value::Object(obj).to_string()
    }

    /// Stream the table as CSV. An empty table still answers with its header row.
    fn into_csv(self) -> AppResult<Response> {
        self.stream("text/csv", |table| {
            let mut lines = Vec::with_capacity(table.row_count() + 1);
            lines.push(table.header_line());
            for i in 0..table.row_count() {
                lines.push(table.csv_line(i));
            }
            lines
        })
    }

    /// Stream the table as NDJSON. An empty table answers with an empty body.
    fn into_ndjson(self) -> AppResult<Response> {
        self.stream("application/x-ndjson", |table| {
            (0..table.row_count())
                .map(|i| table.ndjson_line(i))
                .collect()
        })
    }

    fn stream(
        self,
        content_type: &'static str,
        render: fn(&Self) -> Vec<String>,
    ) -> AppResult<Response> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(100);

        tokio::spawn(async move {
            for line in render(&self) {
                if tx.send(Ok(format!("{line}\n"))).await.is_err() {
                    break;
                }
            }
        });

        Response::builder()
            .header(header::CONTENT_TYPE, HeaderValue::from_static(content_type))
            .body(axum::body::Body::from_stream(ReceiverStream::new(rx)))
            .map_err(|e| AppError::Internal(e.to_string()))
    }
}

/// Return a series in the format the caller asked for.
///
/// `data` is whatever the handler assembled; `table` projects it for the bulk formats and `json`
/// consumes it for the default format. Only one of the two runs, so JSON pays nothing for the
/// projection and CSV never has to reproduce the JSON body's shape by hand.
pub async fn respond<D, T, J, Fut>(format: &str, data: D, table: T, json: J) -> AppResult<Response>
where
    T: FnOnce(&D) -> Table,
    J: FnOnce(D) -> Fut,
    Fut: Future<Output = AppResult<Response>>,
{
    match format {
        "csv" => table(&data).into_csv(),
        "ndjson" => table(&data).into_ndjson(),
        _ => json(data).await,
    }
}

#[cfg(test)]
mod tests {
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
}
