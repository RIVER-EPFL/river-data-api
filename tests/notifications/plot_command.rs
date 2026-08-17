//! Scenario: the bot answers `/plot` and the legacy `/1d /3d /7d /30d` with a rendered chart.
//!
//! Expected behaviour: a PNG when there is data, a specific sentence when there isn't, and the
//! caller's project scope confines site resolution exactly as it does for the text commands.
//!
//! The handler is driven directly, as the rest of this theme does: `route()` is private and there
//! is no Telegram HTTP mock. `send_photo` is branchless transport; the value is in the resolution
//! and the render, both reachable here.

use std::collections::HashSet;
use std::sync::Arc;

use river_db::common::authz::AccessScope;
use river_db::routes::private::notifications::{Reply, commands};
use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

use crate::common::fixtures::{GLOBAL_PARAM_DEPTH_ID, PROJECT_ID, SITE1_ID};

const PROJECT_B: &str = "00000000-0000-4000-c000-0000000000b1";
const SITE_B: &str = "00000000-0000-4000-c000-0000000000b2";
const MULTIWORD_SITE: &str = "00000000-0000-4000-c000-0000000000b3";

fn scope_a() -> AccessScope {
    AccessScope::Projects(Arc::new(HashSet::from([
        Uuid::parse_str(PROJECT_ID).unwrap(),
    ])))
}

/// The seed fixture's readings sit in January 2025, but a plot window runs back from `now`, so a
/// test needs its own recent rows. The stream is whichever one the fixture created: `readings`
/// has an FK to `data_streams`, and the fixture's ids are not the `STREAM*_ID` constants.
async fn seed_recent_readings(db: &DatabaseConnection, site_id: &str, hours: i64) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO readings (stream_id, site_id, parameter_id, time, raw_value) \
             SELECT (SELECT id FROM data_streams ORDER BY id LIMIT 1), \
                    '{site_id}', '{GLOBAL_PARAM_DEPTH_ID}', \
                    NOW() - (g * 15 || ' minutes')::interval, \
                    400 + 25 * sin(g / 7.0) \
             FROM generate_series(0, {}) g \
             ON CONFLICT DO NOTHING",
            hours * 4
        ),
    )
    .await;
}

async fn seed_project_b(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO projects (id, name, data_source) VALUES ('{PROJECT_B}', 'Project B', 'test')"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
             VALUES ('{SITE_B}', '{PROJECT_B}', 'HiddenStationB', 46.0, 7.0, 500.0)"
        ),
    )
    .await;
}

fn png_bytes(reply: &Reply) -> &[u8] {
    match reply {
        Reply::Photo { png, .. } => png,
        Reply::Text(t) => panic!("expected a chart, got text: {t}"),
    }
}

fn text(reply: &Reply) -> &str {
    match reply {
        Reply::Text(t) => t,
        Reply::Photo { caption, .. } => panic!("expected text, got a chart captioned: {caption}"),
    }
}

#[tokio::test]
#[serial]
async fn plot_returns_a_png_for_a_seeded_series() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 12).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "6h", "Upstream depth").await;
    let png = png_bytes(&reply);
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "reply must carry a real PNG");
    assert!(png.len() > 1_000, "suspiciously small chart: {}", png.len());

    match &reply {
        Reply::Photo { caption, .. } => {
            assert!(caption.contains("Upstream Station"), "caption: {caption}");
            assert!(caption.contains("Water Depth"), "caption: {caption}");
        }
        Reply::Text(_) => unreachable!(),
    }
}

