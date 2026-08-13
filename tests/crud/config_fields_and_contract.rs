//! Config the dashboard writes that the API drops or never reads, plus the two list-contract
//! defects. Each test names the finding it proves in the comment above it.
//!
//! The person-flow tests run as a real Keycloak user, since the defects are about what an operator
//! configures through the dashboard; they self-skip when Keycloak is unreachable unless
//! `REQUIRE_KEYCLOAK` is set. The two list-contract tests use an API token so they always run.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use river_db::common::AppState;
use river_db::common::authz::AccessScope;
use river_db::routes::private::notifications::{
    DeliveryResult, NotificationChannel, OutgoingMessage, commands, triggers,
};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::{
    build_test_app, build_test_app_with_state, cleanup_test_db, full_permissions, get_json,
    get_json_with_token, get_with_token, post_json_parse_with_token, put_json_with_token,
    seed_api_token, setup_test_db,
};

/// The entry of a parameter list (site detail projection or readings series) for one catalog
/// parameter, by id rather than by position.
fn entry_for<'a>(list: &'a serde_json::Value, parameter_id: &str) -> &'a serde_json::Value {
    let entries = list
        .as_array()
        .unwrap_or_else(|| panic!("expected an array of parameters: {list}"));
    entries
        .iter()
        .find(|e| e["parameter_id"] == parameter_id)
        .unwrap_or_else(|| panic!("no entry for parameter {parameter_id}: {list}"))
}

/// The cell under `column` on the first CSV data row.
fn csv_cell(body: &str, column: &str) -> String {
    let mut lines = body.lines();
    let header: Vec<&str> = lines
        .next()
        .unwrap_or_else(|| panic!("csv body has no header: {body}"))
        .split(',')
        .collect();
    let index = header
        .iter()
        .position(|h| *h == column)
        .unwrap_or_else(|| panic!("csv header has no {column} column: {body}"));
    let row: Vec<&str> = lines
        .next()
        .unwrap_or_else(|| panic!("csv body has no data row: {body}"))
        .split(',')
        .collect();
    row.get(index)
        .unwrap_or_else(|| panic!("csv row is short of column {column}: {body}"))
        .to_string()
}

// the Public toggle sent on site_parameter create is dropped, so a slot created public
// comes back private and stays out of the public API until a separate update.
#[tokio::test]
#[serial]
async fn site_parameter_create_honours_is_public() {
    if !kc::require_keycloak_or_skip("site_parameter_create_honours_is_public").await {
        return;
    }
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &jwt, "RD050 Project", "rd050", true).await;
    let site_id = e2e::create_site(&app, &jwt, &project_id, "RD050 Site", "rd050-site").await;
    let public_param = e2e::create_parameter(&app, &jwt, "Rd050Public", "RD050 Public", "mm").await;
    let private_param =
        e2e::create_parameter(&app, &jwt, "Rd050Private", "RD050 Private", "mm").await;

    let (status, created_public) = post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &json!({ "site_id": site_id, "parameter_id": public_param, "is_public": true }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a slot with the Public toggle on ({status}): {created_public}"
    );

    let (status, created_private) = post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &json!({ "site_id": site_id, "parameter_id": private_param }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a slot with the toggle omitted ({status}): {created_private}"
    );

    let (status, after_create) =
        get_json(&app, "/api/public/rd050/sites/rd050-site/parameters").await;
    assert_eq!(
        status, 200,
        "public parameter list ({status}): {after_create}"
    );
    let codes_after_create: HashSet<String> = after_create
        .as_array()
        .unwrap_or_else(|| panic!("public parameters must be an array: {after_create}"))
        .iter()
        .filter_map(|p| p["code"].as_str().map(str::to_string))
        .collect();

    let (status, body) = put_json_with_token(
        &app,
        &format!("/api/site_parameters/{}", e2e::id_of(&created_private)),
        &json!({ "is_public": true }),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "update the toggle on the second slot ({status}): {body}"
    );
    let updated: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("update body is not JSON: {e}: {body}"));

    let (status, after_update) =
        get_json(&app, "/api/public/rd050/sites/rd050-site/parameters").await;
    assert_eq!(
        status, 200,
        "public parameter list after update ({status}): {after_update}"
    );
    let codes_after_update: HashSet<String> = after_update
        .as_array()
        .unwrap_or_else(|| panic!("public parameters must be an array: {after_update}"))
        .iter()
        .filter_map(|p| p["code"].as_str().map(str::to_string))
        .collect();

    // The update path works, so a slot missing from the list above is missing because create
    // dropped the field, not because the public surface is broken.
    assert_eq!(
        updated["is_public"],
        json!(true),
        "update carries is_public: {updated}"
    );
    assert!(
        codes_after_update.contains("Rd050Private"),
        "a slot made public by update is served by the public API: {after_update}"
    );

    assert_eq!(
        created_public["is_public"],
        json!(true),
        "the Public toggle sent on create must survive the create: {created_public}"
    );
    assert!(
        codes_after_create.contains("Rd050Public"),
        "a slot created public is served by the public API without a follow-up update: {after_create}"
    );

    assert_eq!(
        created_private["is_public"],
        json!(false),
        "omitting the toggle still creates a private slot: {created_private}"
    );
    assert!(
        !codes_after_create.contains("Rd050Private"),
        "a slot created without the toggle stays out of the public API: {after_create}"
    );
}

