use std::path::{Path, PathBuf};

/// Every file under `src/` that still spells SQL as text (CID2), with its count of statements and
/// of `Expr::cust` fragments. A file that gains one fails this test until it is argued into the
/// list; a file that loses one fails it too, so the list is lowered as the text is rebuilt.
/// Tests are not scanned, neither the inline modules nor the sibling `tests/` directories.
///
/// An entry with a comment above it is a sanctioned site and says which; every other entry is text
/// still to be rebuilt as a built statement.
const ALLOWED: &[(&str, usize, usize)] = &[
    // The session's actor for the audit trigger, a setting on the write's own transaction.
    ("src/common/actor.rs", 1, 0),
    // The continuous-aggregate refresh, the one named exception (Q146).
    ("src/common/aggregates.rs", 1, 0),
    // `SET LOCAL` lifting the decompression cap on the write's transaction.
    ("src/common/bulk_write.rs", 1, 0),
    ("src/common/scope.rs", 5, 0),
    ("src/common/served.rs", 4, 5),
    // The advisory lock serialising migrations across replicas.
    ("src/main.rs", 2, 0),
    // The cutover binary's body, and the two function names its sample recompute calls
    // (`refresh_sample_aggregate` over `unnest`).
    ("src/restore.rs", 21, 2),
    // The health probe's `SELECT 1`; its two fragments are still to be rebuilt.
    ("src/routes/mod.rs", 1, 2),
    ("src/routes/private/alarms/service.rs", 1, 37),
    ("src/routes/private/alarms/views.rs", 1, 0),
    ("src/routes/private/collection_events/flows.rs", 0, 2),
    ("src/routes/private/collection_events/service.rs", 15, 6),
    ("src/routes/private/collection_events/views.rs", 0, 2),
    ("src/routes/private/data_streams/service.rs", 1, 16),
    ("src/routes/private/data_streams/views.rs", 4, 0),
    ("src/routes/private/derived_parameters/views.rs", 0, 2),
    ("src/routes/private/me.rs", 3, 0),
    ("src/routes/private/meteoswiss/service.rs", 1, 0),
    ("src/routes/private/notifications/flows.rs", 0, 8),
    ("src/routes/private/parameter_groups/service.rs", 4, 0),
    ("src/routes/private/parameter_groups/views.rs", 1, 0),
    ("src/routes/private/readings/flows.rs", 0, 5),
    ("src/routes/private/readings/service.rs", 11, 56),
    ("src/routes/private/reprocessing_jobs/service.rs", 1, 3),
    ("src/routes/private/reprocessing_jobs/views.rs", 1, 0),
    ("src/routes/private/sensor_calibrations/resolver.rs", 0, 9),
    ("src/routes/private/sensor_calibrations/service.rs", 2, 92),
    ("src/routes/private/sensor_calibrations/views.rs", 0, 19),
    ("src/routes/private/sensor_deployments/flows.rs", 2, 0),
    ("src/routes/private/sensor_deployments/service.rs", 1, 0),
    ("src/routes/private/sensor_deployments/views.rs", 0, 13),
    ("src/routes/private/sensors/service.rs", 7, 3),
    ("src/routes/private/sensors/views.rs", 5, 13),
    ("src/routes/private/site_parameters/service.rs", 2, 0),
    ("src/routes/private/sites/service.rs", 6, 17),
    ("src/routes/private/sites/views.rs", 1, 24),
    ("src/routes/private/standard_curves/views.rs", 1, 0),
    ("src/routes/private/sync/service.rs", 6, 23),
    ("src/routes/private/sync/views.rs", 4, 2),
    ("src/routes/private/tools/flows.rs", 0, 5),
    ("src/routes/private/tools/models.rs", 0, 1),
    ("src/routes/private/tools/service.rs", 0, 14),
    ("src/routes/public/service.rs", 1, 0),
    ("src/routes/public/views.rs", 1, 13),
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

/// A token of Rust source as far as this scan needs one: a string literal's contents, a run of
/// identifier characters, or one of the marks that delimit an attribute and the item it applies to.
#[derive(Debug, PartialEq)]
enum Token {
    Literal(String),
    Word(String),
    Mark(char),
}

/// Split source into words and string literals, dropping comments and character literals, so a
/// quote inside a comment or a `'"'` does not open a string.
fn tokens(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '\'' {
            // A character literal is `'x'` or `'\…'`; anything else is a lifetime.
            if next == Some('\\') {
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if chars.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else if c == 'r' && matches!(next, Some('"' | '#')) && !is_word_char_before(&chars, i) {
            let mut hashes = 0;
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if chars.get(j) != Some(&'"') {
                i += 1;
                continue;
            }
            let start = j + 1;
            let mut k = start;
            loop {
                if k >= chars.len() {
                    break;
                }
                if chars[k] == '"' && (1..=hashes).all(|h| chars.get(k + h) == Some(&'#')) {
                    break;
                }
                k += 1;
            }
            out.push(Token::Literal(
                chars[start..k.min(chars.len())].iter().collect(),
            ));
            i = k + 1 + hashes;
        } else if c == '"' {
            let mut text = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                    match chars.get(i) {
                        Some('n') => text.push('\n'),
                        Some('\n') => {
                            // A trailing backslash continues the literal past the line's indent.
                            while chars.get(i + 1).is_some_and(|c| c.is_whitespace()) {
                                i += 1;
                            }
                            text.push(' ');
                        }
                        Some(other) => text.push(*other),
                        None => {}
                    }
                } else {
                    text.push(chars[i]);
                }
                i += 1;
            }
            out.push(Token::Literal(text));
            i += 1;
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(Token::Word(chars[start..i].iter().collect()));
        } else {
            if matches!(c, '#' | '[' | ']' | '{' | '}' | ';') {
                out.push(Token::Mark(c));
            }
            i += 1;
        }
    }
    out
}

