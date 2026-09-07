//! Scenario: the pairing review shows how much a catalog parameter is already used.
//!
//! Expected behaviour: the counts are the counts. `SUM` over a bigint is NUMERIC in Postgres, so a
//! reading count decoded as an integer fails; read through a default it came back zero for every
//! parameter and said so on the review surface (C108).
//!
//! Run: cargo test --test sync catalog_usage -- --test-threads=1

use sea_orm::DatabaseConnection;
use serial_test::serial;

use river_db::routes::private::sync::service::load_entity_catalog;

use crate::common::{GLOBAL_PARAM_DO_ID, PARAM_S1_DO_ID, SITE1_ID};

async fn setup() -> DatabaseConnection {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    db
}

#[tokio::test]
#[serial]
async fn a_parameter_with_readings_does_not_report_none() {
    let db = setup().await;
    let before = count_of(&db).await;
    let stream = uuid::Uuid::new_v4();
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, source_name, \
                                       site_parameter_id, paired_at) \
             VALUES ('{stream}', 'api', 'catalog-usage', 'Catalog usage', '{PARAM_S1_DO_ID}', NOW())"
        ),
    )
    .await;
    for (i, value) in [(0, 1.0), (1, 2.0), (2, 3.0)] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO readings (stream_id, time, replicate_index, raw_value, site_id, \
                                       parameter_id, measurement_type) \
                 VALUES ('{stream}', '2025-06-0{}T00:00:00Z', 0, {value}, '{SITE1_ID}', \
                         '{GLOBAL_PARAM_DO_ID}', 'continuous')",
                i + 1
            ),
        )
        .await;
    }

    assert_eq!(
        count_of(&db).await,
        before + 3,
        "the readings are counted, not defaulted away"
    );
    assert!(before > 0, "the seeded readings are counted too: {before}");
}

/// The catalog's reading count for the seeded parameter.
async fn count_of(db: &DatabaseConnection) -> i64 {
    let catalog = load_entity_catalog(db).await.expect("catalog loads");
    catalog
        .params
        .iter()
        .find(|p| p.id.to_string() == GLOBAL_PARAM_DO_ID)
        .expect("the seeded parameter is in the catalog")
        .reading_count
}
