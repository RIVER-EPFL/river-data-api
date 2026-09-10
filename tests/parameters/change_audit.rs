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
        trail(
            &app,
            &token,
            "parameter:00000000-0000-4000-a000-00000000dead"
        )
        .await
        .is_empty()
    );
}

/// Scenario: a slot is declared by a route that owns its own transaction, not by CRUD.
///
/// Expected behaviour: the entry names who did it. The trigger reads `river.actor`, which only the
/// request knows, so the write declares it on the transaction it runs in.
#[tokio::test]
#[serial]
async fn a_route_that_owns_its_transaction_names_the_writer() {
    use sea_orm::ConnectionTrait;
    let (db, app, token) = setup().await;
    let group_id = uuid::Uuid::new_v4();
    let parameter_id = uuid::Uuid::new_v4();
    for statement in [
        format!(
            "INSERT INTO parameters (id, code, name, category, created_at) \
             VALUES ('{parameter_id}', 'ActorProbe', 'ActorProbe', 'measurement', NOW())"
        ),
        format!(
            "INSERT INTO parameter_groups (id, code, label, ordinal, created_at) \
             VALUES ('{group_id}', 'actor_grp', 'Actor group', 0, NOW())"
        ),
        format!(
            "INSERT INTO parameter_group_members \
                 (id, group_id, parameter_id, ordinal, role, created_at) \
             VALUES (gen_random_uuid(), '{group_id}', '{parameter_id}', 0, 'output', NOW())"
        ),
    ] {
        crate::common::exec(&db, &statement).await;
    }

    let (status, body) = crate::common::post_json_with_token(
        &app,
        &format!("/api/sites/{}/parameter_groups", crate::common::SITE1_ID),
        &json!({ "group_id": group_id }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let row = db
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT changed_by FROM change_audit \
              WHERE change = 'site_parameter_insert' \
              ORDER BY changed_at DESC LIMIT 1"
                .to_string(),
        ))
        .await
        .expect("the trail is readable")
        .expect("the applied group left an entry");
    let changed_by: Option<String> = row.try_get("", "changed_by").expect("changed_by");
    assert!(
        changed_by
            .as_deref()
            .is_some_and(|a| a.starts_with("token:")),
        "the entry names the caller, not nobody: {changed_by:?}"
    );
}

/// Scenario: a slot is created through CRUD, on the transaction the orchestrator opens and
/// `after_begin` declares the writer on. Without that declaration the row records what changed and
/// not who.
///
/// Expected behaviour: the create's entry names the caller.
#[tokio::test]
#[serial]
async fn a_slot_created_through_crud_names_its_caller() {
    let (_db, app, token) = setup().await;
    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
            "name": "Depth",
            "sensor_type": "sensor",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = created["id"].as_str().unwrap();

    let entries = trail(&app, &token, &format!("site_parameter:{id}")).await;
    assert_eq!(entries.len(), 1, "the create is one entry: {entries:?}");
    assert_eq!(entries[0]["change"], "site_parameter_insert");
    let changed_by = entries[0]["changed_by"].as_str();
    assert!(
        changed_by.is_some_and(|a| a.starts_with("token:")),
        "the entry names the caller, not nobody: {changed_by:?}"
    );
}