fn is_word_char_before(chars: &[char], i: usize) -> bool {
    i > 0 && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_')
}

/// Whether a literal is SQL: it holds one of the clause keywords, upper case, as a whole word.
/// SQL here is written in capitals and prose is not, which is what tells the two apart.
fn is_sql(text: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "SELECT",
        "INSERT",
        "UPDATE",
        "DELETE",
        "FROM",
        "WHERE",
        "JOIN",
        "VALUES",
        "CALL",
        "RETURNING",
        "TRUNCATE",
        "LOCK",
        "COALESCE",
        "EXISTS",
    ];
    let words: Vec<&str> = text
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    words.iter().any(|w| KEYWORDS.contains(w))
        || words.windows(2).any(|w| {
            matches!(
                w,
                ["SET", "LOCAL"] | ["ON", "CONFLICT"] | ["GROUP" | "ORDER", "BY"]
            )
        })
}

/// Whether `tokens` open with `#[cfg(test)]`.
fn is_cfg_test(tokens: &[Token]) -> bool {
    matches!(
        tokens,
        [Token::Mark('#'), Token::Mark('['), Token::Word(cfg), Token::Word(test), Token::Mark(']'), ..]
            if cfg == "cfg" && test == "test"
    )
}

/// How many tokens the attribute at the start of `tokens` spans, by bracket depth.
fn attribute_len(tokens: &[Token]) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate().skip(1) {
        match token {
            Token::Mark('[') => depth += 1,
            Token::Mark(']') => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// How many tokens the item at the start of `tokens` spans: to a `;` before any brace (`mod x;`,
/// `use …;`), or to the brace that closes its body.
fn item_len(tokens: &[Token]) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::Mark('{') => depth += 1,
            Token::Mark('}') => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            Token::Mark(';') if depth == 0 => return i + 1,
            _ => {}
        }
    }
    tokens.len()
}

/// `tokens` with every item a `#[cfg(test)]` attribute applies to left out, along with its
/// attributes. Only that item: what follows an inline test module is live code.
fn live(tokens: Vec<Token>) -> Vec<Token> {
    let mut keep = vec![true; tokens.len()];
    let mut i = 0;
    while i < tokens.len() {
        if !is_cfg_test(&tokens[i..]) {
            i += 1;
            continue;
        }
        let mut end = i;
        while matches!(&tokens[end..], [Token::Mark('#'), Token::Mark('['), ..]) {
            end += attribute_len(&tokens[end..]);
        }
        end += item_len(&tokens[end..]);
        keep[i..end.min(tokens.len())].fill(false);
        i = end;
    }
    tokens
        .into_iter()
        .zip(keep)
        .filter_map(|(token, kept)| kept.then_some(token))
        .collect()
}

