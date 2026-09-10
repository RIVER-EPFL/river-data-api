//! Scenario: a database carried forward holds replicate groups whose samples were computed before
//! the median and the two named standard deviations existed as columns.
//!
//! Expected behaviour: the backfill fills all three from the replicates themselves, by the same
//! arithmetic and the same exclusions the trigger uses, and the declared `stdev` follows them,
//! since it is generated from the pair.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use migration::m20260907_000003_sample_statistics::BACKFILL;

const STREAM_ID: &str = "00000000-0000-4000-eb00-000000000001";
const AT: &str = "2025-01-15T12:00:00Z";

struct Stats {
    median: Option<f64>,
    stdev_sample: Option<f64>,
    stdev_population: Option<f64>,
    stdev: Option<f64>,
}

async fn stats_of(db: &DatabaseConnection, sample_id: Uuid) -> Stats {
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT median, stdev_sample, stdev_population, stdev \
                 FROM samples WHERE id = '{sample_id}'"
            ),
        ))
        .await
        .expect("query")
        .expect("the sample");
    Stats {
        median: row.try_get("", "median").ok(),
        stdev_sample: row.try_get("", "stdev_sample").ok(),
        stdev_population: row.try_get("", "stdev_population").ok(),
        stdev: row.try_get("", "stdev").ok(),
    }
}

/// A group of replicates, with the three columns cleared afterwards: the trigger fills them on
/// insert, and a row that predates the migration is one that never had them.
async fn seed_group_without_the_new_columns(
    db: &DatabaseConnection,
    values: &[(i16, f64, bool)],
) -> Uuid {
    crate::common::seed_test_data(db).await;
    crate::common::seed_data_stream(db, STREAM_ID, "grab_sample", "grab_sample:stats").await;
    let sample_id = Uuid::new_v4();
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO samples (id, site_id, parameter_id, collected_at) \
             VALUES ('{sample_id}', '{}', '{}', '{AT}')",
            crate::common::SITE1_ID,
            crate::common::GLOBAL_PARAM_TEMP_ID
        ),
    )
    .await;
    for (index, value, flagged) in values {
        crate::common::exec(
            db,
            &format!(
                "INSERT INTO readings \
                 (stream_id, time, replicate_index, site_id, parameter_id, raw_value, \
                  sample_id, is_flagged, measurement_type) \
                 VALUES ('{STREAM_ID}', '{AT}', {index}, '{}', '{}', {value}, \
                         '{sample_id}', {flagged}, 'spot')",
                crate::common::SITE1_ID,
                crate::common::GLOBAL_PARAM_TEMP_ID
            ),
        )
        .await;
    }
    crate::common::exec(
        db,
        &format!(
            "UPDATE samples SET median = NULL, stdev_sample = NULL, stdev_population = NULL \
             WHERE id = '{sample_id}'"
        ),
    )
    .await;
    sample_id
}

#[tokio::test]
#[serial]
async fn the_backfill_fills_the_three_columns_from_the_replicates() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let sample_id = seed_group_without_the_new_columns(
        &db,
        &[(0, 10.0), (1, 20.0), (2, 60.0)].map(|(i, v)| (i, v, false)),
    )
    .await;

    let before = stats_of(&db, sample_id).await;
    assert!(
        before.median.is_none() && before.stdev_sample.is_none(),
        "the fixture stands in for a row that predates the columns"
    );

    crate::common::exec_unprepared(&db, BACKFILL).await;

    let after = stats_of(&db, sample_id).await;
    assert_eq!(after.median, Some(20.0), "the middle of 10, 20, 60");
    // population sd of 10, 20, 60: sqrt(((10-30)^2 + (20-30)^2 + (60-30)^2) / 3)
    let pop = after.stdev_population.expect("population sd");
    assert!((pop - (1400.0f64 / 3.0).sqrt()).abs() < 1e-9, "{pop}");
    // sample sd: the same numerator over n-1
    let samp = after.stdev_sample.expect("sample sd");
    assert!((samp - (1400.0f64 / 2.0).sqrt()).abs() < 1e-9, "{samp}");
    assert!(
        samp > pop,
        "the two divisors are not the same number, which is why both are stored"
    );

    crate::common::cleanup_test_db(&db).await;
}

/// The backfill reads what the trigger reads: a flagged replicate is out of the group, so a
/// backfilled statistic and a recomputed one are the same number.
#[tokio::test]
#[serial]
async fn the_backfill_excludes_a_flagged_replicate() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let sample_id = seed_group_without_the_new_columns(
        &db,
        &[(0, 10.0, false), (1, 20.0, false), (2, 900.0, true)],
    )
    .await;

    crate::common::exec_unprepared(&db, BACKFILL).await;

    let after = stats_of(&db, sample_id).await;
    assert_eq!(
        after.median,
        Some(15.0),
        "the flagged replicate is not in the median"
    );
    assert!(
        (after.stdev_population.expect("population sd") - 5.0).abs() < 1e-9,
        "nor in the standard deviations"
    );
    assert_eq!(
        after.stdev, after.stdev_sample,
        "`stdev` is generated from the two the backfill writes, under an undeclared slot's \
         sample divisor, so it follows them"
    );

    crate::common::cleanup_test_db(&db).await;
}
