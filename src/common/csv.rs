//! The one CSV writer every export uses, so a field is quoted by one rule.
//!
//! Quoting is RFC 4180 as the `csv` crate implements it: a field carrying the delimiter, a quote,
//! `\n` or `\r` is quoted and its quotes doubled, and nothing else is. The crate was already a
//! dependency, read-only, in the CSV importer.

use std::fmt::Write as _;

/// Accumulates CSV rows in memory. Fields are borrowed as strings and written verbatim except for
/// the quoting.
pub struct CsvWriter {
    inner: csv::Writer<Vec<u8>>,
}

impl Default for CsvWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl CsvWriter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: csv::WriterBuilder::new()
                .quote_style(csv::QuoteStyle::Necessary)
                .terminator(csv::Terminator::Any(b'\n'))
                .from_writer(Vec::new()),
        }
    }

    /// Write one record. A field the writer cannot serialise is dropped rather than propagated:
    /// the underlying writer only fails on I/O, and this one writes into a `Vec`.
    pub fn row<I, S>(&mut self, fields: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        let _ = self.inner.write_record(fields);
    }

    #[must_use]
    pub fn finish(mut self) -> String {
        let _ = self.inner.flush();
        self.inner
            .into_inner()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }
}

/// One record as a line, terminator included. For the streaming exports that send a row at a time
/// rather than building a document.
pub fn row_to_string<I, S>(fields: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<[u8]>,
{
    let mut w = CsvWriter::new();
    w.row(fields);
    w.finish()
}

/// A single field, quoted only where the rule requires it. For the exports that assemble a line by
/// hand around values that are already safe (timestamps, uuids, numbers).
#[must_use]
pub fn field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
            if c == '"' {
                out.push('"');
            }
            let _ = write!(out, "{c}");
        }
        out.push('"');
        out
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_quotes_only_what_needs_it() {
        assert_eq!(field("DOC_avg_ppb"), "DOC_avg_ppb");
        assert_eq!(field("a,b"), "\"a,b\"");
        assert_eq!(field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(field("two\nlines"), "\"two\nlines\"");
        assert_eq!(field("carriage\rreturn"), "\"carriage\rreturn\"");
        assert_eq!(field(""), "");
    }

    /// The two must agree, because some exports assemble a line around already-safe values and
    /// others write whole records. Compared inside a two-field record: a record of one empty field
    /// is the one case where they differ, since the writer must emit `""` there to keep it
    /// distinguishable from an empty record, and no export writes a single-field row.
    #[test]
    fn field_agrees_with_the_record_writer() {
        for value in [
            "plain",
            "a,b",
            "say \"hi\"",
            "two\nlines",
            "carriage\rreturn",
            "",
        ] {
            let line = row_to_string(["x", value]);
            assert_eq!(
                line,
                format!("x,{}\n", field(value)),
                "the single-field helper must quote exactly as the record writer does, on {value:?}"
            );
        }
    }

    #[test]
    fn a_row_round_trips_through_a_reader() {
        let mut w = CsvWriter::new();
        w.row(["time", "parameter", "value"]);
        w.row(["2025-01-01T00:00:00Z", "DOC, filtered", "1.5"]);
        let out = w.finish();

        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(out.as_bytes());
        let records: Vec<Vec<String>> = rdr
            .records()
            .map(|r| r.unwrap().iter().map(ToOwned::to_owned).collect())
            .collect();
        assert_eq!(records[1][1], "DOC, filtered", "the comma stays in the cell");
        assert_eq!(records[1].len(), 3, "and does not shift the columns");
    }
}
