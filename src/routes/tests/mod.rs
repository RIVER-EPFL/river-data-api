/// The committed document is the one the router emits.
///
/// The other guards here read `docs/openapi.json` and check it against itself or against the
/// source structs, so a stale copy passes all three: a route added, renamed or removed leaves
/// the document describing the old surface until somebody regenerates it. The workflow that
/// regenerates and diffs runs only on `src/routes/**`, so a dependency bump that renames a
/// derived schema never reaches it, which is how three paths and nine schemas drifted.
#[test]
fn test_the_committed_document_is_the_one_the_router_emits() {
    let generated = super::committed_document().expect("the document serialises");
    let committed = include_str!("../../../docs/openapi.json");
    assert!(
        generated == committed,
        "docs/openapi.json no longer describes the router: {}. Regenerate it with:\n  \
         cargo run --bin dump_openapi -- docs/openapi.json",
        what_moved(&generated, committed)
    );
}

/// What differs between two documents, in one line: the paths and schemas one carries and the
/// other does not, or the first line they disagree on when the two sets match.
fn what_moved(generated: &str, committed: &str) -> String {
    fn keys(doc: &serde_json::Value, section: &str) -> std::collections::BTreeSet<String> {
        doc.pointer(section)
            .and_then(serde_json::Value::as_object)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }
    let (left, right): (serde_json::Value, serde_json::Value) = (
        serde_json::from_str(generated).expect("the generated document parses"),
        serde_json::from_str(committed).expect("the committed document parses"),
    );
    let mut moved = Vec::new();
    for (section, label) in [("/paths", "path"), ("/components/schemas", "schema")] {
        let (l, r) = (keys(&left, section), keys(&right, section));
        for name in l.difference(&r) {
            moved.push(format!("{label} {name} is served and not committed"));
        }
        for name in r.difference(&l) {
            moved.push(format!("{label} {name} is committed and not served"));
        }
    }
    if moved.is_empty() {
        let line = generated
            .lines()
            .zip(committed.lines())
            .position(|(a, b)| a != b)
            .map_or_else(
                || "the two are of different length".to_string(),
                |i| format!("first difference at line {}", i + 1),
            );
        return line;
    }
    moved.join(", ")
}

/// Every `$ref` in the committed document names a schema the document carries.
///
/// A dangling one is not a rendering blemish: a consumer that resolves the document, which is
/// what generating a client from it means, fails on the whole file rather than on that one
/// property. The document is the artefact `.github/workflows/openapi.yml` regenerates and
/// diffs, so asserting it here asserts what the router emits.
#[test]
fn test_every_schema_ref_in_the_committed_document_resolves() {
    const DOCUMENT: &str = include_str!("../../../docs/openapi.json");
    let doc: serde_json::Value = serde_json::from_str(DOCUMENT).expect("the document parses");
    let names: std::collections::BTreeSet<&str> = doc["components"]["schemas"]
        .as_object()
        .expect("the document declares schemas")
        .keys()
        .map(String::as_str)
        .collect();

    let mut dangling = std::collections::BTreeSet::new();
    collect_refs(&doc, &mut |r| {
        if let Some(name) = r.strip_prefix("#/components/schemas/") {
            if !names.contains(name) {
                dangling.insert(name.to_string());
            }
        }
    });
    assert!(
        dangling.is_empty(),
        "referenced and not declared: {dangling:?}"
    );
}

/// The document says which `Option` fields are sent and which are omitted.
///
/// utoipa marks every `Option<T>` not-required and nullable whatever serde does with it, so
/// both halves of the truth are lost. A field with no `skip_serializing_if` is always on the
/// wire and may be null: `#[schema(required)]`. A field with one is omitted when it is `None`
/// and is never null: `#[schema(nullable = false)]`. A struct that deserializes as well may be
/// read as a request, where an `Option` is genuinely optional; `#[serde(default)]` is how such
/// a field says so, and is what excuses it from the first rule. All three are derivable, so
/// they are asserted rather than trusted; a generated client that has to handle an absence or a
/// null that cannot happen is what stopped C114 replacing its hand-written types.
#[test]
fn test_the_document_says_which_optional_fields_are_sent_and_which_are_omitted() {
    let (checked, wrong) = optional_fields_the_document_misdescribes();
    assert!(
        checked > 0,
        "the scan found no schema struct at all, so it is asserting nothing"
    );
    assert!(
        wrong.is_empty(),
        "each is described as the opposite of what the wire does; \
         an always-sent field takes #[schema(required)] and an omitted one \
         #[schema(nullable = false)]: {wrong:?}"
    );
}

