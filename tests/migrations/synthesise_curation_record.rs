//! Scenario: a database carried forward from the production tag holds rows an operator flagged,
//! withdrew or curved by hand, and the curation record is introduced underneath them.
//!
//! Expected behaviour: every curated row ends up with one decision it could have been decided by,
//! stamped `migration` and attributed to nobody, projecting exactly the columns it was
//! synthesised from; an uncurated row gets none; and the drift sweep, which reports a column no
//! decision stands behind, then reports nothing.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_TEMP_ID, SITE1_ID, cleanup_test_db, seed_test_data, setup_test_db};

const AT: &str = "2025-06-15T10:00:00Z";

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The migration's own SQL, run the way the migrator runs it: several statements in one implicit
/// transaction, which is what `SET LOCAL` needs.
async fn run_migration(db: &DatabaseConnection) {
    db.execute_unprepared(
        &migration::m20260907_000006_synthesise_curation_record::synthesise_curation_record(),
    )
    .await
    .expect("the synthesis applies");
}

async fn scalar_i64(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}

/// Four rows in the shape an upgrade finds them: flagged, withdrawn, hand-curved, and plain. The
/// columns are written directly, which is exactly what the old code did and what leaves them with
/// no decision behind them.
async fn seed_curated_rows(db: &DatabaseConnection) -> Uuid {
    let stream = crate::common::sensor_lifecycle::create_paired_stream(
        db,
        "upgrade-temp",
        crate::common::PARAM_S1_TEMP_ID,
    )
    .await;
    let curve_id = Uuid::new_v4();
    let sensor_id: Uuid = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!("SELECT sensor_id AS id FROM data_streams WHERE id = '{stream}'"),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "id")
        .unwrap();
    exec(
        db,
        &format!(
            "INSERT INTO standard_curves (id, sensor_id, slope, intercept, name) \
             VALUES ('{curve_id}', '{sensor_id}', 2.0, 1.0, 'bench')"
        ),
    )
    .await;
    for i in 0..4 {
        exec(
            db,
            &format!(
                "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value, \
                 replicate_index, measurement_type, ingested_at) \
                 VALUES ('{stream}', '{SITE1_ID}', '{GLOBAL_PARAM_TEMP_ID}', '{AT}', {i}.0, {i}, \
                         'spot', '2025-06-16T00:00:00Z')"
            ),
        )
        .await;
    }
    for sql in [
        "UPDATE readings SET is_flagged = TRUE, flag_reason = 'vial cracked' \
         WHERE stream_id = '{s}' AND time = '{AT}' AND replicate_index = 0",
        "UPDATE readings SET withdrawn_at = '2025-06-20T00:00:00Z', \
                withdrawn_reason = 'retracted at source' \
         WHERE stream_id = '{s}' AND time = '{AT}' AND replicate_index = 1",
        "UPDATE readings SET standard_curve_id = '{c}' \
         WHERE stream_id = '{s}' AND time = '{AT}' AND replicate_index = 2",
    ] {
        exec(
            db,
            &sql.replace("{s}", &stream.to_string())
                .replace("{AT}", AT)
                .replace("{c}", &curve_id.to_string()),
        )
        .await;
    }
    stream
}

