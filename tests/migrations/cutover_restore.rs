//! Scenario: production is dumped, a new database is built from the baseline and rebuilt to the
//! point where its streams, sites, parameters and instruments exist, and the dump's curated state
//! is carried across (Q138, M189, M190).
//!
//! Expected behaviour: the readings, their curation ledger, the sample statistics and the public
//! API setups arrive intact under the rebuilt database's own ids. The assertion is a content hash
//! per table over every column, with the id references replaced by the natural keys the two
//! databases share, so a column the restore forgets fails it.
//!
//! Run: cargo test --test migrations cutover_restore -- --test-threads=1

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use serial_test::serial;

/// What a rebuild mints before anything is carried into it: both databases run this, so every id
/// the restore resolves differs between them.
const METADATA: &str = r"
INSERT INTO projects (name, description) VALUES ('Breathe', 'Alpine streams');
INSERT INTO sites (project_id, name, latitude, longitude, altitude_m)
     SELECT id, 'Martigny', 46.1, 7.07, 471 FROM projects WHERE name = 'Breathe';
INSERT INTO parameters (code, name, default_units) VALUES ('doc', 'DOC', 'ppb');
INSERT INTO site_parameters (site_id, parameter_id, name)
     SELECT s.id, p.id, 'Martigny DOC' FROM sites s, parameters p
      WHERE s.name = 'Martigny' AND p.code = 'doc';
INSERT INTO sensors (serial_number, name, data_frequency) VALUES ('SN-1', 'DOC probe', 'low');
INSERT INTO sensor_calibrations (sensor_id, parameter_id, slope, intercept, valid_from)
     SELECT n.id, p.id, 2, 1, '2024-01-01T00:00:00Z' FROM sensors n, parameters p
      WHERE n.serial_number = 'SN-1' AND p.code = 'doc';
INSERT INTO sensor_deployments (sensor_id, site_id, parameter_id, deployed_from)
     SELECT n.id, s.id, p.id, '2024-01-01T00:00:00Z' FROM sensors n, sites s, parameters p
      WHERE n.serial_number = 'SN-1' AND s.name = 'Martigny' AND p.code = 'doc';
INSERT INTO standard_curves (sensor_id, name, slope, intercept, source_system, source_key)
     SELECT n.id, 'DOC 2024', 3, 0.5, 'cnet', 'curve-7' FROM sensors n
      WHERE n.serial_number = 'SN-1';
INSERT INTO data_streams (source_system, source_key, site_parameter_id, sensor_id)
     SELECT 'cnet', 'doc-1', sp.id, n.id FROM site_parameters sp, sensors n
      WHERE sp.name = 'Martigny DOC' AND n.serial_number = 'SN-1';
INSERT INTO collection_events (site_id, collected_at)
     SELECT id, '2024-06-01T09:00:00Z' FROM sites WHERE name = 'Martigny';
";

/// The state only the dump holds: the measurements, what was decided about them, the statistics
/// the replicates compute to, and what the project publishes.
const CURATED: &str = r"
UPDATE projects SET is_public = true, public_code = 'breathe',
       public_api_title = 'BREATHE', public_api_description = 'Alpine stream chemistry',
       public_api_version = '0.4.0', public_contact_email = 'river@example.org'
 WHERE name = 'Breathe';
UPDATE site_parameters SET is_public = true WHERE name = 'Martigny DOC';

INSERT INTO samples (id, site_id, parameter_id, collected_at, sd_estimator, sd_estimator_source)
     SELECT '11111111-1111-1111-1111-111111111111', s.id, p.id, '2024-06-01T09:00:00Z',
            'population', 'slot'
       FROM sites s, parameters p WHERE s.name = 'Martigny' AND p.code = 'doc';

INSERT INTO readings (stream_id, time, replicate_index, site_id, parameter_id, raw_value,
                      calibrated_value, sensor_id, calibration_id, deployment_id, logged,
                      measurement_type, sample_id, standard_curve_id, collection_event_id,
                      provenance, label, notes, created_by, unverified)
     SELECT d.id, '2024-06-01T09:00:00Z', r.i, s.id, p.id, 100 + r.i, 201 + 2 * r.i, n.id, c.id,
            dep.id, true, 'spot', '11111111-1111-1111-1111-111111111111', sc.id, e.id,
            jsonb_build_object('tool', 'doc', 'run', r.i), 'repeat ' || r.i, 'lab bench',
            'someone@example.org', r.i = 2
       FROM generate_series(0, 2) AS r(i), data_streams d, sites s, parameters p, sensors n,
            sensor_calibrations c, sensor_deployments dep, standard_curves sc,
            collection_events e
      WHERE d.source_key = 'doc-1' AND s.name = 'Martigny' AND p.code = 'doc'
        AND n.serial_number = 'SN-1' AND sc.source_key = 'curve-7';

