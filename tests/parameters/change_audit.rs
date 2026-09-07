//! The change trail: what was done to a parameter, a slot or a group, by whom, and when.
//!
//! `change_audit` is one table keyed by subject, so the route that reads it is keyed by subject
//! too and the per-entity routes are its callers. What only real SQL can show is the triggers
//! firing on every writer and the no-op update writing nothing.

use serde_json::json;
use serial_test::serial;

async fn setup() -> (sea_orm::DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

async fn trail(app: &axum::Router, token: &str, subject: &str) -> Vec<serde_json::Value> {
    let (status, body) = crate::common::get_json_with_token(
        app,
        &format!("/api/change_audit?subject={subject}"),
        token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body.as_array().cloned().unwrap_or_default()
}

#[tokio::test]
#[serial]
async fn a_slot_edit_is_recorded_and_readable_by_subject() {
    let (_db, app, token) = setup().await;
    let slot = crate::common::PARAM_S1_TEMP_ID;

    let before = trail(&app, &token, &format!("site_parameter:{slot}")).await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{slot}"),
        &json!({ "decimal_places": 4 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let after = trail(&app, &token, &format!("site_parameter:{slot}")).await;
    assert_eq!(
        after.len(),
        before.len() + 1,
        "the edit is one entry: {after:?}"
    );
    let newest = &after[0];
    assert_eq!(newest["change"], "site_parameter_update");
    assert_eq!(newest["new_value"]["decimal_places"], 4);
    assert_ne!(
        newest["old_value"]["decimal_places"], newest["new_value"]["decimal_places"],
        "the entry says what it was and what it became: {newest}"
    );
}

#[tokio::test]
#[serial]
async fn an_update_that_changes_nothing_is_not_a_change() {
    let (db, app, token) = setup().await;
    let slot = crate::common::PARAM_S1_TEMP_ID;
    let before = trail(&app, &token, &format!("site_parameter:{slot}")).await;

    crate::common::exec(
        &db,
        &format!("UPDATE site_parameters SET decimal_places = decimal_places WHERE id = '{slot}'"),
    )
    .await;

    let after = trail(&app, &token, &format!("site_parameter:{slot}")).await;
    assert_eq!(
        after.len(),
        before.len(),
        "a rewrite to the same values is nobody's edit: {after:?}"
    );
}

/// The group trail predates the general route and was written by a trigger nothing could read.
#[tokio::test]
#[serial]
async fn a_parameter_group_trail_is_readable_through_the_same_route() {
    let (_db, app, token) = setup().await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/parameter_groups",
        &json!({ "code": "audit_grp", "label": "Audit group", "ordinal": 1 }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let group: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = group["id"].as_str().unwrap();

    let entries = trail(&app, &token, &format!("parameter_group:{id}")).await;
    assert_eq!(entries.len(), 1, "the create is one entry: {entries:?}");
    assert_eq!(entries[0]["change"], "group_insert");
}

/// A subject nothing has been done to is an empty list, not a 404.
#[tokio::test]
#[serial]
async fn an_unknown_subject_is_an_empty_trail() {
    let (_db, app, token) = setup().await;
    assert!(
        trail(&app, &token, "parameter:00000000-0000-4000-a000-00000000dead")
            .await
            .is_empty()
    );
}