// /sites/{id}/readings serves the site-level units with no catalog fallback, so an
// adopt-created slot reports null units while /sites/{id}/parameters reports the catalog default.
#[tokio::test]
#[serial]
async fn adopted_slot_reports_the_same_units_on_both_site_endpoints() {
    if !kc::require_keycloak_or_skip("adopted_slot_reports_the_same_units").await {
        return;
    }
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &jwt, "RD051 Project", "rd051", false).await;
    let site_id = e2e::create_site(&app, &jwt, &project_id, "RD051 Site", "rd051-site").await;
    let adopted_param =
        e2e::create_parameter(&app, &jwt, "Rd051Do", "RD051 Dissolved Oxygen", "uM").await;
    let override_param =
        e2e::create_parameter(&app, &jwt, "Rd051Temp", "RD051 Temperature", "degC").await;

    let sensor_id = e2e::create_sensor(&app, &jwt, &adopted_param, "RD051-0001").await;
    let (status, adopted) = post_json_parse_with_token(
        &app,
        &format!("/api/sensors/{sensor_id}/adopt"),
        &json!({
            "site_id": site_id,
            "parameter_id": adopted_param,
            "create_site_parameter": true,
        }),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "adopt the sensor into a new slot ({status}): {adopted}"
    );
    assert_eq!(
        adopted["site_parameter_created"],
        json!(true),
        "adopt created the slot under test: {adopted}"
    );

    let (status, overridden) = post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &json!({ "site_id": site_id, "parameter_id": override_param, "display_units": "K" }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a slot carrying a site-level units override ({status}): {overridden}"
    );

    let (status, ingested) = post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "readings": [
            { "site_id": site_id, "parameter_id": adopted_param, "time": "2025-06-10T00:00:00Z", "raw_value": 210.0 },
            { "site_id": site_id, "parameter_id": override_param, "time": "2025-06-10T00:00:00Z", "raw_value": 11.0 },
        ]}),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "seed one reading per slot ({status}): {ingested}"
    );

    let (status, catalog_view) =
        get_json_with_token(&app, &format!("/api/sites/{site_id}/parameters"), &jwt).await;
    assert_eq!(
        status, 200,
        "site parameter list ({status}): {catalog_view}"
    );

    let (status, series_view) = get_json_with_token(
        &app,
        &format!(
            "/api/sites/{site_id}/readings?start=2025-06-01T00:00:00Z&end=2025-06-30T00:00:00Z"
        ),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "site readings ({status}): {series_view}");
    let series = &series_view["parameters"];

    // A site-level override is reported by both endpoints, so the two agree whenever the slot
    // carries its own units.
    assert_eq!(
        entry_for(&catalog_view, &override_param)["units"],
        json!("K"),
        "the parameter list reports the site-level override: {catalog_view}"
    );
    assert_eq!(
        entry_for(series, &override_param)["units"],
        json!("K"),
        "the readings series reports the site-level override: {series_view}"
    );

    assert_eq!(
        entry_for(&catalog_view, &adopted_param)["units"],
        json!("uM"),
        "the parameter list falls back to the catalog units: {catalog_view}"
    );
    assert_eq!(
        entry_for(series, &adopted_param)["units"],
        json!("uM"),
        "the readings series reports the same units for the same slot: {series_view}"
    );
}

