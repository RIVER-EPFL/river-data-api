//! The two health probes are built rather than written out, so what proves them is Postgres
//! accepting them: a LATERAL with a window function, and a correlated regression over a night
//! window. Both are syntax no unit test can check.
//!
//! Run: cargo test --test notifications probe_sql -- --test-threads=1

use river_db::routes::private::notifications::flows;
use sea_orm::ConnectionTrait;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn the_health_probes_are_sql_postgres_runs() {
    let db = crate::common::setup_test_db().await;

    db.query_all_raw(flows::stale_slots_query())
        .await
        .expect("the stale-slot probe runs");
    db.query_all_raw(flows::battery_trend_query(uuid::Uuid::nil()))
        .await
        .expect("the battery-trend probe runs");

    crate::common::cleanup_test_db(&db).await;
}
