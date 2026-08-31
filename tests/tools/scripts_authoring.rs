//! What identifies a version, what it records, who it credits, and what the lint and the
//! validator refuse.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;

use crate::common::keycloak as kc;

const ADMIN_EMAIL: &str = "scriptadmin@test.local";

async fn remove_script(db: &DatabaseConnection, name: &str) {
    for sql in [
        "UPDATE tool_scripts SET active_version_id = NULL WHERE name = $1",
        "DELETE FROM tool_scripts WHERE name = $1",
    ] {
        db.execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            sql,
            [name.into()],
        ))
        .await
        .expect("script removed");
    }
}

/// The app with an Administrator JWT, and the named script cleared so a rerun starts from
/// nothing (`tool_scripts` outlives the truncation, the seeded tools live there).
async fn setup(script_name: &str) -> (axum::Router, String, String) {
    let (app, admin, id, _db) = setup_with_db(script_name).await;
    (app, admin, id)
}

async fn setup_with_db(script_name: &str) -> (axum::Router, String, String, DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    remove_script(&db, script_name).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    kc::ensure_realm_user("scriptadmin", "scriptadmin", &["riverdata-admin"]).await;
    let admin = kc::get_keycloak_jwt("scriptadmin", "scriptadmin").await;

    let (status, script) = crate::common::post_json_parse_with_token(
        &app,
        "/api/tool_scripts",
        &json!({ "name": script_name, "label": "Authoring probe" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "script created: {script}");
    let id = script["id"].as_str().unwrap().to_string();
    (app, admin, id, db)
}

const DOUBLER: &str = "tool <- function(inputs, constants, curves) list(doubled = 2 * inputs$x)";

fn manifest(label: &str) -> serde_json::Value {
    json!({
        "label": label,
        "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
        "outputs": [ { "key": "doubled", "label": "Doubled", "per_replicate": false } ],
    })
}

fn cases(expected: f64) -> serde_json::Value {
    json!({ "tolerance": 1e-9, "cases": [
        { "name": "two", "inputs": { "x": 2 }, "expected": { "doubled": expected } } ] })
}

#[tokio::test]
#[serial]
async fn a_version_is_identified_by_its_whole_content_and_carries_a_note() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_version_is_identified_by_its_whole_content_and_carries_a_note",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_content_identity").await {
        return;
    }
    let (app, admin, sid) = setup("content_identity").await;
    let versions = format!("/api/tool_scripts/{sid}/versions");

    let (status, first) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({ "script": DOUBLER, "manifest": manifest("Doubler"),
                 "test_cases": cases(4.0), "note": "first cut" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version created: {first}");
    assert_eq!(first["version"]["note"], "first cut");
    assert_eq!(
        first["version"]["created_by"], ADMIN_EMAIL,
        "the author is the caller: {first}"
    );

    let (status, duplicate) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({ "script": DOUBLER, "manifest": manifest("Doubler"),
                 "test_cases": cases(4.0), "note": "first cut" }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "nothing changed: {duplicate}");

    let (status, relabelled) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({ "script": DOUBLER, "manifest": manifest("Doubler (mg/L)"),
                 "test_cases": cases(4.0), "note": "unit in the label" }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 200,
        "a manifest-only edit is a version: {relabelled}"
    );
    assert_eq!(relabelled["version"]["version_no"], 2);
    assert_ne!(
        relabelled["version"]["content_hash"],
        first["version"]["content_hash"]
    );

    let (status, recased) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({ "script": DOUBLER, "manifest": manifest("Doubler (mg/L)"),
                 "test_cases": json!({ "tolerance": 1e-9, "cases": [
                     { "name": "two", "inputs": { "x": 2 }, "expected": { "doubled": 4.0 } },
                     { "name": "three", "inputs": { "x": 3 }, "expected": { "doubled": 6.0 } } ] }),
                 "note": "a case for odd inputs" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "a case-only edit is a version: {recased}");

    let vid = relabelled["version"]["id"].as_str().unwrap();
    let (status, detail) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(detail["note"], "unit in the label", "the note is stored");
    assert!(
        detail["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"),
        "{detail}"
    );

    let (_, listed) =
        crate::common::get_json_with_token(&app, &format!("/api/tool_scripts/{sid}"), &admin).await;
    let notes: Vec<&str> = listed["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["note"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        notes,
        vec!["a case for odd inputs", "unit in the label", "first cut"],
        "every version carries its own note, newest first"
    );
}

#[tokio::test]
#[serial]
async fn activation_credits_the_caller_not_the_request_body() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "activation_credits_the_caller_not_the_request_body",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_activation_actor").await {
        return;
    }
    let (app, admin, sid) = setup("activation_actor").await;

    let (_, created) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": DOUBLER, "manifest": manifest("Doubler"),
                 "test_cases": cases(4.0), "created_by": "someone else" }),
        &admin,
    )
    .await;
    let vid = created["version"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["version"]["created_by"], ADMIN_EMAIL, "{created}");

    for (step, body) in [
        ("validate", json!({})),
        ("activate", json!({ "activated_by": "someone else" })),
    ] {
        let (status, out) = crate::common::post_json_parse_with_token(
            &app,
            &format!("/api/tool_scripts/{sid}/versions/{vid}/{step}"),
            &body,
            &admin,
        )
        .await;
        assert_eq!(status, 200, "{step}: {out}");
    }

    let (status, audit) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/activations"),
        &admin,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        audit[0]["activated_by"], ADMIN_EMAIL,
        "the audit records the token's identity: {audit}"
    );
}