// units_name, units_min, units_max, variable_mappings and decimal_places are accepted and
// stored, and nothing consumes them: a slot set to one decimal place still renders full precision
// and the setting never reaches the renderer.
#[tokio::test]
#[serial]
async fn site_parameter_display_config_reaches_a_reader() {
    if !kc::require_keycloak_or_skip("site_parameter_display_config_reaches_a_reader").await {
        return;
    }
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &jwt, "RD052 Project", "rd052", false).await;
    let site_id = e2e::create_site(&app, &jwt, &project_id, "RD052 Site", "rd052-site").await;
    let configured_param =
        e2e::create_parameter(&app, &jwt, "Rd052Depth", "RD052 Depth", "mm").await;
    let plain_param =
        e2e::create_parameter(&app, &jwt, "Rd052Turb", "RD052 Turbidity", "NTU").await;

    let mappings = json!({ "depth": "Rd052Depth" });
    let (status, configured) = post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &json!({
            "site_id": site_id,
            "parameter_id": configured_param,
            "display_units": "mm",
            "decimal_places": 1,
            "units_name": "millimetres",
            "units_min": 0.5,
            "units_max": 99.5,
            "variable_mappings": mappings,
        }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a slot carrying display config ({status}): {configured}"
    );
    assert_eq!(
        configured["decimal_places"],
        json!(1),
        "decimal_places stored: {configured}"
    );
    assert_eq!(
        configured["units_name"],
        json!("millimetres"),
        "units_name stored: {configured}"
    );
    assert_eq!(
        configured["units_min"],
        json!(0.5),
        "units_min stored: {configured}"
    );
    assert_eq!(
        configured["units_max"],
        json!(99.5),
        "units_max stored: {configured}"
    );
    assert_eq!(
        configured["variable_mappings"], mappings,
        "variable_mappings stored: {configured}"
    );

    let (status, reloaded) = get_json_with_token(
        &app,
        &format!("/api/site_parameters/{}", e2e::id_of(&configured)),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "reload the slot ({status}): {reloaded}");
    assert_eq!(
        reloaded["decimal_places"],
        json!(1),
        "decimal_places round-trips: {reloaded}"
    );
    assert_eq!(
        reloaded["units_name"],
        json!("millimetres"),
        "units_name round-trips: {reloaded}"
    );
    assert_eq!(
        reloaded["units_min"],
        json!(0.5),
        "units_min round-trips: {reloaded}"
    );
    assert_eq!(
        reloaded["units_max"],
        json!(99.5),
        "units_max round-trips: {reloaded}"
    );
    assert_eq!(
        reloaded["variable_mappings"], mappings,
        "variable_mappings round-trips: {reloaded}"
    );

    let (status, plain) = post_json_parse_with_token(
        &app,
        "/api/site_parameters",
        &json!({ "site_id": site_id, "parameter_id": plain_param }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a slot with no display config ({status}): {plain}"
    );

    let (status, ingested) = post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "readings": [
            { "site_id": site_id, "parameter_id": configured_param, "time": "2025-06-10T00:00:00Z", "raw_value": 12.3456 },
            { "site_id": site_id, "parameter_id": plain_param, "time": "2025-06-10T00:00:00Z", "raw_value": 45.6789 },
        ]}),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "seed one reading per slot ({status}): {ingested}"
    );

    let range = "start=2025-06-01T00:00:00Z&end=2025-06-30T00:00:00Z";
    let (status, csv) = get_with_token(
        &app,
        &format!("/api/sites/{site_id}/readings?{range}&format=csv"),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "csv export ({status}): {csv}");

    let (status, catalog_view) =
        get_json_with_token(&app, &format!("/api/sites/{site_id}/parameters"), &jwt).await;
    assert_eq!(
        status, 200,
        "site parameter list ({status}): {catalog_view}"
    );
    let configured_entry = entry_for(&catalog_view, &configured_param);

    // A slot with no decimal_places keeps full precision, so a blanket rounding is not the fix.
    assert_eq!(
        csv_cell(&csv, "Rd052Turb"),
        "45.6789",
        "an unconfigured slot renders the stored value unchanged: {csv}"
    );

    let rounded = csv_cell(&csv, "Rd052Depth") == "12.3";
    let carried_to_the_renderer = configured_entry.get("decimal_places") == Some(&json!(1));
    assert!(
        rounded || carried_to_the_renderer,
        "decimal_places must reach display, either by rounding the served value or by travelling \
         with the slot to the client. csv: {csv}, site parameter entry: {configured_entry}"
    );
}