INSERT INTO readings (stream_id, time, site_id, parameter_id, raw_value, calibrated_value,
                      sensor_id, calibration_id, deployment_id, measurement_type)
     SELECT d.id, '2024-06-02T09:00:00Z', s.id, p.id, 12.5, 26, n.id, c.id, dep.id, 'continuous'
       FROM data_streams d, sites s, parameters p, sensors n, sensor_calibrations c,
            sensor_deployments dep
      WHERE d.source_key = 'doc-1' AND s.name = 'Martigny' AND p.code = 'doc'
        AND n.serial_number = 'SN-1';

INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, old, new, actor, origin,
                               reason)
     SELECT d.id, '2024-06-01T09:00:00Z', 1, 'flag', '{}'::jsonb,
            jsonb_build_object('reason', 'bubble in the cuvette'), 'someone@example.org',
            'manual', 'bubble in the cuvette'
       FROM data_streams d WHERE d.source_key = 'doc-1';
INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, old, new, actor, origin)
     SELECT d.id, '2024-06-01T09:00:00Z', 2, 'withdraw', '{}'::jsonb,
            jsonb_build_object('reason', 'wrong vial'), 'someone@example.org', 'manual'
       FROM data_streams d WHERE d.source_key = 'doc-1';
INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, old, new, actor, origin)
     SELECT d.id, '2024-06-01T09:00:00Z', 0, 'curve', '{}'::jsonb,
            jsonb_build_object('standard_curve_id', sc.id), 'someone@example.org', 'manual'
       FROM data_streams d, standard_curves sc
      WHERE d.source_key = 'doc-1' AND sc.source_key = 'curve-7';
INSERT INTO reading_decisions (stream_id, time, replicate_index, kind, old, new, actor, origin)
     SELECT d.id, '2024-06-02T09:00:00Z', 0, 'verify', '{}'::jsonb, '{}'::jsonb,
            'someone@example.org', 'manual'
       FROM data_streams d WHERE d.source_key = 'doc-1';
";

/// Every column of a table, with the id references it carries replaced by the natural keys both
/// databases name the same rows by. One text per row, so what the two databases hold is compared
/// whole rather than column by column.
const ROWS: &[(&str, &str)] = &[
    (
        "readings",
        r"SELECT ((to_jsonb(r) - 'stream_id' - 'site_id' - 'parameter_id' - 'sensor_id'
                     - 'calibration_id' - 'deployment_id' - 'standard_curve_id'
                     - 'collection_event_id')
                    || jsonb_build_object(
                         'stream', d.source_system || '/' || d.source_key,
                         'site', lower(s.name), 'parameter', lower(p.code),
                         'sensor', n.serial_number, 'calibration', c.valid_from::text,
                         'deployment', dep.deployed_from::text,
                         'curve', sc.source_key, 'visit', e.collected_at::text))::text AS row
              FROM readings r
              JOIN data_streams d ON d.id = r.stream_id
              LEFT JOIN sites s ON s.id = r.site_id
              LEFT JOIN parameters p ON p.id = r.parameter_id
              LEFT JOIN sensors n ON n.id = r.sensor_id
              LEFT JOIN sensor_calibrations c ON c.id = r.calibration_id
              LEFT JOIN sensor_deployments dep ON dep.id = r.deployment_id
              LEFT JOIN standard_curves sc ON sc.id = r.standard_curve_id
              LEFT JOIN collection_events e ON e.id = r.collection_event_id",
    ),
    (
        "reading_decisions",
        r"SELECT ((to_jsonb(x) - 'stream_id' - 'at' - 'new')
                    || jsonb_build_object('stream', d.source_system || '/' || d.source_key,
                                          'new', x.new - 'standard_curve_id'))::text AS row
              FROM reading_decisions x
              JOIN data_streams d ON d.id = x.stream_id",
    ),
    (
        "samples",
        r"SELECT ((to_jsonb(x) - 'site_id' - 'parameter_id' - 'created_at' - 'updated_at')
                    || jsonb_build_object('site', lower(s.name), 'parameter', lower(p.code)))::text
                     AS row
              FROM samples x
              JOIN sites s ON s.id = x.site_id
              JOIN parameters p ON p.id = x.parameter_id",
    ),
    (
        "public settings",
        r"SELECT (lower(x.name) || x.is_public::text || coalesce(x.public_code, '')
                    || coalesce(x.public_api_title, '') || coalesce(x.public_api_description, '')
                    || coalesce(x.public_api_version, '')
                    || coalesce(x.public_contact_email, '')) AS row
              FROM projects x
            UNION ALL
            SELECT lower(s.name) || lower(p.code) || sp.is_public::text
              FROM site_parameters sp
              JOIN sites s ON s.id = sp.site_id
              JOIN parameters p ON p.id = sp.parameter_id",
    ),
];