#[tokio::test]
#[serial]
async fn validation_applies_the_manifest_the_calculate_path_applies() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "validation_applies_the_manifest_the_calculate_path_applies",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_validation_manifest").await {
        return;
    }
    let (app, admin, sid) = setup("validation_manifest").await;

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({
            "script": DOUBLER,
            "manifest": manifest("Doubler"),
            "test_cases": json!({ "tolerance": 1e-9, "cases": [
                { "name": "sound", "inputs": { "x": 2 }, "expected": { "doubled": 4.0 } },
                { "name": "unknown_field", "inputs": { "x": 2, "y": 3 },
                  "expected": { "doubled": 4.0 } },
                { "name": "wrong_kind", "inputs": { "x": "two" }, "expected": { "doubled": 4.0 } },
                { "name": "missing_required", "inputs": {}, "expected": { "doubled": 4.0 } }
            ] }),
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version created: {created}");
    let vid = created["version"]["id"].as_str().unwrap();

    let (status, validated) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/validate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "validation ran: {validated}");
    assert_eq!(validated["passed"], false, "{validated}");

    let by_name = |name: &str| {
        validated["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(by_name("sound")["passed"], true, "{validated}");
    for (case, wanted) in [
        ("unknown_field", "unknown field 'y'"),
        ("wrong_kind", "must be number"),
        ("missing_required", "missing required field 'x'"),
    ] {
        let result = by_name(case);
        assert_eq!(result["passed"], false, "{case}: {result}");
        assert!(
            result["error"].as_str().unwrap_or("").contains(wanted),
            "{case} fails the way the calculate path would: {result}"
        );
    }

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/activate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "an unvalidated version stays inactive: {refused}"
    );
}

/// Every line is a construct the lint has to name, except the two that only look like one:
/// line 2 mentions a call inside a string and line 3 puts a `#` inside a string before a real
/// call, which a split-on-`#` scan would swallow.
const LINT_SUBJECT: &str = r#"tool <- function(inputs, constants, curves) {
  msg <- "system( is fine inside a string"
  note <- "a # inside a string"; unlink("scratch")
  writeLines("x", "out.txt")
  saveRDS(inputs, "out.rds")
  save(msg, file = "out.rda")
  dir.create("scratch")
  con <- file("out.txt", "w")
  cat("x", file = "out.txt")
  sink("out.txt")
  file.create("out.txt")
  tmp <- tempfile()
  list(ok = 1, msg = msg, note = note, tmp = tmp, con = con)
}"#;