/// Scenario: a slot and a parameter are updated and deleted through the plain CRUD routes.
///
/// Expected behaviour: every entry names the caller. The orchestrator's transaction carries the
/// declaration, so an entity whose operations struct opts out of it (by overriding `update` or
/// `create` itself, as `constants` and `tool_scripts` once did) fails here rather than being read
/// out of the code (T84).
#[tokio::test]
#[serial]
async fn a_crud_update_and_delete_name_their_caller() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": crate::common::SITE2_ID,
            "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
            "name": "Depth",
            "sensor_type": "sensor",
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{id}"),
        &json!({ "decimal_places": 3 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let after_update = trail(&app, &token, &format!("site_parameter:{id}")).await;
    assert_eq!(after_update[0]["change"], "site_parameter_update");
    for entry in &after_update {
        assert!(
            entry["changed_by"]
                .as_str()
                .is_some_and(|a| a.starts_with("token:")),
            "every entry names the caller, not nobody: {entry}"
        );
    }

    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/site_parameters/{id}"), &token).await;
    assert!(
        (200..300).contains(&status),
        "the slot is deletable: {status} {body}"
    );

    let after_delete = trail(&app, &token, &format!("site_parameter:{id}")).await;
    assert_eq!(
        after_delete[0]["change"], "site_parameter_delete",
        "the delete is the newest entry: {after_delete:?}"
    );
    assert!(
        after_delete[0]["changed_by"]
            .as_str()
            .is_some_and(|a| a.starts_with("token:")),
        "the delete names the caller, not nobody: {}",
        after_delete[0]
    );
}

/// Scenario: a catalog parameter is created, renamed and deleted through CRUD.
///
/// Expected behaviour: each write leaves one entry under `parameter:{id}` carrying both snapshots,
/// which is the trail Q87 chose over an origin column: what the units and the code were before the
/// edit is what makes a wrong series explicable months later (M123).
#[tokio::test]
#[serial]
async fn a_catalog_parameter_leaves_a_trail_with_both_snapshots() {
    let (_db, app, token) = setup().await;

    let (status, body) = crate::common::post_json_with_token(
        &app,
        "/api/parameters",
        &json!({
            "code": "TrailProbe",
            "name": "Trail probe",
            "category": "measurement",
            "default_units": "ppb",
            "aliases": [],
        }),
        &token,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let entries = trail(&app, &token, &format!("parameter:{id}")).await;
    assert_eq!(entries.len(), 1, "the create is one entry: {entries:?}");
    assert_eq!(entries[0]["change"], "parameter_insert");
    assert!(
        entries[0]["old_value"].is_null(),
        "an insert has no before: {}",
        entries[0]
    );
    assert_eq!(entries[0]["new_value"]["default_units"], "ppb");

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/parameters/{id}"),
        &json!({ "default_units": "mg/L" }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let entries = trail(&app, &token, &format!("parameter:{id}")).await;
    assert_eq!(
        entries.len(),
        2,
        "the edit is the second entry: {entries:?}"
    );
    assert_eq!(entries[0]["change"], "parameter_update");
    assert_eq!(
        entries[0]["old_value"]["default_units"], "ppb",
        "the entry says what the units were: {}",
        entries[0]
    );
    assert_eq!(entries[0]["new_value"]["default_units"], "mg/L");

    let (status, body) =
        crate::common::delete_with_token(&app, &format!("/api/parameters/{id}"), &token).await;
    assert!(
        (200..300).contains(&status),
        "the parameter is deletable: {status} {body}"
    );

    let entries = trail(&app, &token, &format!("parameter:{id}")).await;
    assert_eq!(entries[0]["change"], "parameter_delete");
    assert_eq!(
        entries[0]["old_value"]["default_units"], "mg/L",
        "the delete keeps the row it removed: {}",
        entries[0]
    );
    assert!(
        entries[0]["new_value"].is_null(),
        "a delete has no after: {}",
        entries[0]
    );
}

/// Scenario: the trail is read across every subject rather than one at a time.
///
/// Expected behaviour: the entity's list route answers with the same rows the subject-keyed route
/// serves, carrying the subject and the writer, so the two readers cannot disagree about a column.
#[tokio::test]
#[serial]
async fn the_entity_lists_the_same_entry_the_subject_route_serves() {
    let (_db, app, token) = setup().await;
    let slot = crate::common::PARAM_S1_TEMP_ID;
    let subject = format!("site_parameter:{slot}");

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/site_parameters/{slot}"),
        &json!({ "decimal_places": 3 }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let newest = trail(&app, &token, &subject).await.remove(0);

    let (status, listed) = crate::common::get_json_with_token(
        &app,
        &format!(
            "/api/change_audit_entries?filter=%7B%22subject%22%3A%22{subject}%22%7D\
             &sort=%5B%22changed_at%22%2C%22DESC%22%5D"
        ),
        &token,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    let rows = listed.as_array().cloned().unwrap_or_default();
    assert!(!rows.is_empty(), "the entity lists the trail: {listed}");
    assert_eq!(rows[0]["subject"], subject);
    assert_eq!(rows[0]["change"], newest["change"]);
    assert_eq!(rows[0]["changed_by"], newest["changed_by"]);
    assert_eq!(rows[0]["new_value"], newest["new_value"]);
}
