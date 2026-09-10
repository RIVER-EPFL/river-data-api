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
    assert_eq!(
        records[1][1], "DOC, filtered",
        "the comma stays in the cell"
    );
    assert_eq!(records[1].len(), 3, "and does not shift the columns");
}