// status-event pages are ordered by time with no tiebreaker, so rows sharing a timestamp
// can repeat or vanish across pages instead of each appearing exactly once.
#[tokio::test]
#[serial]
async fn status_event_pages_cover_every_tied_row_exactly_once() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());

    let project_id = e2e::create_project(&app, &token, "RD055 Project", "rd055", false).await;
    let site_id = e2e::create_site(&app, &token, &project_id, "RD055 Site", "rd055-site").await;

    let mut parameter_ids = Vec::new();
    for i in 0..4 {
        let pid = e2e::create_parameter(
            &app,
            &token,
            &format!("Rd055P{i}"),
            &format!("RD055 Parameter {i}"),
            "state",
        )
        .await;
        e2e::assign_site_parameter_minimal(&app, &token, &site_id, &pid).await;
        parameter_ids.push(pid);
    }

    // Four streams emitting at the same eight timestamps, so every timestamp carries a four-row tie.
    let mut events = Vec::new();
    for hour in 0..8 {
        for (index, pid) in parameter_ids.iter().enumerate() {
            events.push(json!({
                "site_id": site_id,
                "parameter_id": pid,
                "time": format!("2025-05-01T{hour:02}:00:00Z"),
                "value": format!("h{hour}-p{index}"),
            }));
        }
    }
    let total_events = events.len();
    let (status, ingested) = post_json_parse_with_token(
        &app,
        "/api/status_events/batch",
        &json!({ "events": events }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "seed status events ({status}): {ingested}");
    assert_eq!(
        ingested["inserted"],
        json!(total_events),
        "every seeded event landed: {ingested}"
    );

    let range = "start=2025-05-01T00:00:00Z&end=2025-05-02T00:00:00Z";
    let (status, unpaged) = get_json_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{range}"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "unpaged status events ({status}): {unpaged}");
    let unpaged_values: Vec<String> = unpaged["events"]
        .as_array()
        .unwrap_or_else(|| panic!("events must be an array: {unpaged}"))
        .iter()
        .filter_map(|e| e["value"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        unpaged_values.len(),
        total_events,
        "the unpaged read returns every event: {unpaged}"
    );

    let mut paged_values = Vec::new();
    for offset in 0..total_events {
        let (status, page) = get_json_with_token(
            &app,
            &format!("/api/sites/{site_id}/status_events?{range}&limit=1&offset={offset}"),
            &token,
        )
        .await;
        assert_eq!(status, 200, "page at offset {offset} ({status}): {page}");
        assert_eq!(
            page["total"],
            json!(total_events),
            "total counts the full match set on every page: {page}"
        );
        let values = page["events"]
            .as_array()
            .unwrap_or_else(|| panic!("events must be an array: {page}"));
        assert_eq!(values.len(), 1, "a limit of 1 returns one row: {page}");
        paged_values.push(
            values[0]["value"]
                .as_str()
                .unwrap_or_else(|| panic!("event has no value: {page}"))
                .to_string(),
        );
    }

    let distinct: HashSet<&String> = paged_values.iter().collect();
    assert_eq!(
        distinct.len(),
        total_events,
        "every tied row appears exactly once across the pages, none duplicated and none skipped: \
         paged {paged_values:?}"
    );
    assert_eq!(
        paged_values, unpaged_values,
        "paging walks the same total order as the unpaged read: paged {paged_values:?}, \
         unpaged {unpaged_values:?}"
    );

    let (status, csv) = get_with_token(
        &app,
        &format!("/api/sites/{site_id}/status_events?{range}&format=csv"),
        &token,
    )
    .await;
    assert_eq!(status, 200, "csv export ({status}): {csv}");
    assert_eq!(
        csv.lines().count(),
        total_events + 1,
        "the csv export stays a full-range export: {csv}"
    );
}

/// Run a request on its own task so a panic inside the handler is reported as an error rather than
/// unwinding the test.
async fn get_without_unwinding(
    app: &axum::Router,
    uri: &str,
    token: &str,
) -> Result<(u16, String), String> {
    let app = app.clone();
    let uri = uri.to_string();
    let token = token.to_string();
    tokio::spawn(async move { get_with_token(&app, &uri, &token).await })
        .await
        .map_err(|e| e.to_string())
}

// per_page=0 is clamped on the upper bound only, so it reaches SeaORM's paginate() and
// panics instead of answering the caller.
#[tokio::test]
#[serial]
async fn sync_list_endpoints_answer_a_zero_per_page() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let token = seed_api_token(&db, full_permissions(), None).await;
    let app = build_test_app(db.clone());

    for path in ["/api/sync/commands", "/api/sync/events"] {
        let (status, body) = get_without_unwinding(&app, &format!("{path}?per_page=25"), &token)
            .await
            .unwrap_or_else(|e| panic!("{path} with a normal page size must answer: {e}"));
        assert_eq!(status, 200, "{path} with per_page=25 ({status}): {body}");

        let (status, body) = get_without_unwinding(&app, &format!("{path}?per_page=1000"), &token)
            .await
            .unwrap_or_else(|e| panic!("{path} above the upper clamp must answer: {e}"));
        assert_eq!(
            status, 200,
            "{path} clamps an oversized page size rather than refusing it ({status}): {body}"
        );

        let outcome = get_without_unwinding(&app, &format!("{path}?per_page=0"), &token).await;
        assert!(
            outcome.is_ok(),
            "{path} with per_page=0 must answer the caller, not panic the handler: {outcome:?}"
        );
        let (status, body) = outcome.unwrap();
        assert!(
            status == 400 || status == 200,
            "{path} with per_page=0 rejects the value or clamps it to a page ({status}): {body}"
        );
    }
}