#[tokio::test]
#[serial]
async fn legacy_window_commands_render() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 30).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    // `/1d` reads raw readings, so freshly inserted rows are visible immediately.
    let raw = commands::plot(&state, &AccessScope::Unrestricted, "1d", "Upstream depth").await;
    assert_eq!(
        &png_bytes(&raw)[..8],
        b"\x89PNG\r\n\x1a\n",
        "/1d must render from raw readings"
    );

    // `/3d` and up read the continuous aggregates, which a plain INSERT does not populate and
    // which this suite cannot refresh safely: `refresh_continuous_aggregate` serialises per view,
    // and 31 test binaries share one database. So assert the contract that holds either way: the
    // command answers, and an empty rollup says so specifically rather than erroring. The rollup
    // query itself is covered by `tier_for`'s unit tests and by rendering against a real database.
    for cmd in ["3d", "7d", "30d"] {
        match commands::plot(&state, &AccessScope::Unrestricted, cmd, "Upstream depth").await {
            Reply::Photo { png, .. } => assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n"),
            Reply::Text(t) => assert!(
                t.contains("No data"),
                "/{cmd} must render or report an empty window, got: {t}"
            ),
        }
    }
}

/// The security-critical case: a member must not reach a site outside their grants, and the
/// failure must be indistinguishable from a site that does not exist.
#[tokio::test]
#[serial]
async fn plot_cannot_reach_an_out_of_scope_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_project_b(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let by_name = commands::plot(&state, &scope_a(), "7d", "HiddenStationB depth").await;
    assert!(
        text(&by_name).contains("No site matches"),
        "member must not resolve project B by name: {}",
        text(&by_name)
    );

    let by_id = commands::plot(&state, &scope_a(), "7d", &format!("{SITE_B} depth")).await;
    assert!(
        text(&by_id).contains("No site matches"),
        "member must not resolve project B by id: {}",
        text(&by_id)
    );

    // An administrator resolves it, and so reports missing data rather than a missing site.
    let admin = commands::plot(
        &state,
        &AccessScope::Unrestricted,
        "7d",
        &format!("{SITE_B} depth"),
    )
    .await;
    assert!(
        !text(&admin).contains("No site matches"),
        "an admin resolves the site: {}",
        text(&admin)
    );
}

#[tokio::test]
#[serial]
async fn plot_reports_an_empty_window_specifically() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    // The fixture's readings are in January 2025, so a six-hour window is empty.
    let reply = commands::plot(&state, &AccessScope::Unrestricted, "6h", "Upstream depth").await;
    let msg = text(&reply);
    assert!(msg.contains("No data"), "{msg}");
    assert!(
        msg.contains("Latest reading"),
        "an empty window should name the most recent reading: {msg}"
    );
    assert!(
        !msg.contains("Something went wrong"),
        "an empty window is not an error: {msg}"
    );
}

#[tokio::test]
#[serial]
async fn plot_names_the_failing_side_on_a_bad_parameter() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "7d", "Upstream zzz").await;
    let msg = text(&reply);
    assert!(msg.contains("No parameter matches"), "{msg}");
    assert!(
        msg.contains("Upstream Station"),
        "the message should say which side resolved: {msg}"
    );
}

/// The exact bug the legacy R bot had: `station <- parts[2]` made a multi-word site unreachable,
/// contradicting its own README's `/mute "Les Dailles" turb 7d` example.
#[tokio::test]
#[serial]
async fn plot_handles_a_multi_word_site_name() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
             VALUES ('{MULTIWORD_SITE}', '{PROJECT_ID}', 'Les Dailles', 46.1, 7.2, 1400.0)"
        ),
    )
    .await;
    seed_recent_readings(&db, MULTIWORD_SITE, 12).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let bare = commands::plot(
        &state,
        &AccessScope::Unrestricted,
        "plot",
        "Les Dailles depth 6h",
    )
    .await;
    assert_eq!(
        &png_bytes(&bare)[..8],
        b"\x89PNG\r\n\x1a\n",
        "an unquoted multi-word site must resolve"
    );

    let comma = commands::plot(
        &state,
        &AccessScope::Unrestricted,
        "plot",
        "Les Dailles, depth, 6h",
    )
    .await;
    assert_eq!(&png_bytes(&comma)[..8], b"\x89PNG\r\n\x1a\n");

    let quoted = commands::plot(
        &state,
        &AccessScope::Unrestricted,
        "6h",
        "\"Les Dailles\" depth",
    )
    .await;
    assert_eq!(&png_bytes(&quoted)[..8], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
#[serial]
async fn plot_draws_thresholds_and_annotations_without_failing() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 12).await;
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds (parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max) \
             VALUES ('{GLOBAL_PARAM_DEPTH_ID}', '{SITE1_ID}', 390, 420, 380, 430)"
        ),
    )
    .await;
    // Starts before the window and is still open: it must clip into view, not vanish.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO annotations (site_id, parameter_id, start_time, end_time, text, category) \
             VALUES ('{SITE1_ID}', '{GLOBAL_PARAM_DEPTH_ID}', NOW() - INTERVAL '2 days', \
                     NOW() - INTERVAL '2 hours', 'probe fouled', 'field')"
        ),
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "6h", "Upstream depth").await;
    match &reply {
        Reply::Photo { png, caption } => {
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            assert!(
                caption.contains("1 note"),
                "an overlapping annotation should be reported: {caption}"
            );
        }
        Reply::Text(t) => panic!("expected a chart: {t}"),
    }
}