#[tokio::test]
#[serial]
async fn the_lint_names_file_writes_and_reads_strings_as_strings() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "the_lint_names_file_writes_and_reads_strings_as_strings",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_lint").await {
        return;
    }
    let (app, admin, sid) = setup("lint_subject").await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": LINT_SUBJECT, "manifest": manifest("Lint subject") }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "the lint refuses the script: {refused}");
    let findings = refused["detail"].as_array().unwrap();
    let at = |line: u64| -> Vec<String> {
        findings
            .iter()
            .filter(|f| f["line"] == line)
            .map(|f| f["message"].as_str().unwrap_or("").to_string())
            .collect()
    };

    for (line, token) in [
        (3, "unlink"),
        (4, "writeLines"),
        (5, "saveRDS"),
        (6, "save"),
        (7, "dir.create"),
        (8, "file"),
        (9, "cat"),
        (10, "sink"),
        (11, "file.create"),
    ] {
        let messages = at(line);
        assert!(
            messages.iter().any(|m| m.contains(&format!("'{token}'"))),
            "line {line} names {token}: {messages:?}"
        );
    }
    assert!(
        at(2).is_empty(),
        "a call inside a string is text: {:?}",
        at(2)
    );
    assert!(
        at(12).is_empty(),
        "tempfile() is not a file connection: {:?}",
        at(12)
    );
}

#[tokio::test]
#[serial]
async fn a_script_that_does_not_parse_as_r_is_refused_with_its_line() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_script_that_does_not_parse_as_r_is_refused_with_its_line",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_syntax").await {
        return;
    }
    let (app, admin, sid) = setup("syntax_subject").await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({
            "script": "tool <- function(inputs, constants, curves) {\n  x <- 1 +\n}\n",
            "manifest": manifest("Syntax subject"),
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "the script does not parse: {refused}");
    let findings = refused["detail"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["message"].as_str().unwrap_or("").contains("parse")),
        "the finding says what is wrong: {refused}"
    );
    assert!(
        findings.iter().any(|f| f["line"] == 3),
        "the finding carries the line R named: {refused}"
    );
}

/// The spellings a scan over characters has to anticipate one by one, each on its own line, plus
/// an R raw string literal whose closing quote is not where a quote-tracking scanner looks for it.
/// Line 5 calls the alias assigned on line 4: the name that reaches `system` is bound on 4, so
/// that is the line the finding belongs on.
const BYPASS_SUBJECT: &str = r####"tool <- function(inputs, constants, curves) {
  raw <- r"(a " b)"
  system ("ls")
  runner <- system
  runner("ls")
  do.call("system", list("ls"))
  get("system")()
  `system`("ls")
  loadNamespace("curl")
  cat("x", f = "out.txt")
  list(ok = 1, raw = raw)
}"####;

#[tokio::test]
#[serial]
async fn no_spelling_of_a_forbidden_call_gets_past_the_lint() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "no_spelling_of_a_forbidden_call_gets_past_the_lint",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_bypass").await {
        return;
    }
    let (app, admin, sid) = setup("bypass_subject").await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": BYPASS_SUBJECT, "manifest": manifest("Bypass subject") }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "the lint refuses the script: {refused}");
    let findings = refused["detail"].as_array().unwrap();
    let at = |line: u64| -> Vec<String> {
        findings
            .iter()
            .filter(|f| f["line"] == line)
            .map(|f| f["message"].as_str().unwrap_or("").to_string())
            .collect()
    };

    for (line, named, spelling) in [
        (3, "system", "a space before the parenthesis"),
        (4, "system", "the function assigned to another name"),
        (6, "system", "do.call with the name as a string"),
        (7, "system", "get with the name as a string"),
        (8, "system", "a backtick-quoted name"),
        (9, "curl", "loadNamespace in place of library"),
        (10, "cat", "an abbreviated file= argument"),
    ] {
        let messages = at(line);
        assert!(
            messages.iter().any(|m| m.contains(&format!("'{named}'"))),
            "line {line} ({spelling}) names {named}: {messages:?}"
        );
    }
}

