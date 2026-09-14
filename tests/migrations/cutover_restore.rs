//! Scenario: production is dumped, a new database is built from the baseline and rebuilt to the
//! point where its streams, sites, parameters and instruments exist, and the dump's curated state
//! is carried across (Q138, M189, M190).
//!
//! Expected behaviour: the readings, their curation ledger, the sample statistics, the review
//! queue, the annotations, the site notes, the alarm episodes, the device status and the public
//! API setups arrive intact under the rebuilt database's own ids. The assertion is a content hash
//! per table over every column, with the id references replaced by the natural keys the two
//! databases share, so a column the restore forgets fails it.
//!
//! Run: cargo test --test migrations cutover_restore -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::scratch;

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

INSERT INTO tool_runs (id, tool_name, tool_version, inputs, constants, curves, outputs,
                       created_by, context, source)
     SELECT '22222222-2222-2222-2222-222222222222', 'doc',
            jsonb_build_object('version_no', 1), jsonb_build_object('replicates', '[1, 2]'::jsonb),
            '{}'::jsonb, '{}'::jsonb, jsonb_build_object('doc_avg_ppb', 101),
            'someone@example.org',
            jsonb_build_object('site_id', s.id, 'collected_at', '2024-06-01T09:00:00Z'),
            'interactive'
       FROM sites s WHERE s.name = 'Martigny';

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
            jsonb_build_object('tool', 'doc', 'run_id',
                               '22222222-2222-2222-2222-222222222222'),
            'repeat ' || r.i, 'lab bench',
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

INSERT INTO replicate_audit_holds (id, stream_id, site_id, parameter_id, group_time, expected,
                                   computed, delta, status, acknowledged_by, acknowledged_at,
                                   manual_value, resolution, kind, tool)
     SELECT '22222222-2222-2222-2222-222222222222', d.id, s.id, p.id, '2024-06-01T09:00:00Z',
            jsonb_build_object('avg', 101), jsonb_build_object('avg', 101.5),
            jsonb_build_object('avg', 0.5), 'acknowledged', 'someone@example.org',
            '2024-06-03T10:00:00Z', 101.5, jsonb_build_object('choice', 'use_manual'),
            'replicate_stats', 'doc'
       FROM data_streams d, sites s, parameters p
      WHERE d.source_key = 'doc-1' AND s.name = 'Martigny' AND p.code = 'doc';

INSERT INTO annotations (site_id, parameter_id, start_time, end_time, text, category, created_by,
                         audit_hold_id, standard_curve_id)
     SELECT s.id, p.id, '2024-06-01T00:00:00Z', '2024-06-02T00:00:00Z', 'bubbles all morning',
            'quality', 'someone@example.org', '22222222-2222-2222-2222-222222222222', sc.id
       FROM sites s, parameters p, standard_curves sc
      WHERE s.name = 'Martigny' AND p.code = 'doc' AND sc.source_key = 'curve-7';

INSERT INTO notes (site_id, text, verified, created_by)
     SELECT id, 'gate padlock code changed', true, 'someone@example.org'
       FROM sites WHERE name = 'Martigny';

INSERT INTO meteoswiss_subscriptions (site_id, station_abbr, variable, parameter_id)
     SELECT s.id, 'MOB', 'prestas0', p.id
       FROM sites s, parameters p WHERE s.name = 'Martigny' AND p.code = 'doc';

INSERT INTO alarm_events (site_id, parameter_id, severity, max_severity, started_at,
                          value_at_start, last_seen_at, last_value, acknowledged_at,
                          acknowledged_by, measurement_type)
     SELECT s.id, p.id, 2, 3, '2024-06-01T09:00:00Z', 480, '2024-06-01T11:00:00Z', 512,
            '2024-06-01T12:00:00Z', 'someone@example.org', 'continuous'
       FROM sites s, parameters p WHERE s.name = 'Martigny' AND p.code = 'doc';

