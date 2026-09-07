//! One sync cycle of the fake portal, through the real driver, into the real API.
//!
//! This is what makes `common::fake_portal` a fixture rather than a file: it proves the
//! descriptors it emits register, that the pinned replicate assignments come back to it, and that
//! a windowed pass lands. The story on top of it, the pairing plan, the second cycle's digest and
//! a historical edit accepted as a correction, is T38.
//!
//! Run: cargo test --test sync fake_portal_cycle -- --test-threads=1

use river_data_core::client::SyncService;
use serial_test::serial;

use crate::common::fake_portal::{
    FAMILY_MEAN_COLUMN, FAMILY_MEMBERS, FakePortal, SOURCE_SYSTEM, STATIONS, enrolled_driver,
};

#[tokio::test]
#[serial]
async fn the_fake_portal_registers_and_pushes_through_the_real_driver() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let driver = enrolled_driver(app, &state, FakePortal::seeded()).await;

    let result = driver.sync(false).await.expect("one cycle");
    assert!(
        result.errors.is_empty(),
        "the cycle reported errors: {:?}",
        result.errors
    );

    // Two stations, each with a single column and a replicate family.
    let streams = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM data_streams WHERE source_system = '{SOURCE_SYSTEM}'"
        ),
    )
    .await;
    assert_eq!(streams, (STATIONS.len() * 2) as i64, "registered streams");

    // The family's three columns are pinned to three distinct indexes, and the readings carry
    // them: 3 replicates over 3 visits at each station, less the one empty cell per station.
    for station in STATIONS {
        let family_key = format!("{station}:{FAMILY_MEAN_COLUMN}:reps");
        let rows = crate::common::e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint FROM readings r JOIN data_streams s ON s.id = r.stream_id \
                 WHERE s.source_system = '{SOURCE_SYSTEM}' AND s.source_key = '{family_key}'"
            ),
        )
        .await;
        assert_eq!(rows, 8, "replicate readings at {station}");
        let indexes = crate::common::e2e::count(
            &db,
            &format!(
                "SELECT COUNT(DISTINCT r.replicate_index)::bigint FROM readings r \
                 JOIN data_streams s ON s.id = r.stream_id \
                 WHERE s.source_system = '{SOURCE_SYSTEM}' AND s.source_key = '{family_key}'"
            ),
        )
        .await;
        assert_eq!(
            indexes,
            FAMILY_MEMBERS.len() as i64,
            "one index per source column at {station}"
        );
    }

    // The gap in the source is a gap in the store: S02's second visit has no single value.
    let singles = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM readings r JOIN data_streams s ON s.id = r.stream_id \
             WHERE s.source_system = '{SOURCE_SYSTEM}' AND s.source_key = 'S02:water_temp_degC'"
        ),
    )
    .await;
    assert_eq!(singles, 2, "the empty cell is not a stored reading");

    // Every pass declared its window, so each stream committed a receipt.
    let receipts = crate::common::e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint FROM ingest_receipts ir \
             JOIN data_streams s ON s.id = ir.stream_id \
             WHERE s.source_system = '{SOURCE_SYSTEM}'"
        ),
    )
    .await;
    assert_eq!(
        receipts,
        (STATIONS.len() * 2) as i64,
        "one receipt per windowed stream"
    );
}