/// The URL with its database name replaced, so the scratch databases are made on the same server.
fn url_for(base: &str, database: &str) -> String {
    let cut = base.rfind('/').expect("a database name in DATABASE_URL");
    let query = base[cut..]
        .find('?')
        .map(|q| &base[cut + q..])
        .unwrap_or("");
    format!("{}/{database}{query}", &base[..cut])
}

async fn rows(db: &DatabaseConnection, statement: &str) -> Vec<String> {
    let mut found: Vec<String> = db
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            statement.to_string(),
        ))
        .await
        .expect("the comparison query runs")
        .iter()
        .map(|row| row.try_get::<String>("", "row").expect("row"))
        .collect();
    found.sort();
    found
}

async fn build(base: &str, admin: &DatabaseConnection, name: &str) -> DatabaseConnection {
    admin
        .execute_unprepared(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
        .await
        .expect("drop");
    admin
        .execute_unprepared(&format!("CREATE DATABASE {name}"))
        .await
        .expect("create");
    let db = Database::connect(url_for(base, name))
        .await
        .expect("connect to the scratch database");
    migration::Migrator::up(&db, None)
        .await
        .expect("the baseline builds the database");
    db.execute_unprepared(METADATA).await.expect("the metadata");
    db
}

#[tokio::test]
#[serial]
async fn a_dump_and_a_rebuilt_database_hold_the_same_curated_state() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let pid = std::process::id();
    let (source_name, target_name) = (
        format!("river_cutover_src_{pid}"),
        format!("river_cutover_dst_{pid}"),
    );

    let admin = Database::connect(url_for(&base, "postgres"))
        .await
        .expect("connect to the server");
    let source = build(&base, &admin, &source_name).await;
    let target = build(&base, &admin, &target_name).await;
    source.execute_unprepared(CURATED).await.expect("the dump");

    let report = river_db::restore::restore(&source, &target)
        .await
        .expect("the cutover runs");

    let mut wrong = Vec::new();
    for (table, statement) in ROWS {
        let (before, after) = (
            rows(&source, statement).await,
            rows(&target, statement).await,
        );
        for lost in before.iter().filter(|row| !after.contains(row)) {
            wrong.push(format!("{table} lost {lost}"));
        }
        for gained in after.iter().filter(|row| !before.contains(row)) {
            wrong.push(format!("{table} gained {gained}"));
        }
    }
    let unmatched = report.unmatched.join(", ");
    let refused = report.refused.join(", ");
    let counts = format!(
        "{} readings, {} decisions, {} samples",
        report.rows("readings"),
        report.rows("reading_decisions"),
        report.rows("samples")
    );

    source.close().await.expect("close the source");
    target.close().await.expect("close the target");
    for name in [&source_name, &target_name] {
        admin
            .execute_unprepared(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .await
            .expect("drop");
    }

    assert!(
        wrong.is_empty(),
        "the cutover moved {counts} and lost state: {}",
        wrong.join("; ")
    );
    assert!(
        unmatched.is_empty(),
        "the rebuilt database matched every natural key, but the run reported: {unmatched}"
    );
    assert!(refused.is_empty(), "the cutover refused rows: {refused}");
    assert_eq!(report.rows("readings"), 4, "{counts}");
    assert_eq!(report.rows("reading_decisions"), 4, "{counts}");
}
