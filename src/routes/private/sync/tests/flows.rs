use super::{SWEPT_REASON, stale_running};
use chrono::{TimeZone, Utc};
use sea_orm::{EntityTrait, QueryFilter, QueryTrait};

use crate::routes::private::sync::models::events;

fn swept_update_sql() -> String {
    events::Entity::update_many()
        .filter(stale_running(Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap()))
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
