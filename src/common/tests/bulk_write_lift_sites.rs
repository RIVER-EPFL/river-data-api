use std::path::{Path, PathBuf};

/// The lift is `SET LOCAL`, so it exists only for the transaction that issued it, and a bulk
/// hypertable write that forgets it fails on the first compressed chunk it reaches. One
/// spelling, here: `guarded` cannot be called without it and `lift_decompression_cap` is the
/// one-liner for a transaction opened for other reasons. A tenth literal copy fails this test.
const ALLOWED: &[(&str, usize)] = &[("src/common/bulk_write.rs", 1)];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Occurrences of the setting's name in the non-comment, non-test part of a source file.
fn lift_literals(source: &str) -> usize {
    source
        .split("#[cfg(test)]")
        .next()
        .unwrap_or("")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("max_tuples_decompressed_per_dml_transaction"))
        .count()
}

#[test]
fn test_the_decompression_lift_is_spelled_once() {
    let root = &crate::test_crate_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    let mut found: Vec<(String, usize)> = files
        .iter()
        .filter_map(|f| {
            let n = lift_literals(&std::fs::read_to_string(f).expect("read file"));
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
    assert_eq!(
        found, expected,
        "a bulk write spells the lift itself; call lift_decompression_cap"
    );
}

#[test]
fn test_lift_literals_ignores_comments_and_test_modules() {
    assert_eq!(
        lift_literals("SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0"),
        1
    );
    assert_eq!(
        lift_literals("// max_tuples_decompressed_per_dml_transaction, default 100k"),
        0
    );
    assert_eq!(
        lift_literals("//! max_tuples_decompressed_per_dml_transaction"),
        0
    );
    assert_eq!(
        lift_literals(
            "fn live() {}\n#[cfg(test)]\nmod t { const S: &str = \"max_tuples_decompressed_per_dml_transaction\"; }"
        ),
        0
    );
}