/// The scan the test above asserts on, and the one below proves can fail: how many schema
/// structs were examined, and which of their fields the document misdescribes.
fn optional_fields_the_document_misdescribes() -> (usize, Vec<String>) {
    let mut checked = 0;
    let mut wrong = Vec::new();
    let root = crate::test_crate_root().join("src");
    for path in rust_sources(root) {
        let src = std::fs::read_to_string(&path).expect("a source file reads");
        let (c, m) = scan(&src);
        checked += c;
        wrong.extend(m);
    }
    (checked, wrong)
}

/// The attributes are what keep the test above green, so the same scan over a source missing
/// them must report both kinds. Without this, a scan that matched nothing would pass just as
/// quietly, which is what the first version of it did.
#[test]
fn test_the_scan_reports_a_field_that_lost_its_attribute() {
    let source = "\
#[derive(Serialize, ToSchema)]
pub struct Answer {
pub id: Uuid,
#[schema(required)]
pub note: Option<String>,
#[serde(skip_serializing_if = \"Option::is_none\")]
#[schema(nullable = false)]
pub omitted: Option<String>,
#[serde(skip_serializing_if = \"Option::is_none\")]
pub omitted_unmarked: Option<String>,
pub bare: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct Ask {
#[serde(skip_serializing_if = \"Option::is_none\")]
pub also_omitted: Option<String>,
pub filter: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Both {
#[serde(default)]
pub asked: Option<String>,
pub answered: Option<String>,
}
";
    let (checked, wrong) = scan(source);
    assert_eq!(
        checked, 3,
        "every schema struct is scanned; only the rules differ"
    );
    assert_eq!(
        wrong,
        vec![
            "Answer.omitted_unmarked".to_string(),
            "Answer.bare".to_string(),
            "Ask.also_omitted".to_string(),
            "Both.answered".to_string(),
        ],
        "a request's plain Option is genuinely optional, and on a struct that travels both ways \
         `#[serde(default)]` is what says so"
    );
}

/// An attribute rustfmt has wrapped is one attribute, so the scan reads it over its brackets
/// rather than over lines: closing it at the newline lost `nullable = false` and reported a field
/// the document describes correctly. A wrapped `#[derive(...)]` decides which way the struct
/// travels, so it is read the same way.
#[test]
fn test_a_wrapped_attribute_is_read_whole() {
    let source = "\
#[derive(
    Serialize,
    ToSchema,
)]
pub struct Wrapped {
#[serde(skip_serializing_if = \"Option::is_none\")]
#[schema(
    value_type = Option<MeasurementType>,
    nullable = false
)]
pub described: Option<String>,
#[schema(
    required
)]
pub sent: Option<String>,
}
";
    let (checked, wrong) = scan(source);
    assert_eq!(checked, 1, "the wrapped derive still names a schema struct");
    assert!(
        wrong.is_empty(),
        "both fields carry the attribute the document needs: {wrong:?}"
    );
}

fn rust_sources(root: std::path::PathBuf) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs")
                // A sibling `tests/` file declares no served schema, and the fixture below is
                // source text the scan would otherwise read as one.
                && !path.components().any(|c| c.as_os_str() == "tests")
            {
                out.push(path);
            }
        }
    }
    out
}