/// The spellings that put a forbidden name somewhere other than the head of a plain call: behind an
/// index or a slot, behind a namespace in value position, and as a string handed to a call that
/// resolves it with `match.fun`.
///
/// Line 8 carries no finding on purpose. `runner` is bound on line 7, so that is where the name
/// reaching `system` is visible, and line 8 only calls a local variable.
const INDIRECTION_SUBJECT: &str = r#"tool <- function(inputs, constants, curves) {
  a <- unlink("scratch")$z
  b <- system("ls")$x
  d <- asNamespace("base")$system("ls")
  e <- baseenv()$system("ls")
  f <- system("ls")@x
  runner <- base::system
  runner("ls")
  g <- lapply(1, "system")
  h <- Reduce("unlink", 1:2)
  i <- Map("system", "ls")
  j <- inputs[["system"]]("ls")
  k <- Vectorize("system")
  list(ok = 1)
}"#;

#[tokio::test]
#[serial]
async fn a_forbidden_name_is_reached_through_an_index_a_slot_or_a_namespace() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_forbidden_name_is_reached_through_an_index_a_slot_or_a_namespace",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_indirection").await {
        return;
    }
    let (app, admin, sid) = setup("indirection_subject").await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": INDIRECTION_SUBJECT, "manifest": manifest("Indirection subject") }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "the lint refuses the script: {refused}");
    let findings = refused["detail"].as_array().unwrap();
    let at = |line: u64| -> Vec<String> {
        findings
            .iter()
            .filter(|f| f["line"] == line)
            .map(|f| f["message"].as_str().unwrap_or("").to_string())
            .collect()
    };

    for (line, named, spelling) in [
        (2, "unlink", "a call indexed with $"),
        (3, "system", "a call indexed with $"),
        (4, "asNamespace", "a namespace fetched as a value"),
        (4, "system", "a field read off a namespace, then called"),
        (5, "baseenv", "an environment fetched as a value"),
        (5, "system", "a field read off an environment, then called"),
        (6, "system", "a call whose result is reached with @"),
        (7, "system", "a namespaced name assigned to another name"),
        (9, "system", "lapply with the function named as a string"),
        (10, "unlink", "Reduce with the function named as a string"),
        (11, "system", "Map with the function named as a string"),
        (12, "system", "an element indexed with [[, then called"),
        (
            13,
            "system",
            "Vectorize with the function named as a string",
        ),
    ] {
        let messages = at(line);
        assert!(
            messages.iter().any(|m| m.contains(&format!("'{named}'"))),
            "line {line} ({spelling}) names {named}: {messages:?}"
        );
    }
    assert!(
        at(8).is_empty(),
        "calling a local variable names nothing: {:?}",
        at(8)
    );
}

#[tokio::test]
#[serial]
async fn a_raw_string_does_not_blind_the_line_after_it() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_raw_string_does_not_blind_the_line_after_it",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_raw_string").await {
        return;
    }
    let (app, admin, sid) = setup("raw_string_subject").await;

    let script = concat!(
        "tool <- function(inputs, constants, curves) {\n",
        "  quoted <- r\"(system( \" )\"\n",
        "  unlink(\"scratch\")\n",
        "  list(ok = 1, quoted = quoted)\n",
        "}"
    );
    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({ "script": script, "manifest": manifest("Raw string subject") }),
        &admin,
    )
    .await;
    assert_eq!(status, 409, "the lint refuses the script: {refused}");
    let findings = refused["detail"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(
        findings[0]["line"], 3,
        "the raw string is a string and the call after it is still read: {findings:?}"
    );
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("'unlink'"),
        "{findings:?}"
    );
}

/// A case that reads a constant, so the answer can change without the version changing.
const CONSTANT_READER: &str =
    "tool <- function(inputs, constants, curves) list(scaled = inputs$x * constants$probe_gate)";

fn constant_manifest() -> serde_json::Value {
    json!({
        "label": "Constant reader",
        "params": [ { "name": "x", "label": "X", "kind": "number", "required": true } ],
        "constants": ["probe_gate"],
        "outputs": [ { "key": "scaled", "label": "Scaled", "per_replicate": false } ],
    })
}

async fn set_probe_gate(db: &DatabaseConnection, value: f64) {
    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!(
            "INSERT INTO constants (name, value) VALUES ('probe_gate', {value}) \
             ON CONFLICT (name) DO UPDATE SET value = {value}"
        ),
    ))
    .await
    .expect("probe constant written");
}

