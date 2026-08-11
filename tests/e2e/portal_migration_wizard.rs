//! CNET/METALP portal migration path over the HTTP surface: streams registered exactly as the
//! rshiny sync backend emits them (source_key "{station}:{column}", source_name
//! "{station} - {display}", metadata with station/parameter/units/hierarchy/coordinates), then
//! the discovery/pairing wizard: grouped discovery, pairing plan create, entry edits (rename,
//! converge two entries onto one will-be-created parameter, reclassify onto an existing catalog
//! parameter), apply as a tracked job, and revert.
//!
//! Run: cargo test --test e2e portal_migration -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

const SOURCE_SYSTEM: &str = "cnet";
const STATION_UP: &str = "BER_UP";
const STATION_DN: &str = "BER_DN";
const DO_FIELD_DISPLAY: &str = "Dissolved Oxygen - Field [mg/L]";

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    let row = db
        .query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_string()))
        .await
        .expect("query")
        .expect("row");
    row.try_get::<i64>("", "c").expect("c")
}

async fn scalar_opt_string(db: &DatabaseConnection, sql: &str) -> Option<String> {
    db.query_one(Statement::from_string(DatabaseBackend::Postgres, sql.to_owned()))
        .await
        .unwrap()
        .unwrap()
        .try_get::<Option<String>>("", "v")
        .unwrap()
}

/// Register a stream with the exact shape `RshinyBackend::discover_stream_descriptors` emits.
async fn register_portal_stream(
    app: &axum::Router,
    token: &str,
    station: &str,
    column: &str,
    display: &str,
    units: &str,
    elevation: f64,
) -> String {
    let body = serde_json::json!({
        "source_system": SOURCE_SYSTEM,
        "source_key": format!("{station}:{column}"),
        "source_name": format!("{station} - {display}"),
        "source_path": format!("{SOURCE_SYSTEM}/{station}/{column}"),
        "metadata": {
            "station": {
                "name": station,
                "full_name": serde_json::Value::Null,
                "catchment": "Berne",
                "elevation": elevation,
            },
            "parameter": {
                "column_name": column,
                "display_name": display,
                "section": "chemistry",
            },
            "units": units,
            "hierarchy": {
                "project": SOURCE_SYSTEM.to_uppercase(),
                "site": station,
                "parameter": display,
            },
            "coordinates": {
                "latitude": serde_json::Value::Null,
                "longitude": serde_json::Value::Null,
                "altitude_m": elevation,
            },
        },
        "measurement_type": "spot",
    });
    let (status, resp) =
        crate::common::post_json_parse_with_token(app, "/api/streams/register", &body, token).await;
    assert_eq!(status, 200, "register {station}:{column} ({status}): {resp}");
    resp["id"].as_str().expect("stream id").to_string()
}

fn entry_for<'a>(plan: &'a serde_json::Value, stream_id: &str) -> &'a serde_json::Value {
    plan["entries"]
        .as_array()
        .unwrap_or_else(|| panic!("entries array: {plan}"))
        .iter()
        .find(|e| e["stream_id"] == serde_json::json!(stream_id))
        .unwrap_or_else(|| panic!("entry for stream {stream_id} missing: {plan}"))
}

/// Post apply/revert (both tracked jobs), wait for completion, return the job's detail.counts.
async fn run_plan_action(
    app: &axum::Router,
    token: &str,
    plan_id: &str,
    action: &str,
) -> serde_json::Value {
    let (status, res) = crate::common::post_json_parse_with_token(
        app,
        &format!("/api/sync/pairing-plans/{plan_id}/{action}"),
        &serde_json::json!({}),
        token,
    )
    .await;
    assert_eq!(status, 200, "{action} ({status}): {res}");
    let job_id = res["job_id"].as_str().unwrap_or_else(|| panic!("{action} job_id: {res}"));
    assert_eq!(
        crate::common::e2e::poll_job(app, token, job_id, 30).await,
        "completed",
        "{action} job completes",
    );
    let (_, job) =
        crate::common::get_json_with_token(app, &format!("/api/reprocessing_jobs/{job_id}"), token)
            .await;
    job["detail"]["counts"].clone()
}

async fn seed_catalog_parameters(db: &DatabaseConnection) -> (Uuid, Uuid, Uuid) {
    let nitrate = Uuid::new_v4();
    let water_temp = Uuid::new_v4();
    let turb_fnu = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category, aliases) VALUES \
             ('{nitrate}', 'Nitrate', 'Nitrate', 'mg/L', 'measurement', ARRAY['NO3-N raw']), \
             ('{water_temp}', 'DO_Temperature', 'Water Temperature', '°C', 'measurement', ARRAY[]::text[]), \
             ('{turb_fnu}', 'Turb_FNU', 'Turbidity FNU', 'FNU', 'measurement', ARRAY[]::text[])"
        ),
    )
    .await;
    (nitrate, water_temp, turb_fnu)
}

