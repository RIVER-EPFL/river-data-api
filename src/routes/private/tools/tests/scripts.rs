use super::{
    FORBIDDEN, FUNCTION_ARGS, LIBRARY_WHITELIST, NAME_RESOLVERS, findings_from_scan,
    forbidden_reason,
};
use crate::routes::private::tools::models::ScannedArg;
use crate::routes::private::tools::models::ScannedName;
use crate::routes::private::tools::models::ScriptScan;

fn scan(calls: Vec<(&str, i64)>, symbols: Vec<(&str, i64)>, args: Vec<ScannedArg>) -> ScriptScan {
    ScriptScan {
        parse_ok: true,
        parse_error: None,
        calls: calls
            .into_iter()
            .map(|(name, line)| ScannedName {
                name: name.to_string(),
                line,
            })
            .collect(),
        symbols: symbols
            .into_iter()
            .map(|(name, line)| ScannedName {
                name: name.to_string(),
                line,
            })
            .collect(),
        args,
    }
}

fn arg(call: &str, name: &str, value: &str, kind: &str, line: i64) -> ScannedArg {
    ScannedArg {
        call: call.to_string(),
        name: name.to_string(),
        value: value.to_string(),
        kind: kind.to_string(),
        line,
    }
}

/// R matches an argument name by prefix, so every prefix of `file` opens a file.
#[test]
fn an_abbreviated_file_argument_is_read_as_file() {
    for abbreviation in ["f", "fi", "fil", "file"] {
        let findings = findings_from_scan(&scan(
            vec![("cat", 4)],
            vec![],
            vec![arg("cat", abbreviation, "out.txt", "string", 4)],
        ));
        assert_eq!(findings.len(), 1, "{abbreviation}: {findings:?}");
        assert_eq!(findings[0].line, 4);
        assert!(findings[0].message.contains("'cat'"), "{findings:?}");
    }
    let sep_only = findings_from_scan(&scan(
        vec![("cat", 4)],
        vec![],
        vec![arg("cat", "sep", " ", "string", 4)],
    ));
    assert!(sep_only.is_empty(), "{sep_only:?}");
}

/// The same name reached three ways is the same finding, and each carries the line it was
/// reached on.
#[test]
fn an_alias_and_a_resolved_name_are_reported_like_a_call() {
    let findings = findings_from_scan(&scan(
        vec![("system", 1), ("do.call", 3)],
        vec![("system", 2)],
        vec![arg("do.call", "", "system", "string", 3)],
    ));
    let at = |line: usize| -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f.line == line)
            .map(|f| f.message.as_str())
            .collect()
    };
    for line in [1, 2, 3] {
        assert!(
            at(line).iter().any(|m| m.contains("'system'")),
            "line {line}: {findings:?}"
        );
    }
}

#[test]
fn a_namespaced_call_is_read_as_its_package_and_its_function() {
    let internals = findings_from_scan(&scan(vec![("dplyr:::select", 1)], vec![], vec![]));
    assert!(
        internals.iter().any(|f| f.message.contains(":::")),
        "{internals:?}"
    );
    let outside = findings_from_scan(&scan(vec![("curl::curl_fetch_memory", 2)], vec![], vec![]));
    assert!(
        outside.iter().any(|f| f.message.contains("'curl'")),
        "{outside:?}"
    );
    let through_base = findings_from_scan(&scan(vec![("base::system", 3)], vec![], vec![]));
    assert!(
        through_base.iter().any(|f| f.message.contains("'system'")),
        "{through_base:?}"
    );
}

/// The resolvers are refused as calls in their own right, so a name built at run time is
/// stopped where a string literal would have been read.
#[test]
fn every_name_resolver_is_forbidden_in_its_own_right() {
    for resolver in NAME_RESOLVERS {
        assert!(
            forbidden_reason(resolver).is_some(),
            "{resolver} resolves names but is allowed"
        );
    }
}

/// A namespaced name read in value position is the same three questions as a namespaced call:
/// `runner <- base::system` reaches `system` and `x <- curl::handle` names a package that is
/// not in the image.
#[test]
fn a_namespaced_name_is_read_the_same_called_or_assigned() {
    let assigned = findings_from_scan(&scan(vec![], vec![("base::system", 4)], vec![]));
    assert!(
        assigned.iter().any(|f| f.message.contains("'system'")),
        "{assigned:?}"
    );
    assert_eq!(assigned[0].line, 4);

    let outside = findings_from_scan(&scan(vec![], vec![("curl::handle", 2)], vec![]));
    assert!(
        outside.iter().any(|f| f.message.contains("'curl'")),
        "{outside:?}"
    );

    let internals = findings_from_scan(&scan(vec![], vec![("dplyr:::select", 6)], vec![]));
    assert!(
        internals.iter().any(|f| f.message.contains(":::")),
        "{internals:?}"
    );
}

/// `lapply(x, "system")` passes its string through `match.fun`, so the string is the call.
#[test]
fn a_function_named_as_a_string_is_read_as_that_function() {
    for call in FUNCTION_ARGS {
        let findings = findings_from_scan(&scan(
            vec![(call, 3)],
            vec![],
            vec![arg(call, "", "system", "string", 3)],
        ));
        assert!(
            findings.iter().any(|f| f.message.contains("'system'")),
            "{call}: {findings:?}"
        );
    }
    let ordinary = findings_from_scan(&scan(
        vec![("lapply", 3)],
        vec![],
        vec![arg("lapply", "", "mean", "string", 3)],
    ));
    assert!(ordinary.is_empty(), "{ordinary:?}");
}

#[test]
fn no_whitelisted_package_shares_a_name_with_a_forbidden_construct() {
    for package in LIBRARY_WHITELIST {
        assert!(
            !FORBIDDEN.iter().any(|(name, _)| name == package),
            "{package} is both allowed and forbidden"
        );
    }
}

/// jsonlite is installed in the runner image but is not a script-facing package, so the finding
/// states the rule a script is held to rather than what the image holds.
#[test]
fn a_package_outside_the_script_set_is_refused_as_not_allowed() {
    let called = findings_from_scan(&scan(vec![("jsonlite::toJSON", 2)], vec![], vec![]));
    assert_eq!(
        called[0].message, "package 'jsonlite' is not allowed in tool scripts",
        "{called:?}"
    );
    let loaded = findings_from_scan(&scan(
        vec![("library", 1)],
        vec![],
        vec![arg("library", "", "jsonlite", "string", 1)],
    ));
    assert!(
        loaded
            .iter()
            .any(|f| f.message == "package 'jsonlite' is not allowed in tool scripts"),
        "{loaded:?}"
    );
}
