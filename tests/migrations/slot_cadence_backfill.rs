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