struct PortalStreams {
    do_field_up: String,
    replicate_1: String,
    replicate_2: String,
    replicate_3: String,
    alias_match: String,
    name_match_up: String,
    do_field_dn: String,
    name_match_dn: String,
}

async fn register_portal_streams(app: &axum::Router, token: &str) -> PortalStreams {
    PortalStreams {
        do_field_up: register_portal_stream(
            app, token, STATION_UP, "WTW_DO_field_mgL", DO_FIELD_DISPLAY, "mg/L", 512.0,
        )
        .await,
        replicate_1: register_portal_stream(
            app, token, STATION_UP, "WTW_DO_mgL_1", "WTW_DO_mgL_1", "mg/L", 512.0,
        )
        .await,
        replicate_2: register_portal_stream(
            app, token, STATION_UP, "WTW_DO_mgL_2", "WTW_DO_mgL_2", "mg/L", 512.0,
        )
        .await,
        replicate_3: register_portal_stream(
            app, token, STATION_UP, "WTW_DO_mgL_3", "WTW_DO_mgL_3", "mg/L", 512.0,
        )
        .await,
        alias_match: register_portal_stream(
            app, token, STATION_UP, "NO3_N_raw", "no3-n RAW", "mg/L", 512.0,
        )
        .await,
        name_match_up: register_portal_stream(
            app, token, STATION_UP, "water_temp", "water temperature", "°C", 512.0,
        )
        .await,
        do_field_dn: register_portal_stream(
            app, token, STATION_DN, "WTW_DO_field_mgL", DO_FIELD_DISPLAY, "mg/L", 471.0,
        )
        .await,
        name_match_dn: register_portal_stream(
            app, token, STATION_DN, "water_temp", "water temperature", "°C", 471.0,
        )
        .await,
    }
}

fn grouped_param<'a>(resp: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    resp["parameters"]
        .as_array()
        .expect("parameters array")
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("parameter '{name}' missing: {resp}"))
}

