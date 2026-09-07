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
    ("POST", "/api/actions/compute_derived"),
    ("POST", "/api/actions/derived_parameters/{id}/recompute"),
    ("POST", "/api/actions/invalidate_public_config/{code}"),
    ("POST", "/api/actions/rebuild_alarm_events"),
    ("POST", "/api/actions/reconcile_alarms"),
    ("POST", "/api/actions/refresh_aggregates"),
    ("POST", "/api/actions/reprocess"),
    ("POST", "/api/actions/sensor_calibrations/{id}/recalculate"),
    ("GET", "/api/alarms/thresholds"),
    ("PATCH", "/api/sync/commands/{id}"),
    ("PATCH", "/api/sync/events/{id}"),
    ("POST", "/api/sync/events"),
    ("POST", "/api/sync/credentials/{id}/revoke"),
    ("POST", "/api/sync/services/{id}/revoke"),
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
