use std::path::{Path, PathBuf};

/// Nothing deletes a reading except these statements, each a recorded decision: the
/// destructive replicate reconciliation (retired streams, admin-gated) and grab `replace`
/// mode (uncurated spot rows at the instant being re-entered). A new delete of readings
/// anywhere else fails this test until it is argued into the list. Tests are not scanned,
/// neither the inline modules nor the sibling `tests/` directories they live in; only live
/// code counts.
const ALLOWED: &[(&str, usize)] = &[
    ("src/routes/private/readings/views.rs", 1),
    ("src/routes/private/reprocessing_jobs/reconcile.rs", 1),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Count the deletes of readings in the non-test part of a source file, over any whitespace,
/// quoting or case. All three spellings count: `DELETE FROM readings…` as text, the sea-query
/// form, whose `from_table` names the entity (`from_table` exists only on a delete, so naming the
/// readings entity through it is a delete of readings and nothing else), and the entity form,
/// `readings::Entity::delete_many()`.
fn delete_statements(source: &str) -> usize {
    let live = source.split("#[cfg(test)]").next().unwrap_or("");
    let lowered = live.to_ascii_lowercase();
    let tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    let spelled = tokens
        .windows(3)
        .filter(|w| w[0] == "delete" && w[1] == "from" && w[2].starts_with("readings"))
        .count();
    let built = tokens
        .windows(2)
        .filter(|w| w[0] == "from_table" && w[1].starts_with("readings"))
        .count();
    let entity = tokens
        .windows(3)
        .filter(|w| w[0].starts_with("readings") && w[1] == "entity" && w[2].starts_with("delete"))
        .count();
    spelled + built + entity
}

#[test]
fn test_delete_from_readings_appears_only_at_allowlisted_sites() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    let mut found: Vec<(String, usize)> = files
        .iter()
        .filter_map(|f| {
            let n = delete_statements(&std::fs::read_to_string(f).expect("read file"));
            (n > 0).then(|| {
                (
                    f.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                    n,
                )
            })
        })
        .collect();
    found.sort();
    let expected: Vec<(String, usize)> = ALLOWED
        .iter()
        .map(|(f, n)| ((*f).to_string(), *n))
        .collect();
    assert_eq!(found, expected, "the delete sites moved; see ALLOWED");
}

#[test]
fn test_delete_statements_counts_across_lines_quoting_and_case() {
    assert_eq!(delete_statements("DELETE FROM readings WHERE x"), 1);
    assert_eq!(delete_statements("delete\n  from\n  readings r"), 1);
    assert_eq!(
        delete_statements("SeaQuery::delete().from_table(readings::Entity)"),
        1,
        "the built form counts too"
    );
    assert_eq!(
        delete_statements("SeaQuery::delete().from_table(samples::Entity)"),
        0,
        "another table's delete is not one of these"
    );
    assert_eq!(
        delete_statements("readings::Entity::delete_many().filter(x)"),
        1,
        "the entity form counts too"
    );
    assert_eq!(
        delete_statements("samples::Entity::delete_many().filter(x)"),
        0,
        "another table's entity delete is not one of these"
    );
    assert_eq!(
        delete_statements("readings::Entity::find().filter(x)"),
        0,
        "a read through the entity is not a delete"
    );
    assert_eq!(delete_statements(r#"r"DELETE FROM readings WHERE""#), 1);
    assert_eq!(delete_statements("DELETE FROM readings_hourly"), 1);
    assert_eq!(delete_statements("DELETE FROM samples"), 0);
    assert_eq!(
        delete_statements(
            "fn live() {}\n#[cfg(test)]\nmod t { const S: &str = \"DELETE FROM readings\"; }"
        ),
        0
    );
}
