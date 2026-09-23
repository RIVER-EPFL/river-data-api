//! Scenario: slots created before `site_parameters.cadence` existed, all at its `high` default:
//! one paired to a spot stream, one to a continuous stream, one to both, an unpaired one holding
//! only spot readings, and an unpaired one holding nothing.
//!
//! Expected behaviour: `m20260923_000002_slot_cadence_backfill` declares the spot-paired and the
//! spot-only slots `low`, the rule a slot created now takes from its stream, and leaves the rest.
//!
//! Run: cargo test --test migrations slot_cadence_backfill -- --test-threads=1

use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

use crate::common::scratch;

const BACKFILL: &str = "m20260923_000002_slot_cadence_backfill";
const PROJECT: &str = "11111111-1111-1111-1111-111111111111";
const SITE: &str = "22222222-2222-2222-2222-222222222222";
const SPOT: &str = "33333333-3333-3333-3333-333333333331";
const CONTINUOUS: &str = "33333333-3333-3333-3333-333333333332";
const MIXED: &str = "33333333-3333-3333-3333-333333333333";
const UNPAIRED_SPOT: &str = "33333333-3333-3333-3333-333333333334";
const EMPTY: &str = "33333333-3333-3333-3333-333333333335";
const SENSOR: &str = "66666666-6666-6666-6666-666666666666";
const AT: &str = "2025-06-15T09:00:00Z";

#[tokio::test]
#[serial]
async fn a_slot_fed_only_by_grabs_is_declared_low() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_cadence_{}", std::process::id());
    let server = scratch::server(&base).await;
    scratch::discard(&server, &name).await;
    server
        .execute_unprepared(&format!("CREATE DATABASE {name}"))
        .await
        .expect("create the scratch database");
    let db = Database::connect(scratch::url_for(&base, &name))
        .await
        .expect("connect to the scratch database");

    let before = migration::Migrator::migrations()
        .iter()
        .position(|m| m.name() == BACKFILL)
        .expect("the backfill is registered");
    migration::Migrator::up(&db, Some(u32::try_from(before).expect("a step count")))
        .await
        .expect("migrate to the step before the backfill");

    let mut seed = vec![
        format!("INSERT INTO projects (id, name) VALUES ('{PROJECT}', 'p')"),
        format!("INSERT INTO sites (id, project_id, name) VALUES ('{SITE}', '{PROJECT}', 's')"),
        format!("INSERT INTO sensors (id) VALUES ('{SENSOR}')"),
    ];
    for (i, slot) in [SPOT, CONTINUOUS, MIXED, UNPAIRED_SPOT, EMPTY]
        .into_iter()
        .enumerate()
    {
        seed.push(format!(
            "INSERT INTO parameters (id, code, name, category) \
             VALUES ('{slot}', 'p{i}', 'p{i}', 'measurement')"
        ));
        seed.push(format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, is_active) \
             VALUES ('{slot}', '{SITE}', '{slot}', 'p{i}', true)"
        ));
    }
    let streams = [
        (SPOT, "spot", true),
        (CONTINUOUS, "continuous", true),
        (MIXED, "spot", true),
        (MIXED, "continuous", true),
        (UNPAIRED_SPOT, "spot", false),
    ];
    for (i, (slot, kind, paired)) in streams.into_iter().enumerate() {
        let stream = format!("44444444-4444-4444-4444-44444444444{i}");
        let pairing = if paired {
            format!("'{slot}'")
        } else {
            "NULL".to_string()
        };
        seed.push(format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active, measurement_type, \
                 site_parameter_id) \
             VALUES ('{stream}', 'test', 'k{i}', true, '{kind}', {pairing})"
        ));
        seed.push(format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, sensor_id, time, \
                 replicate_index, raw_value, measurement_type) \
             VALUES ('{stream}', '{SITE}', '{slot}', '{SENSOR}', '{AT}', 0, 1.0, '{kind}')"
        ));
    }
    for sql in &seed {
        db.execute_unprepared(sql).await.expect(sql);
    }

    migration::Migrator::up(&db, None)
        .await
        .expect("apply the remaining steps");

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT id::text AS id, cadence FROM site_parameters ORDER BY id",
        ))
        .await
        .expect("read the slots");
    let declared: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.try_get("", "id").unwrap(),
                r.try_get("", "cadence").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        declared,
        vec![
            (SPOT.to_string(), "low".to_string()),
            (CONTINUOUS.to_string(), "high".to_string()),
            (MIXED.to_string(), "high".to_string()),
            (UNPAIRED_SPOT.to_string(), "low".to_string()),
            (EMPTY.to_string(), "high".to_string()),
        ],
        "spot-fed slots are low; a continuous, mixed or empty slot keeps high"
    );

    drop(db);
    scratch::discard(&server, &name).await;
}

const OUTPUT_SLOT_CADENCE: &str = "m20260923_000006_output_slot_cadence";