#[tokio::test]
#[serial]
async fn portal_migration_wizard_full_flow() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (nitrate_id, water_temp_id, turb_fnu_id) = seed_catalog_parameters(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let app = crate::common::build_test_app(db.clone());

    let streams = register_portal_streams(&app, &token).await;

    let ingest_times: Vec<String> =
        (0..4).map(|i| format!("2025-06-01T00:{:02}:00Z", i * 10)).collect();
    let readings: Vec<serde_json::Value> = ingest_times
        .iter()
        .enumerate()
        .map(|(i, t)| serde_json::json!({ "time": t, "raw_value": 8.0 + i as f64 }))
        .collect();
    let (status, ing) = crate::common::post_json_parse_with_token(
        &app,
        "/api/ingest",
        &serde_json::json!({ "stream_id": streams.name_match_up, "readings": readings }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "ingest ({status}): {ing}");
    assert_eq!(ing["inserted"], 4);
    assert_eq!(ing["paired"], false, "stream must still be unpaired: {ing}");

    // Grouped discovery: stations become sites, parameter display names stay intact
    let (status, disco) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/grouped-discovery",
        &serde_json::json!({ "source_system": SOURCE_SYSTEM }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "grouped-discovery ({status}): {disco}");
    assert_eq!(disco["total_streams"], 8);

    let sites = disco["sites"].as_array().expect("sites array");
    let site_names: Vec<&str> = sites.iter().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(site_names, vec![STATION_DN, STATION_UP], "sites are the station names: {disco}");
    let up = sites.iter().find(|s| s["name"] == STATION_UP).unwrap();
    assert_eq!(up["stream_count"], 6);
    assert!(up["existing_id"].is_null());
    let dn = sites.iter().find(|s| s["name"] == STATION_DN).unwrap();
    assert_eq!(dn["stream_count"], 2);

    let param_names: Vec<&str> = disco["parameters"]
        .as_array()
        .expect("parameters array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        param_names.contains(&DO_FIELD_DISPLAY),
        "display name with ' - ' kept whole: {param_names:?}"
    );
    assert!(
        !param_names.contains(&"Field [mg/L]"),
        "display name must not be truncated at ' - ': {param_names:?}"
    );
    for replicate in ["WTW_DO_mgL_1", "WTW_DO_mgL_2", "WTW_DO_mgL_3"] {
        assert!(
            param_names.contains(&replicate),
            "replicate columns stay distinct parameters: {param_names:?}"
        );
    }
    // Grouped discovery resolves existing parameters the same way the plan does:
    // case-insensitively by code, name, or alias.
    assert!(
        !grouped_param(&disco, "no3-n RAW")["existing_id"].is_null(),
        "alias match must resolve in grouped discovery"
    );
    assert!(
        !grouped_param(&disco, "water temperature")["existing_id"].is_null(),
        "name match must resolve in grouped discovery"
    );

    // Draft plan: extraction and catalog resolution
    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": SOURCE_SYSTEM }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    assert_eq!(plan["status"], "draft");
    assert_eq!(plan["summary"]["total_streams"], 8);
    assert_eq!(plan["summary"]["will_pair"], 8);
    assert_eq!(plan["summary"]["sites_to_create"], 2);

    let do_entry = entry_for(&plan, &streams.do_field_up);
    assert_eq!(do_entry["project"]["name"], "CNET");
    assert_eq!(do_entry["site"]["name"], STATION_UP);
    assert_eq!(do_entry["parameter"]["name"], DO_FIELD_DISPLAY);
    assert_eq!(do_entry["parameter"]["create"], true);
    let do_entry_dn = entry_for(&plan, &streams.do_field_dn);
    assert_eq!(do_entry_dn["site"]["name"], STATION_DN);
    assert_eq!(
        do_entry_dn["parameter"]["name"], DO_FIELD_DISPLAY,
        "same display name at both stations proposes one shared parameter"
    );

    let alias_entry = entry_for(&plan, &streams.alias_match);
    assert_eq!(alias_entry["parameter"]["id"], serde_json::json!(nitrate_id));
    assert_eq!(alias_entry["parameter"]["create"], false);
    let name_entry = entry_for(&plan, &streams.name_match_up);
    assert_eq!(name_entry["parameter"]["id"], serde_json::json!(water_temp_id));
    assert_eq!(name_entry["parameter"]["create"], false);

    for id in [&streams.replicate_1, &streams.replicate_2, &streams.replicate_3] {
        assert_eq!(entry_for(&plan, id)["parameter"]["create"], true);
    }

    // Rename replicate 1 to a custom parameter, then converge replicate 2 onto the same
    // will-be-created parameter
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({ "updates": [
            { "stream_id": streams.replicate_1, "parameter_name": "DO Replicate", "parameter_units": "mg/L" },
            { "stream_id": streams.replicate_2, "parameter_name": "DO Replicate" }
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "patch converge ({status}): {body}");
    let plan_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    for id in [&streams.replicate_1, &streams.replicate_2] {
        let e = entry_for(&plan_doc, id);
        assert_eq!(e["parameter"]["name"], "DO Replicate");
        assert_eq!(e["parameter"]["create"], true);
        assert!(e["parameter"]["id"].is_null());
    }

    // Rename replicate 3 to an existing catalog parameter's name: the entry reclassifies to
    // use the existing parameter instead of creating one
    let (status, body) = crate::common::patch_json_with_token(
        &app,
        &format!("/api/sync/pairing-plans/{plan_id}"),
        &serde_json::json!({ "updates": [
            { "stream_id": streams.replicate_3, "parameter_name": "turbidity fnu" }
        ]}),
        &token,
    )
    .await;
    assert_eq!(status, 200, "patch reclassify ({status}): {body}");
    let plan_doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let e = entry_for(&plan_doc, &streams.replicate_3);
    assert_eq!(e["parameter"]["id"], serde_json::json!(turb_fnu_id));
    assert_eq!(e["parameter"]["create"], false);

    // Apply
    let counts = run_plan_action(&app, &token, &plan_id, "apply").await;
    assert_eq!(counts["projects_created"], 1, "{counts}");
    assert_eq!(counts["sites_created"], 2, "{counts}");
    assert_eq!(
        counts["parameters_created"], 2,
        "only the DO field display and the converged replicate are new: {counts}"
    );
    assert_eq!(counts["site_parameters_created"], 7, "{counts}");
    assert_eq!(counts["streams_paired"], 8, "{counts}");
    assert_eq!(counts["readings_backfilled"], 4, "{counts}");

    let data_source = scalar_opt_string(
        &db,
        "SELECT data_source AS v FROM projects WHERE name = 'CNET'",
    )
    .await;
    assert_eq!(
        data_source.as_deref(),
        Some(SOURCE_SYSTEM),
        "project data_source comes from the stream's source system"
    );

    for name in ["Nitrate", "Water Temperature", "Turbidity FNU"] {
        assert_eq!(
            count(&db, &format!("SELECT count(*) AS c FROM parameters WHERE LOWER(name) = LOWER('{name}')")).await,
            1,
            "no duplicate created for existing parameter '{name}'"
        );
    }
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM parameters WHERE code = 'DO Replicate'").await,
        1,
        "converged entries create exactly one parameter"
    );
    let replicate_aliases = scalar_opt_string(
        &db,
        "SELECT array_to_string(aliases, ',') AS v FROM parameters WHERE code = 'DO Replicate'",
    )
    .await
    .unwrap_or_default();
    assert!(
        replicate_aliases.contains("WTW_DO_mgL_1"),
        "created parameter keeps the source column as an alias: {replicate_aliases}"
    );

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams \
             WHERE source_system = '{SOURCE_SYSTEM}' AND site_parameter_id IS NOT NULL"
        )).await,
        8, "all portal streams paired"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
             WHERE ds.id IN ('{}', '{}') AND sp.site_id = (SELECT id FROM sites WHERE name = '{STATION_UP}')",
            streams.replicate_1, streams.replicate_2,
        )).await,
        2, "converged replicates share the station"
    );
    let converged_sp: Vec<Option<String>> = {
        let mut sps = Vec::new();
        for id in [&streams.replicate_1, &streams.replicate_2] {
            sps.push(
                scalar_opt_string(
                    &db,
                    &format!("SELECT site_parameter_id::text AS v FROM data_streams WHERE id = '{id}'"),
                )
                .await,
            );
        }
        sps
    };
    assert_eq!(converged_sp[0], converged_sp[1], "converged replicates share one site_parameter");

    let sp_name = scalar_opt_string(
        &db,
        &format!(
            "SELECT sp.name AS v FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id WHERE ds.id = '{}'",
            streams.do_field_up,
        ),
    )
    .await;
    assert_eq!(
        sp_name.as_deref(),
        Some(DO_FIELD_DISPLAY),
        "created site_parameter carries the display name"
    );
    let sp_units = scalar_opt_string(
        &db,
        &format!(
            "SELECT sp.display_units AS v FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id WHERE ds.id = '{}'",
            streams.do_field_up,
        ),
    )
    .await;
    assert_eq!(sp_units.as_deref(), Some("mg/L"));

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM readings \
             WHERE stream_id = '{}' AND site_id = (SELECT id FROM sites WHERE name = '{STATION_UP}') \
               AND parameter_id = '{water_temp_id}'",
            streams.name_match_up,
        )).await,
        4, "pre-apply readings are attributed to the station and existing parameter"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams ds \
             JOIN site_parameters sp ON ds.site_parameter_id = sp.id \
             WHERE ds.id = '{}' AND sp.parameter_id = '{water_temp_id}' \
               AND sp.site_id = (SELECT id FROM sites WHERE name = '{STATION_DN}')",
            streams.name_match_dn,
        )).await,
        1, "downstream station pairs onto the same existing parameter"
    );

    // Revert: streams unpair and readings un-attribute, but the created catalog stays
    let counts = run_plan_action(&app, &token, &plan_id, "revert").await;
    assert_eq!(counts["reverted"], 8, "{counts}");

    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams \
             WHERE source_system = '{SOURCE_SYSTEM}' AND site_parameter_id IS NULL"
        )).await,
        8, "all portal streams unpaired"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM data_streams \
             WHERE source_system = '{SOURCE_SYSTEM}' AND pairing_plan_id = '{plan_id}'"
        )).await,
        8, "the plan link stays on the streams as the audit trail"
    );
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM readings WHERE stream_id = '{}' AND site_id IS NOT NULL",
            streams.name_match_up,
        )).await,
        0, "reverted readings lose their attribution"
    );
    assert_eq!(count(&db, "SELECT count(*) AS c FROM projects WHERE name = 'CNET'").await, 1);
    assert_eq!(
        count(&db, &format!(
            "SELECT count(*) AS c FROM sites WHERE name IN ('{STATION_UP}', '{STATION_DN}')"
        )).await,
        2, "created sites remain after revert"
    );
    assert_eq!(
        count(&db, "SELECT count(*) AS c FROM parameters WHERE code = 'DO Replicate'").await,
        1, "created parameters remain after revert"
    );

    crate::common::cleanup_test_db(&db).await;
}