#[tokio::test]
#[serial]
async fn every_curated_row_gets_the_decision_it_could_have_been_decided_by() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let stream = seed_curated_rows(&db).await;

    let drift_sql = river_db::routes::private::readings::decisions::inconsistent_rows_sql();
    assert_eq!(
        scalar_i64(&db, &format!("SELECT count(*)::bigint AS n FROM ({drift_sql}) d")).await,
        2,
        "the flag and the retraction stand behind no decision before the synthesis"
    );

    run_migration(&db).await;

    let decided = |kind: &str, index: i16| {
        format!(
            "SELECT count(*)::bigint AS n FROM reading_decisions \
             WHERE stream_id = '{stream}' AND time = '{AT}' AND replicate_index = {index} \
               AND kind = '{kind}' AND origin = 'migration' AND actor = 'unknown'"
        )
    };
    for (kind, index) in [("flag", 0), ("withdraw", 1), ("curve", 2)] {
        assert_eq!(
            scalar_i64(&db, &decided(kind, index)).await,
            1,
            "one {kind} for replicate {index}"
        );
    }
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*)::bigint AS n FROM reading_decisions \
                 WHERE stream_id = '{stream}' AND time = '{AT}' AND replicate_index = 3"
            )
        )
        .await,
        0,
        "a row nobody curated is decided by nothing"
    );

    // The projection wrote back what it found: the synthesis records the curation, it does not
    // change it.
    let row = db
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT (SELECT flag_reason FROM readings WHERE stream_id = '{stream}' \
                          AND time = '{AT}' AND replicate_index = 0) AS reason, \
                        (SELECT withdrawn_at FROM readings WHERE stream_id = '{stream}' \
                          AND time = '{AT}' AND replicate_index = 1) AS withdrawn_at, \
                        (SELECT withdrawn_reason FROM readings WHERE stream_id = '{stream}' \
                          AND time = '{AT}' AND replicate_index = 1) AS withdrawn_reason"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.try_get::<Option<String>>("", "reason").unwrap().as_deref(),
        Some("vial cracked")
    );
    assert_eq!(
        row.try_get::<Option<String>>("", "withdrawn_reason")
            .unwrap()
            .as_deref(),
        Some("retracted at source")
    );
    assert_eq!(
        row.try_get::<Option<sea_orm::prelude::DateTimeWithTimeZone>>("", "withdrawn_at")
            .unwrap()
            .map(|t| t.to_rfc3339()),
        Some(
            chrono::DateTime::parse_from_rfc3339("2025-06-20T00:00:00Z")
                .unwrap()
                .to_rfc3339()
        ),
        "the stamp is the one the row already carried, not the migration's clock"
    );

    assert_eq!(
        scalar_i64(&db, &format!("SELECT count(*)::bigint AS n FROM ({drift_sql}) d")).await,
        0,
        "the record is total, so the sweep reports nothing"
    );

    // Rerunnable: a second pass finds every row already decided and adds nothing.
    let before = scalar_i64(&db, "SELECT count(*)::bigint AS n FROM reading_decisions").await;
    run_migration(&db).await;
    assert_eq!(
        scalar_i64(&db, "SELECT count(*)::bigint AS n FROM reading_decisions").await,
        before
    );
}

#[tokio::test]
#[serial]
async fn a_decision_someone_took_is_not_synthesised_over() {
    let db = setup_test_db().await;
    cleanup_test_db(&db).await;
    seed_test_data(&db).await;
    let stream = seed_curated_rows(&db).await;

    // The flag was taken by a person before the upgrade reached this row.
    exec(
        &db,
        &format!(
            "INSERT INTO reading_decisions \
                 (stream_id, time, replicate_index, kind, old, new, actor, origin) \
             VALUES ('{stream}', '{AT}', 0, 'flag', '{{}}'::jsonb, \
                     '{{\"reason\": \"vial cracked\"}}'::jsonb, 'evan', 'manual')"
        ),
    )
    .await;

    run_migration(&db).await;

    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*)::bigint AS n FROM reading_decisions \
                 WHERE stream_id = '{stream}' AND time = '{AT}' AND replicate_index = 0 \
                   AND kind = 'flag'"
            )
        )
        .await,
        1,
        "the person's decision stands alone"
    );
    assert_eq!(
        scalar_i64(
            &db,
            &format!(
                "SELECT count(*)::bigint AS n FROM reading_decisions \
                 WHERE stream_id = '{stream}' AND time = '{AT}' AND replicate_index = 0 \
                   AND actor = 'evan'"
            )
        )
        .await,
        1
    );
}