INSERT INTO status_events (stream_id, time, site_id, parameter_id, value, sensor_id)
     SELECT d.id, '2024-06-02T09:05:00Z', s.id, p.id, 'unreachable', n.id
       FROM data_streams d, sites s, parameters p, sensors n
      WHERE d.source_key = 'doc-1' AND s.name = 'Martigny' AND p.code = 'doc'
        AND n.serial_number = 'SN-1';
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
        "replicate_audit_holds",
        r"SELECT ((to_jsonb(x) - 'stream_id' - 'site_id' - 'parameter_id')
                    || jsonb_build_object('stream', d.source_system || '/' || d.source_key,
                                          'site', lower(s.name), 'parameter', lower(p.code)))::text
                     AS row
              FROM replicate_audit_holds x
              LEFT JOIN data_streams d ON d.id = x.stream_id
              LEFT JOIN sites s ON s.id = x.site_id
              LEFT JOIN parameters p ON p.id = x.parameter_id",
    ),
    (
        "annotations",
        r"SELECT ((to_jsonb(x) - 'site_id' - 'parameter_id' - 'standard_curve_id')
                    || jsonb_build_object('site', lower(s.name), 'parameter', lower(p.code),
                                          'curve', sc.source_key))::text AS row
              FROM annotations x
              JOIN sites s ON s.id = x.site_id
              JOIN parameters p ON p.id = x.parameter_id
              LEFT JOIN standard_curves sc ON sc.id = x.standard_curve_id",
    ),
    (
        "notes",
        r"SELECT ((to_jsonb(x) - 'site_id') || jsonb_build_object('site', lower(s.name)))::text
                     AS row
              FROM notes x
              JOIN sites s ON s.id = x.site_id",
    ),
    (
        "alarm_events",
        r"SELECT ((to_jsonb(x) - 'site_id' - 'parameter_id')
                    || jsonb_build_object('site', lower(s.name), 'parameter', lower(p.code)))::text
                     AS row
              FROM alarm_events x
              JOIN sites s ON s.id = x.site_id
              JOIN parameters p ON p.id = x.parameter_id",
    ),
    (
        "status_events",
        r"SELECT ((to_jsonb(x) - 'stream_id' - 'site_id' - 'parameter_id' - 'sensor_id')
                    || jsonb_build_object('stream', d.source_system || '/' || d.source_key,
                                          'site', lower(s.name), 'parameter', lower(p.code),
                                          'sensor', n.serial_number))::text AS row
              FROM status_events x
              JOIN data_streams d ON d.id = x.stream_id
              LEFT JOIN sites s ON s.id = x.site_id
              LEFT JOIN parameters p ON p.id = x.parameter_id
              LEFT JOIN sensors n ON n.id = x.sensor_id",
    ),
    (
        "tool_runs",
        r"SELECT ((to_jsonb(x) - 'created_at' - 'context')
                    || jsonb_build_object('context', (x.context - 'site_id')
                                                     || jsonb_build_object('site',
                                                                           lower(s.name))))::text
                     AS row
              FROM tool_runs x
              JOIN sites s ON s.id = (x.context ->> 'site_id')::uuid",
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

/// A migrated database carrying the reference rows both sides of the comparison need.
async fn build(base: &str, server: &DatabaseConnection, name: &str) -> DatabaseConnection {
    let db = scratch::build(base, server, name).await;
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

    let server = scratch::server(&base).await;
    let source = build(&base, &server, &source_name).await;
    let target = build(&base, &server, &target_name).await;
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
    // The provenance blob carries the run id verbatim, so the run it names has to be in the
    // rebuilt database under that id.
    let orphaned = rows(
        &target,
        r"SELECT count(*)::text AS row FROM readings r
           WHERE r.provenance ? 'run_id'
             AND NOT EXISTS (SELECT 1 FROM tool_runs t
                              WHERE t.id = (r.provenance ->> 'run_id')::uuid)",
    )
    .await
    .join("");

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
        scratch::discard(&server, name).await;
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
    assert_eq!(
        orphaned, "0",
        "carried readings name a tool run the rebuilt database does not hold"
    );
    assert_eq!(report.rows("tool_runs"), 1, "{counts}");
    assert_eq!(report.rows("readings"), 4, "{counts}");
    assert_eq!(report.rows("reading_decisions"), 4, "{counts}");
    for table in [
        "replicate_audit_holds",
        "annotations",
        "notes",
        "meteoswiss_subscriptions",
        "alarm_events",
        "status_events",
    ] {
        assert_eq!(report.rows(table), 1, "{table} was not carried");
    }
}