#[tokio::test]
#[serial]
async fn plot_rejects_a_nonsense_window() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(
        &state,
        &AccessScope::Unrestricted,
        "plot",
        "Upstream depth 9y",
    )
    .await;
    assert!(text(&reply).contains("isn't a window"), "{}", text(&reply));
}

#[tokio::test]
#[serial]
async fn plot_without_arguments_shows_usage() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "plot", "").await;
    assert!(text(&reply).contains("Usage:"), "{}", text(&reply));
}

/// `volt` has been the field team's shorthand for years and matches no parameter code or name.
#[tokio::test]
#[serial]
async fn legacy_parameter_aliases_resolve() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    crate::common::exec(
        &db,
        "INSERT INTO parameters (id, code, name, default_units, category) VALUES \
         ('00000000-0000-4000-b000-0000000000f1', 'BattV', 'Battery', 'V', 'device_health')",
    )
    .await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    // No readings, so this reports an empty window, which proves the parameter resolved, since an
    // unresolved one reports "No parameter matches" instead.
    let reply = commands::plot(&state, &AccessScope::Unrestricted, "7d", "Upstream volt").await;
    let msg = text(&reply);
    assert!(
        msg.contains("Battery"),
        "the `volt` alias must resolve to Battery: {msg}"
    );
    assert!(!msg.contains("No parameter matches"), "{msg}");
}

/// End-to-end against a real database, for eyeballing. Ignored by default and read-only: it never
/// truncates, so it can point at the dev database rather than the test one.
///
/// `DEV_DATABASE_URL=postgresql://... RIVER_PLOT_DUMP=/tmp/x.png \
///   cargo test --test notifications renders_from_a_real_database -- --ignored --nocapture`
#[tokio::test]
#[ignore = "manual: needs DEV_DATABASE_URL"]
async fn renders_from_a_real_database() {
    let url = std::env::var("DEV_DATABASE_URL").expect("DEV_DATABASE_URL");
    let db = sea_orm::Database::connect(&url).await.expect("connect");
    let (_app, state) = crate::common::build_test_app_with_state(db);

    let args = std::env::var("RIVER_PLOT_ARGS").unwrap_or_else(|_| "Saxon depth".to_string());
    let cmd = std::env::var("RIVER_PLOT_CMD").unwrap_or_else(|_| "6h".to_string());
    let reply = commands::plot(&state, &AccessScope::Unrestricted, &cmd, &args).await;

    match reply {
        Reply::Photo { png, caption } => {
            let path =
                std::env::var("RIVER_PLOT_DUMP").unwrap_or_else(|_| "/tmp/real.png".to_string());
            std::fs::write(&path, &png).expect("write");
            eprintln!("wrote {path} ({} bytes) caption: {caption}", png.len());
        }
        Reply::Text(t) => panic!("expected a chart, got: {t}"),
    }
}