/// Scenario: calculation outputs applied at a site while every slot there read `high`, so each
/// output took `high`: one over two grab inputs, one over a logger input, one over a logger input
/// and a grab input held from the last visit, and one over a grab replicate family.
///
/// Expected behaviour: once the backfill lowers the grab inputs, `m20260923_000006` re-derives
/// each output the way Apply calculation does: the grab-fed and replicate-fed outputs are `low`,
/// and the logger-fed ones stay `high`, the held grab input deciding nothing.
#[tokio::test]
#[serial]
async fn an_output_slot_follows_the_inputs_it_reads_at_the_instant() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_output_cadence_{}", std::process::id());
    let server = scratch::server(&base).await;
    scratch::discard(&server, &name).await;
    server
        .execute_unprepared(&format!("CREATE DATABASE {name}"))
        .await
        .expect("create the scratch database");
    let db = Database::connect(scratch::url_for(&base, &name))
        .await
        .expect("connect to the scratch database");

    let before = migration::Migrator::migrations()
        .iter()
        .position(|m| m.name() == BACKFILL)
        .expect("the backfill is registered");
    assert!(
        migration::Migrator::migrations()
            .iter()
            .any(|m| m.name() == OUTPUT_SLOT_CADENCE),
        "the output re-derivation is registered"
    );
    migration::Migrator::up(&db, Some(u32::try_from(before).expect("a step count")))
        .await
        .expect("migrate to the step before the backfill");

    let mut seed = vec![
        format!("INSERT INTO projects (id, name) VALUES ('{PROJECT}', 'p')"),
        format!("INSERT INTO sites (id, project_id, name) VALUES ('{SITE}', '{PROJECT}', 's')"),
    ];
    // Inputs, each paired to one stream of its kind; outputs, unpaired and empty.
    let slots = [
        ("grab_a", Some("spot")),
        ("grab_b", Some("spot")),
        ("logger_c", Some("continuous")),
        ("grab_held", Some("spot")),
        ("out_grab", None),
        ("out_logger", None),
        ("out_held", None),
        ("out_family", None),
    ];
    for (i, (code, kind)) in slots.into_iter().enumerate() {
        let id = format!("77777777-7777-7777-7777-77777777777{i}");
        seed.push(format!(
            "INSERT INTO parameters (id, code, name, category) \
             VALUES ('{id}', '{code}', '{code}', 'measurement')"
        ));
        seed.push(format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, is_active) \
             VALUES ('{id}', '{SITE}', '{id}', '{code}', true)"
        ));
        if let Some(kind) = kind {
            seed.push(format!(
                "INSERT INTO data_streams (id, source_system, source_key, is_active, \
                     measurement_type, site_parameter_id) \
                 VALUES ('88888888-8888-8888-8888-88888888888{i}', 'test', '{code}', true, \
                     '{kind}', '{id}')"
            ));
        }
    }
    let calculations = [
        (
            "calc_grab",
            serde_json::json!({
                "event_inputs": [
                    { "param": "a", "parameter_code": "grab_a" },
                    { "param": "b", "parameter_code": "grab_b" },
                ],
                "outputs": [{ "key": "o", "label": "o", "suggested_parameter_code": "out_grab" }],
            }),
        ),
        (
            "calc_logger",
            serde_json::json!({
                "event_inputs": [{ "param": "c", "parameter_code": "logger_c" }],
                "outputs": [{ "key": "o", "label": "o", "suggested_parameter_code": "OUT_LOGGER" }],
            }),
        ),
        (
            "calc_held",
            serde_json::json!({
                "event_inputs": [
                    { "param": "c", "parameter_code": "logger_c" },
                    { "param": "h", "parameter_code": "grab_held", "alignment": "hold" },
                ],
                "outputs": [{ "key": "o", "label": "o", "suggested_parameter_code": "out_held" }],
            }),
        ),
        (
            "calc_family",
            serde_json::json!({
                "params": [{ "name": "r", "kind": "replicates", "parameter_code": "grab_a" }],
                "outputs": [{ "key": "o", "label": "o", "suggested_parameter_code": "out_family" }],
            }),
        ),
    ];
    for (calc, manifest) in calculations {
        seed.push(format!(
            "INSERT INTO tool_scripts (name, label, created_by) VALUES ('{calc}', '{calc}', 'test')"
        ));
        seed.push(format!(
            "INSERT INTO tool_script_versions (tool_script_id, version_no, script, entry_function, \
                 manifest, test_cases, content_hash, created_by) \
             SELECT id, 1, 'tool <- function(inputs, constants, curves) list()', 'tool', \
                 '{manifest}'::jsonb, '{{}}'::jsonb, md5('{calc}'), 'test' \
             FROM tool_scripts WHERE name = '{calc}'"
        ));
        seed.push(format!(
            "UPDATE tool_scripts s SET active_version_id = v.id FROM tool_script_versions v \
             WHERE v.tool_script_id = s.id AND s.name = '{calc}'"
        ));
    }
    for sql in &seed {
        db.execute_unprepared(sql).await.expect(sql);
    }

    migration::Migrator::up(&db, None)
        .await
        .expect("apply the remaining steps");

    let rows = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT p.code, sp.cadence FROM site_parameters sp \
             JOIN parameters p ON p.id = sp.parameter_id \
             WHERE p.code LIKE 'out_%' ORDER BY p.code",
        ))
        .await
        .expect("read the output slots");
    let declared: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r.try_get("", "code").unwrap(),
                r.try_get("", "cadence").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        declared,
        vec![
            ("out_family".to_string(), "low".to_string()),
            ("out_grab".to_string(), "low".to_string()),
            ("out_held".to_string(), "high".to_string()),
            ("out_logger".to_string(), "high".to_string()),
        ],
        "an output reading only lowered inputs is low; one whose instant inputs are all loggers keeps high"
    );

    drop(db);
    scratch::discard(&server, &name).await;
}
