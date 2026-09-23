use super::{SWEPT_REASON, full_reassert_insert, stale_running};
use chrono::{TimeZone, Utc};
use sea_orm::{EntityTrait, QueryFilter, QueryTrait};

use crate::routes::private::sync::models::events;

fn swept_update_sql() -> String {
    events::Entity::update_many()
        .filter(stale_running(
            Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap(),
        ))
        .build(sea_orm::DatabaseBackend::Postgres)
        .to_string()
}

/// Scenario: the sweeper closes cycles a service stopped reporting on.
/// Expected behaviour: it selects on the two columns that say so, and the cut-off is a bound
/// instant rather than an interval the statement re-parses.
#[test]
fn test_stale_running_selects_on_status_and_start() {
    let sql = swept_update_sql();
    assert!(
        sql.contains("\"status\" = 'running'"),
        "only a running cycle is stale: {sql}"
    );
    assert!(
        sql.contains("\"started_at\" < '2026-09-11 12:00:00"),
        "the cut-off is the instant, not an interval expression: {sql}"
    );
    assert!(
        !sql.contains("seconds"),
        "no interval literal is built from a number: {sql}"
    );
}

/// The sentinel the sweep appends is read back by the test that proves the append, so it is one
/// constant rather than two strings.
#[test]
fn test_the_swept_reason_names_the_sweeper() {
    assert_eq!(SWEPT_REASON, "Closed by sweeper: service stopped reporting");
}

/// Scenario: the weekly re-assert queues a full sync for each live service that opted in.
/// Expected behaviour: one built statement whose cut-off and expiry are bound instants, and
/// which skips a service already holding a pending full sync.
#[test]
fn test_full_reassert_insert_binds_its_instants() {
    let now = Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).unwrap();
    let sql = full_reassert_insert(now, 600)
        .unwrap()
        .to_string(sea_orm::sea_query::PostgresQueryBuilder);
    assert!(sql.starts_with("INSERT INTO \"sync_commands\""), "{sql}");
    assert!(sql.contains("GEN_RANDOM_UUID()"), "{sql}");
    assert!(sql.contains("\"full_reassert_enabled\""), "{sql}");
    assert!(
        sql.contains("\"last_heartbeat\" > '2026-09-23 11:00:00"),
        "an hour-old heartbeat is the live cut-off: {sql}"
    );
    assert!(
        sql.contains("'2026-09-23 12:10:00"),
        "the expiry is the instant, not an interval: {sql}"
    );
    assert!(sql.contains("NOT EXISTS"), "{sql}");
    assert!(sql.contains("RETURNING \"service_id\""), "{sql}");
    assert!(!sql.contains("seconds"), "{sql}");
}
