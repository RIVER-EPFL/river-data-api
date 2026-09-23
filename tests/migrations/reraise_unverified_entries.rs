//! Scenario: a reading awaiting a manager's ruling whose `unverified_entry` hold was superseded,
//! beside one whose hold is still pending and one already withdrawn.
//!
//! Expected behaviour: `m20260923_000001_reraise_unverified_entries` opens one pending hold for
//! the first, naming who entered it, and leaves the other two alone.
//!
//! Run: cargo test --test migrations reraise_unverified_entries -- --test-threads=1

use sea_orm::{ConnectionTrait, Database, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

use crate::common::scratch;

const RERAISE: &str = "m20260923_000001_reraise_unverified_entries";
const SITE: &str = "22222222-2222-2222-2222-222222222222";
const STREAM: &str = "44444444-4444-4444-4444-444444444444";
const SENSOR: &str = "66666666-6666-6666-6666-666666666666";
const LOST: &str = "55555555-5555-5555-5555-555555555551";
const HELD: &str = "55555555-5555-5555-5555-555555555552";
const WITHDRAWN: &str = "55555555-5555-5555-5555-555555555553";
const AT: &str = "2025-06-15T09:00:00Z";

#[tokio::test]
#[serial]
async fn a_pending_entry_whose_hold_was_superseded_is_back_in_the_queue() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_reraise_{}", std::process::id());
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
        .position(|m| m.name() == RERAISE)
        .expect("the re-raise is registered");
    migration::Migrator::up(&db, Some(u32::try_from(before).expect("a step count")))
        .await
        .expect("migrate to the step before the re-raise");

    let mut seed = vec![
        "INSERT INTO projects (id, name) VALUES ('11111111-1111-1111-1111-111111111111', 'p')"
            .to_string(),
        format!(
            "INSERT INTO sites (id, project_id, name) \
             VALUES ('{SITE}', '11111111-1111-1111-1111-111111111111', 's')"
        ),
        format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{STREAM}', 'grab_sample', 'k', true)"
        ),
        format!("INSERT INTO sensors (id) VALUES ('{SENSOR}')"),
    ];
    for (i, parameter) in [LOST, HELD, WITHDRAWN].into_iter().enumerate() {
        seed.push(format!(
            "INSERT INTO parameters (id, code, name, category) \
             VALUES ('{parameter}', 'p{i}', 'p{i}', 'measurement')"
        ));
        seed.push(format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, sensor_id, time, \
                 replicate_index, raw_value, measurement_type, unverified, withdrawn_at) \
             VALUES ('{STREAM}', '{SITE}', '{parameter}', '{SENSOR}', '{AT}', {i}, 1.0, 'spot', true, {})",
            if parameter == WITHDRAWN {
                "now()"
            } else {
                "NULL"
            }
        ));
        seed.push(format!(
            "INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, actor, origin) \
             VALUES ('{STREAM}', '{AT}', {i}, 'unverified_entry', 'intern-a', 'manual')"
        ));
    }
    for (parameter, status) in [(LOST, "superseded"), (HELD, "pending")] {
        seed.push(format!(
            "INSERT INTO replicate_audit_holds (site_id, parameter_id, group_time, kind, \
                 expected, computed, delta, status) \
             VALUES ('{SITE}', '{parameter}', '{AT}', 'unverified_entry', '{{}}', '{{}}', '{{}}', \
                     '{status}')"
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
            "SELECT parameter_id::text AS p, computed->>'entered_by' AS by \
               FROM replicate_audit_holds \
              WHERE kind = 'unverified_entry' AND status = 'pending' ORDER BY parameter_id",
        ))
        .await
        .expect("read the holds");
    let pending: Vec<(String, Option<String>)> = rows
        .iter()
        .map(|r| (r.try_get("", "p").unwrap(), r.try_get("", "by").unwrap()))
        .collect();
    assert_eq!(
        pending,
        vec![
            (LOST.to_string(), Some("intern-a".to_string())),
            (HELD.to_string(), None),
        ],
        "the lost entry is back, the held one is not doubled, the withdrawn one is not raised"
    );

    drop(db);
    scratch::discard(&server, &name).await;
}