/// The SQL text in the non-test part of a source file: `(statements, fragments)`. A fragment is an
/// `Expr::cust` call, whatever it holds; a statement is any other literal that reads as SQL.
fn sql_sites(source: &str) -> (usize, usize) {
    let tokens = live(tokens(source));
    let mut statements = 0;
    let mut fragments = 0;
    let mut after_cust = false;
    for token in &tokens {
        match token {
            Token::Word(w)
                if matches!(
                    w.as_str(),
                    "cust" | "cust_with_values" | "cust_with_expr" | "cust_with_exprs"
                ) =>
            {
                fragments += 1;
                after_cust = true;
            }
            // `Expr::cust(format!("…"))` holds its text one macro further in.
            Token::Word(w) if w == "format" => {}
            Token::Word(_) => after_cust = false,
            Token::Mark(_) => {}
            Token::Literal(text) => {
                if !after_cust && is_sql(text) {
                    statements += 1;
                }
                after_cust = false;
            }
        }
    }
    (statements, fragments)
}

#[test]
fn test_sql_text_appears_only_at_allowlisted_sites() {
    let root = &crate::test_crate_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    let found: Vec<(String, usize, usize)> = files
        .iter()
        .filter_map(|f| {
            let (statements, fragments) =
                sql_sites(&std::fs::read_to_string(f).expect("read file"));
            (statements + fragments > 0).then(|| {
                (
                    f.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                    statements,
                    fragments,
                )
            })
        })
        .collect();
    let expected: Vec<(String, usize, usize)> = ALLOWED
        .iter()
        .map(|(f, s, c)| ((*f).to_string(), *s, *c))
        .collect();
    let listing: String = found
        .iter()
        .map(|(f, s, c)| format!("    (\"{f}\", {s}, {c}),\n"))
        .collect();
    assert_eq!(
        found, expected,
        "the SQL text sites moved; ALLOWED as the tree stands:\n{listing}"
    );
}

#[test]
fn test_sql_sites_skips_only_the_item_a_cfg_test_attribute_applies_to() {
    let below_a_path_module = "#[cfg(test)]\n#[path = \"tests/x.rs\"]\nmod tests;\n\
                               fn live() { Expr::cust(\"x IS NULL\"); }";
    assert_eq!(
        sql_sites(below_a_path_module),
        (0, 1),
        "text after the module is live"
    );
    let below_an_inline_module = "pub mod admission {\n    fn a() {}\n    #[cfg(test)]\n    \
        mod tests { fn t() { let q = \"SELECT 1 FROM t\"; let b = '{'; } }\n}\n\
        fn live() { let q = \"DELETE FROM t\"; }";
    assert_eq!(
        sql_sites(below_an_inline_module),
        (1, 0),
        "the inline module is skipped by its braces, and what follows it is counted"
    );
    assert_eq!(
        sql_sites("#[cfg(test)]\nfn helper() { let q = \"SELECT 1 FROM t\"; }"),
        (0, 0),
        "a test-only function is skipped whole"
    );
}

#[test]
fn test_sql_sites_counts_statements_and_fragments() {
    assert_eq!(sql_sites(r#"let q = "SELECT id FROM sites";"#), (1, 0));
    assert_eq!(
        sql_sites("let q = r#\"INSERT INTO t (a) VALUES ($1)\"#;"),
        (1, 0),
        "a raw literal is read too"
    );
    assert_eq!(
        sql_sites(r#"Expr::cust("measurement_type IS DISTINCT FROM 'spot'")"#),
        (0, 1),
        "a fragment counts once, not again as a statement"
    );
    assert_eq!(
        sql_sites(r#"Expr::cust_with_values("r.provenance ->> $1", ["run_id"])"#),
        (0, 1)
    );
    assert_eq!(
        sql_sites(r#"Expr::cust(format!("{col} IS NOT NULL AND x IN (SELECT 1)"))"#),
        (0, 1),
        "nor when it is formatted"
    );
    assert_eq!(
        sql_sites(r#"let custom = "plain";"#),
        (0, 0),
        "a word beginning cust is not a fragment"
    );
    assert_eq!(
        sql_sites(r#"AppError::BadRequest("Select a site, then update it".into())"#),
        (0, 0),
        "prose is not SQL"
    );
    assert_eq!(
        sql_sites("// SELECT * FROM readings\nlet q = \"x\";"),
        (0, 0),
        "a comment is not a statement"
    );
    assert_eq!(
        sql_sites("let q = '\"'; let r = \"SELECT 1 FROM t\";"),
        (1, 0),
        "a quote character does not open a string"
    );
    assert_eq!(
        sql_sites("fn live() {}\n#[cfg(test)]\nmod t { const S: &str = \"SELECT 1 FROM t\"; }"),
        (0, 0),
        "an inline test module is not scanned"
    );
    assert_eq!(
        sql_sites("\"SET LOCAL timescaledb.max_tuples_decompressed_per_dml_transaction = 0\""),
        (1, 0)
    );
}
