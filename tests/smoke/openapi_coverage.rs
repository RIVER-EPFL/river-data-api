//! What the served document can be generated from. A response typed as a free JSON object is a
//! shape no client can be generated against and no drift can be caught in, so the set of them is
//! written down here and only ever shrinks.
//!
//! Run: cargo test --test smoke openapi_coverage -- --test-threads=1

use serial_test::serial;

/// Operations whose 2xx body is a free JSON object or has no schema at all. Each is a response
/// still built with `serde_json::json!` rather than a typed struct. Removing one from this list is
/// the work; adding one is a regression this test refuses.
const UNTYPED: &[(&str, &str)] = &[
    ("GET", "/api/notifications/me/push"),
    ("POST", "/api/notifications/me/push"),
    ("DELETE", "/api/notifications/me/push"),
    ("POST", "/api/notifications/me/push/ping"),
    ("POST", "/api/notifications/me/push/test"),
    ("GET", "/healthz"),
];

/// Whether an operation's 2xx body is something a type can be generated from: a `$ref`, an array,
/// or a declared object. A 204 answers with no body at all, and a non-JSON content type says what
/// it serves instead of an object; both are typed by saying so.
fn typed(op: &utoipa::openapi::path::Operation) -> bool {
    let Ok(value) = serde_json::to_value(op) else {
        return false;
    };
    let Some(responses) = value
        .get("responses")
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    let Some((code, body)) = responses
        .iter()
        .filter(|(code, _)| code.starts_with('2'))
        .min_by_key(|(code, _)| (*code).clone())
    else {
        return false;
    };
    if code == "204" {
        return true;
    }
    let Some(content) = body.get("content").and_then(serde_json::Value::as_object) else {
        return false;
    };
    // A response that declares what it is and is not JSON (an event stream, a file) is typed by
    // saying so; there is no object for a client generator to miss.
    if !content.contains_key("application/json") && !content.is_empty() {
        return true;
    }
    let Some(schema) = content
        .get("application/json")
        .and_then(|j| j.get("schema"))
    else {
        return false;
    };
    let text = schema.to_string();
    if text.contains("$ref") || schema.get("items").is_some() {
        return true;
    }
    // A bare `{"type": "object"}` is `serde_json::Value`: everything and therefore nothing.
    !(schema.get("type").and_then(serde_json::Value::as_str) == Some("object")
        && schema.get("properties").is_none())
}

fn operations(
    item: &utoipa::openapi::PathItem,
) -> Vec<(&'static str, &utoipa::openapi::path::Operation)> {
    let mut ops = Vec::new();
    for (method, op) in [
        ("GET", &item.get),
        ("POST", &item.post),
        ("PUT", &item.put),
        ("PATCH", &item.patch),
        ("DELETE", &item.delete),
    ] {
        if let Some(op) = op {
            ops.push((method, op));
        }
    }
    ops
}

#[tokio::test]
#[serial]
async fn the_untyped_responses_are_the_ones_written_down() {
    let db = crate::common::setup_test_db().await;
    let mut config = crate::common::test_config();
    config.keycloak_admin_client_id = Some("river-data-admin".into());
    config.keycloak_admin_client_secret = Some("unused".into());
    let state = river_db::common::AppState::new(db.clone(), config, None);
    let spec = river_db::routes::openapi_spec(&state);

    let mut untyped = Vec::new();
    let mut total = 0usize;
    for (path, item) in &spec.paths.paths {
        for (method, op) in operations(item) {
            total += 1;
            if !typed(op) {
                untyped.push((method, path.clone()));
            }
        }
    }
    let known: std::collections::HashSet<(&str, &str)> = UNTYPED.iter().copied().collect();
    let new: Vec<_> = untyped
        .iter()
        .filter(|(m, p)| !known.contains(&(*m, p.as_str())))
        .collect();
    assert!(
        new.is_empty(),
        "responses typed as a free JSON object, which nothing can be generated from:\n  {new:?}\n\
         Give each a struct, or add it to UNTYPED with the reason."
    );
    let fixed: Vec<_> = UNTYPED
        .iter()
        .filter(|(m, p)| !untyped.iter().any(|(um, up)| um == m && up == p))
        .collect();
    assert!(
        fixed.is_empty(),
        "these are typed now and belong out of the UNTYPED list: {fixed:?}"
    );
    assert!(
        total > 300,
        "the document should describe the whole private API, found {total} operations"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// Expected behaviour: the blobs a client reads field by field are described field by field. The
/// audit hold's three statistics blobs and the plan PATCH body are free JSON on the row and in the
/// handler, and were objects with no properties in the document, so the panel's `computed.n` and
/// the review's `instrument_clear` were names nothing on either side checked.
#[tokio::test]
#[serial]
async fn the_shapes_a_client_reads_are_described() {
    let db = crate::common::setup_test_db().await;
    let state = river_db::common::AppState::new(db.clone(), crate::common::test_config(), None);
    let spec = river_db::routes::openapi_spec(&state);
    let doc = serde_json::to_value(&spec).expect("the document serialises");
    let schemas = &doc["components"]["schemas"];

    for (name, fields) in [
        ("HoldExpected", &["mean", "sd"][..]),
        ("HoldComputed", &["mean", "sd", "n", "values"][..]),
        ("HoldDelta", &["mean", "sd"][..]),
        (
            "PlanEntryUpdate",
            &["stream_id", "instrument_clear", "acknowledged"][..],
        ),
        ("PlanCurveUpdate", &["curve_id", "instrument_source_key"][..]),
    ] {
        let properties = &schemas[name]["properties"];
        for field in fields {
            assert!(
                properties.get(*field).is_some(),
                "{name}.{field} is not in the document: {}",
                schemas[name]
            );
        }
    }

    for (blob, schema) in [
        ("expected", "HoldExpected"),
        ("computed", "HoldComputed"),
        ("delta", "HoldDelta"),
    ] {
        assert_eq!(
            schemas["HoldRow"]["properties"][blob]["$ref"],
            serde_json::json!(format!("#/components/schemas/{schema}")),
            "the hold's {blob} names its own shape"
        );
    }

    let body = &doc["paths"]["/api/sync/pairing-plans/{id}"]["patch"]["requestBody"]["content"]
        ["application/json"]["schema"]["$ref"];
    assert_eq!(
        body,
        &serde_json::json!("#/components/schemas/UpdatePairingPlanRequest"),
        "the plan PATCH names the body it deserializes: {body}"
    );
}