async fn stored_stamp(db: &DatabaseConnection, vid: &str) -> Option<String> {
    db.query_one(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT validated_at::text AS stamp FROM tool_script_versions WHERE id = '{vid}'"),
    ))
    .await
    .expect("version read")
    .and_then(|row| row.try_get::<Option<String>>("", "stamp").unwrap())
}

/// Scenario: a version passes its cases, then the catalog it reads moves under it.
///
/// Expected behaviour: the failing re-validation clears `validated_at`, and activation is refused
/// afterwards. The stamp is what the gate reads, so a stamp that outlives the run that earned it
/// puts a failing version live.
#[tokio::test]
#[serial]
async fn a_failed_revalidation_clears_the_stamp_and_blocks_activation() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "a_failed_revalidation_clears_the_stamp_and_blocks_activation",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_stale_stamp").await {
        return;
    }
    let (app, admin, sid, db) = setup_with_db("stale_stamp_subject").await;
    set_probe_gate(&db, 2.0).await;

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({
            "script": CONSTANT_READER,
            "manifest": constant_manifest(),
            "test_cases": { "tolerance": 1e-9, "cases": [
                { "name": "three", "inputs": { "x": 3 }, "expected": { "scaled": 6.0 } } ] },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version created: {created}");
    let vid = created["version"]["id"].as_str().unwrap().to_string();

    let (status, passed) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/validate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "validation ran: {passed}");
    assert_eq!(passed["passed"], true, "{passed}");
    assert!(stored_stamp(&db, &vid).await.is_some(), "the stamp is set");

    set_probe_gate(&db, 5.0).await;

    let (status, failed) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/validate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "re-validation ran: {failed}");
    assert_eq!(failed["passed"], false, "{failed}");
    assert!(failed["validated_at"].is_null(), "{failed}");
    assert!(
        stored_stamp(&db, &vid).await.is_none(),
        "the stamp the gate reads is cleared, not just the response field"
    );

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/activate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "a version that now fails stays inactive: {refused}"
    );

    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM constants WHERE name = 'probe_gate'".to_string(),
    ))
    .await
    .expect("probe constant removed");
}

/// Scenario: a version was validated, and what it reads changes before anyone activates it.
///
/// Expected behaviour: activation runs the cases itself and refuses. Nothing re-validates
/// between the stamp and the flip, so the stamp alone cannot be what puts a version live.
#[tokio::test]
#[serial]
async fn activation_runs_the_cases_rather_than_trusting_the_stamp() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "activation_runs_the_cases_rather_than_trusting_the_stamp",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_activation_runs_cases").await {
        return;
    }
    let (app, admin, sid, db) = setup_with_db("activation_gate_subject").await;
    set_probe_gate(&db, 2.0).await;

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions"),
        &json!({
            "script": CONSTANT_READER,
            "manifest": constant_manifest(),
            "test_cases": { "tolerance": 1e-9, "cases": [
                { "name": "three", "inputs": { "x": 3 }, "expected": { "scaled": 6.0 } } ] },
        }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version created: {created}");
    let vid = created["version"]["id"].as_str().unwrap().to_string();

    let (status, passed) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/validate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "validation ran: {passed}");
    assert_eq!(passed["passed"], true, "{passed}");

    set_probe_gate(&db, 5.0).await;

    let (status, refused) = crate::common::post_json_parse_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}/activate"),
        &json!({}),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "the stamp is still set and activation refuses anyway: {refused}"
    );
    assert!(
        refused.to_string().contains("scaled"),
        "the refusal carries the failing case: {refused}"
    );
    assert!(
        stored_stamp(&db, &vid).await.is_none(),
        "the run activation made records its own outcome"
    );

    let (status, script) =
        crate::common::get_json_with_token(&app, &format!("/api/tool_scripts/{sid}"), &admin).await;
    assert_eq!(status, 200, "{script}");
    assert!(
        script["active_version_id"].is_null(),
        "nothing went live: {script}"
    );

    db.execute(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        "DELETE FROM constants WHERE name = 'probe_gate'".to_string(),
    ))
    .await
    .expect("probe constant removed");
}