/// Walk one source line by line: attributes accumulate, a `pub struct` line consumes them, and
/// a struct ends at a closing brace on its own indent. An attribute closes over its brackets, not
/// at the newline, so one rustfmt has wrapped is accumulated whole. Returns the schema structs
/// examined and the fields the document describes as the opposite of what the wire does.
fn scan(src: &str) -> (usize, Vec<String>) {
    let mut checked = 0;
    let mut missing = Vec::new();
    let mut attrs = Attributes::default();
    // struct name, its indent, and which way it travels
    let mut open: Option<(String, String, Travels)> = None;
    for line in src.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if let Some((name, struct_indent, kind)) = &open {
            if trimmed == "}" && indent == struct_indent {
                open = None;
                attrs.clear();
                continue;
            }
            if attrs.wants(trimmed) {
                attrs.push(trimmed);
                continue;
            }
            if let Some(field) = trimmed.strip_prefix("pub ")
                && let Some((field_name, ty)) = field.split_once(british_colon())
                && ty.trim_start().starts_with("Option<")
            {
                let omitted = attrs.contains("skip_serializing_if");
                // A struct that also deserializes may be read as a request, where an `Option`
                // is genuinely optional. `#[serde(default)]` is how such a field says so, so a
                // field without one is answering, not asking, whichever traits the struct has.
                let answered = match kind {
                    Travels::Response => true,
                    Travels::Request => false,
                    Travels::Both => !attrs.contains("serde(default"),
                };
                let misdescribed = if omitted {
                    !attrs.contains("nullable = false")
                } else {
                    answered && !attrs.contains("schema(required")
                };
                if misdescribed {
                    missing.push(format!("{name}.{field_name}"));
                }
            }
            if !trimmed.starts_with("///") && !trimmed.starts_with("//") {
                attrs.clear();
            }
            continue;
        }
        if attrs.wants(trimmed) || trimmed.starts_with("///") || trimmed.starts_with("//") {
            attrs.push(trimmed);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("pub struct ")
            && rest.ends_with('{')
            && attrs.contains("ToSchema")
        {
            checked += 1;
            let name = rest.trim_end_matches('{').trim();
            let name = name.split(['<', ' ']).next().unwrap_or(name);
            open = Some((name.to_string(), indent.to_string(), travels(&attrs.text)));
            attrs.clear();
            continue;
        }
        if !trimmed.is_empty() {
            attrs.clear();
        }
    }
    (checked, missing)
}

/// One field's or struct's accumulated attributes, closed over brackets rather than over lines:
/// rustfmt wraps a long `#[schema(...)]` or `#[derive(...)]` across several lines, and reading only
/// the first loses whatever the rest of it said.
#[derive(Default)]
struct Attributes {
    text: String,
    /// Unclosed `[` in the attribute being accumulated.
    depth: usize,
}

impl Attributes {
    /// Whether this line belongs to the attributes: a new one, or the continuation of one still
    /// open.
    fn wants(&self, trimmed: &str) -> bool {
        self.depth > 0 || trimmed.starts_with("#[")
    }

    fn push(&mut self, trimmed: &str) {
        self.text.push_str(trimmed);
        self.depth = (self.depth + trimmed.matches('[').count())
            .saturating_sub(trimmed.matches(']').count());
    }

    fn contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }

    fn clear(&mut self) {
        self.text.clear();
        self.depth = 0;
    }
}

const fn british_colon() -> char {
    ':'
}

/// Which way a schema struct travels, which is what decides whether an `Option` is a value the
/// API always sends or one a client may omit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Travels {
    /// Serializes only: every `Option` is on the wire, null when empty.
    Response,
    /// Deserializes only: every `Option` is the client's to omit.
    Request,
    /// Both, so the field itself says which, through `#[serde(default)]`.
    Both,
}

fn travels(attrs: &str) -> Travels {
    let Some(open) = attrs.find("#[derive(") else {
        return Travels::Request;
    };
    let derives = &attrs[open + "#[derive(".len()..];
    let Some(close) = derives.find(')') else {
        return Travels::Request;
    };
    let derives = &derives[..close];
    match (
        derives.contains("Serialize"),
        derives.contains("Deserialize"),
    ) {
        (true, false) => Travels::Response,
        (true, true) => Travels::Both,
        _ => Travels::Request,
    }
}

fn collect_refs(value: &serde_json::Value, found: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "$ref" {
                    if let Some(r) = child.as_str() {
                        found(r);
                    }
                }
                collect_refs(child, found);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_refs(item, found);
            }
        }
        _ => {}
    }
}
