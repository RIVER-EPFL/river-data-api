//! One table projection for every series endpoint, and the single point where the response format
//! is decided.
//!
//! A handler builds one [`Table`] (the shared time axis plus the export columns) and hands it,
//! together with its JSON arm, to [`respond`]. [`respond`] is the only way these handlers return,
//! the empty path included, so format parity is structural rather than remembered.
//!
//! The table is built only when a bulk format was asked for: [`respond`] takes the data once and
//! two builders over it, so a JSON request never pays for the projection.

use axum::http::header::{self, HeaderValue};
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::future::Future;

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

use crate::common::csv::field as csv_cell;

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
        let lines = render(&self)
            .into_iter()
            .map(|line| Ok::<_, std::io::Error>(format!("{line}\n")));

        Response::builder()
            .header(header::CONTENT_TYPE, HeaderValue::from_static(content_type))
            .body(axum::body::Body::from_stream(futures::stream::iter(lines)))
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
#[path = "tests/series.rs"]
mod tests;