/// Scenario: the lint reads the parse tree, so a rule written against one spelling now reaches
/// every spelling of it.
///
/// Expected behaviour: the thirteen shipped tools still pass. They are the corpus the policy has
/// to stay compatible with, and a rule that refuses one of them is a rule that refuses the
/// portal's own R.
#[tokio::test]
#[serial]
async fn every_shipped_tool_script_passes_the_lint() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "every_shipped_tool_script_passes_the_lint",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_seed_lint").await {
        return;
    }
    let (app, admin, sid, db) = setup_with_db("seed_lint_subject").await;

    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            r"SELECT s.name, v.script, v.entry_function, v.manifest
              FROM tool_scripts s
              JOIN tool_script_versions v ON v.id = s.active_version_id
              ORDER BY s.name"
                .to_string(),
        ))
        .await
        .expect("active versions read");
    assert!(
        rows.len() >= 2,
        "the seeded tool and the created subject are present: {}",
        rows.len()
    );

    for row in &rows {
        let name: String = row.try_get("", "name").unwrap();
        let script: String = row.try_get("", "script").unwrap();
        let entry: String = row.try_get("", "entry_function").unwrap();
        let manifest: serde_json::Value = row.try_get("", "manifest").unwrap();
        let (status, out) = crate::common::post_json_parse_with_token(
            &app,
            &format!("/api/tool_scripts/{sid}/versions"),
            &json!({ "script": script, "entry_function": entry, "manifest": manifest }),
            &admin,
        )
        .await;
        assert_eq!(status, 200, "{name} passes the lint: {out}");
    }
}

/// Scenario: a provenance blob pins a version's content hash, and the duplicate check refuses a
/// version whose content already exists.
///
/// Expected behaviour: the hash on a stored version is recomputable from that version, so fetching
/// one and posting it back unchanged is recognised as the duplicate it is. `jsonb` is a parsed
/// value, not the text it arrived as: a hash taken over the request body would not survive the
/// tolerance `1e-9` reading back as `0.000000001`.
#[tokio::test]
#[serial]
async fn an_authored_hash_recomputes_from_the_stored_version() {
    if !crate::common::tools_runner::require_runner_or_skip(
        "an_authored_hash_recomputes_from_the_stored_version",
    )
    .await
    {
        return;
    }
    if !kc::require_keycloak_or_skip("tool_script_stored_hash").await {
        return;
    }
    let (app, admin, sid) = setup("stored_hash").await;
    let versions = format!("/api/tool_scripts/{sid}/versions");
    let renormalising = json!({ "tolerance": 1e-9, "cases": [
        { "name": "two", "inputs": { "x": 2 }, "expected": { "doubled": 4.0 },
          "scale": 1.50E+2 } ] });

    let (status, created) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({ "script": DOUBLER, "manifest": manifest("Doubler"),
                 "test_cases": renormalising, "note": "stored hash" }),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version created: {created}");
    let vid = created["version"]["id"].as_str().unwrap().to_string();

    let (status, stored) = crate::common::get_json_with_token(
        &app,
        &format!("/api/tool_scripts/{sid}/versions/{vid}"),
        &admin,
    )
    .await;
    assert_eq!(status, 200, "version fetched: {stored}");
    // The renormalisation under test is textual and happens inside Postgres (`1e-9` is stored and
    // re-rendered as `0.000000001`), so it is invisible here: serde parses either spelling to the
    // same number. What can be asserted is that the value survived; the two assertions below are
    // the property itself.
    assert_eq!(
        stored["test_cases"]["tolerance"].as_f64(),
        Some(1e-9),
        "the tolerance survives the round trip: {stored}"
    );
    assert_eq!(
        river_db::routes::private::tools::scripts::version_content_hash(
            stored["script"].as_str().expect("script"),
            stored["entry_function"].as_str().expect("entry_function"),
            &stored["manifest"],
            &stored["test_cases"],
        ),
        stored["content_hash"].as_str().expect("content_hash"),
        "the stored hash does not describe the stored version"
    );

    let (status, reposted) = crate::common::post_json_parse_with_token(
        &app,
        &versions,
        &json!({ "script": stored["script"], "entry_function": stored["entry_function"],
                 "manifest": stored["manifest"], "test_cases": stored["test_cases"] }),
        &admin,
    )
    .await;
    assert_eq!(
        status, 409,
        "a fetched version posted back is the same version: {reposted}"
    );
}