struct RecordingChannel {
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
}

#[async_trait::async_trait]
impl NotificationChannel for RecordingChannel {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn check_health(&self) -> Result<String, String> {
        Ok("recording healthy".to_string())
    }

    async fn deliver(&self, _state: &AppState, msg: &OutgoingMessage) -> Vec<DeliveryResult> {
        self.sent.lock().unwrap().push(msg.clone());
        vec![DeliveryResult {
            recipient: "recording".to_string(),
            outcome: Ok(()),
        }]
    }
}

// a notification mute is consulted only by the alarm dispatcher, so a muted slot still
// receives stale-data notifications.
#[tokio::test]
#[serial]
async fn muted_slot_receives_no_stale_data_notification() {
    if !kc::require_keycloak_or_skip("muted_slot_receives_no_stale_data_notification").await {
        return;
    }
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;
    let (_worker_app, state) = build_test_app_with_state(db.clone());

    let project_id = e2e::create_project(&app, &jwt, "RD048 Project", "rd048", false).await;
    let site_id = e2e::create_site(&app, &jwt, &project_id, "RD048 Site", "rd048-site").await;
    let muted_param = e2e::create_parameter(&app, &jwt, "Rd048Muted", "RD048 Muted", "mm").await;
    let audible_param =
        e2e::create_parameter(&app, &jwt, "Rd048Audible", "RD048 Audible", "mm").await;
    e2e::assign_site_parameter_minimal(&app, &jwt, &site_id, &muted_param).await;
    e2e::assign_site_parameter_minimal(&app, &jwt, &site_id, &audible_param).await;

    // Older than the 6h stale threshold the test config carries.
    let stale_at = (Utc::now() - Duration::hours(10)).to_rfc3339();
    let (status, ingested) = post_json_parse_with_token(
        &app,
        "/api/readings/batch",
        &json!({ "readings": [
            { "site_id": site_id, "parameter_id": muted_param, "time": stale_at, "raw_value": 1.0 },
            { "site_id": site_id, "parameter_id": audible_param, "time": stale_at, "raw_value": 2.0 },
        ]}),
        &jwt,
    )
    .await;
    assert_eq!(
        status, 200,
        "seed one stale reading per slot ({status}): {ingested}"
    );

    let (status, mute) = post_json_parse_with_token(
        &app,
        "/api/notification_mutes",
        &json!({ "site_id": site_id, "parameter_id": muted_param }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "mute the noisy slot ({status}): {mute}"
    );

    let sent = Arc::new(Mutex::new(Vec::new()));
    let channels: Vec<Box<dyn NotificationChannel>> =
        vec![Box::new(RecordingChannel { sent: sent.clone() })];
    triggers::run(&state, &channels).await;

    let muted_id = Uuid::parse_str(&muted_param).expect("muted parameter id");
    let audible_id = Uuid::parse_str(&audible_param).expect("audible parameter id");
    let messages = sent.lock().unwrap().clone();
    let stale: Vec<&OutgoingMessage> = messages.iter().filter(|m| m.kind == "stale_data").collect();
    let for_slot = |parameter_id: Uuid| -> usize {
        stale
            .iter()
            .filter(|m| {
                m.slot
                    .as_ref()
                    .is_some_and(|s| s.parameter_id == parameter_id)
            })
            .count()
    };

    // The unmuted slot is stale on the same data, so a missing muted message is suppression and
    // not a detection failure.
    assert_eq!(
        for_slot(audible_id),
        1,
        "the unmuted slot raises exactly one stale-data notification: {stale:?}"
    );
    assert_eq!(
        for_slot(muted_id),
        0,
        "a muted slot raises no stale-data notification: {stale:?}"
    );
    assert_eq!(
        e2e::count(
            &db,
            "SELECT count(*) AS c FROM notification_log WHERE kind = 'stale_data'"
        )
        .await,
        1,
        "only the unmuted slot's delivery is logged"
    );
}

// the Telegram /thresholds command queries alarm_thresholds directly, so a site whose
// thresholds resolve from the parameter-default tier reports none configured.
#[tokio::test]
#[serial]
async fn telegram_thresholds_reports_the_parameter_default_tier() {
    if !kc::require_keycloak_or_skip("telegram_thresholds_reports_the_parameter_default_tier").await
    {
        return;
    }
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let jwt = kc::get_keycloak_jwt("admin", "admin").await;

    let project_id = e2e::create_project(&app, &jwt, "RD049 Project", "rd049", false).await;
    let site_id = e2e::create_site(&app, &jwt, &project_id, "RD049 Site", "rd049-site").await;

    let (status, default_param) = post_json_parse_with_token(
        &app,
        "/api/parameters",
        &json!({
            "code": "Rd049Default",
            "name": "RD049 Default Tier",
            "default_units": "mm",
            "category": "measurement",
            "aliases": [],
            "default_warning_min": 5.0,
            "default_warning_max": 20.0,
            "default_alarm_min": 1.0,
            "default_alarm_max": 30.0,
        }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a parameter carrying default bounds ({status}): {default_param}"
    );
    let default_param_id = e2e::id_of(&default_param);
    e2e::assign_site_parameter_minimal(&app, &jwt, &site_id, &default_param_id).await;

    let site_param = e2e::create_parameter(&app, &jwt, "Rd049Site", "RD049 Site Tier", "mm").await;
    e2e::assign_site_parameter_minimal(&app, &jwt, &site_id, &site_param).await;
    let (status, threshold) = post_json_parse_with_token(
        &app,
        "/api/alarm_thresholds",
        &json!({
            "site_id": site_id,
            "parameter_id": site_param,
            "warning_min": 2.0,
            "warning_max": 8.0,
            "alarm_min": 0.5,
            "alarm_max": 9.5,
        }),
        &jwt,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "create a site-level threshold ({status}): {threshold}"
    );

    let (status, resolved) = get_json_with_token(
        &app,
        &format!("/api/alarms/thresholds?site_id={site_id}"),
        &jwt,
    )
    .await;
    assert_eq!(status, 200, "resolved thresholds ({status}): {resolved}");
    let default_row = entry_for(&resolved, &default_param_id);
    assert_eq!(
        default_row["source"],
        json!("default"),
        "the slot resolves from the parameter-default tier: {resolved}"
    );

    let reply = commands::thresholds(&db, &AccessScope::Unrestricted, &site_id).await;

    // The site-tier slot proves the command renders thresholds at all.
    assert!(
        reply.contains("RD049 Site Tier"),
        "the command reports the site-tier slot: {reply}"
    );
    assert!(
        !reply.contains("(none configured)"),
        "a site with effective thresholds is not reported as unconfigured: {reply}"
    );
    assert!(
        reply.contains("RD049 Default Tier"),
        "the command reports the slot whose thresholds come from the parameter defaults: {reply}"
    );
    for key in ["warning_min", "warning_max", "alarm_min", "alarm_max"] {
        let bound = default_row[key]
            .as_f64()
            .unwrap_or_else(|| panic!("resolved {key} must be a number: {resolved}"));
        assert!(
            reply.contains(&format!("{bound:.1}")),
            "the command reports the same {key} as GET /api/alarms/thresholds ({bound:.1}): {reply}"
        );
    }
}
