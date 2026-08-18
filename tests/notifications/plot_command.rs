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
use river_db::routes::private::notifications::keyboard::{self, Action, Button};
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
        Uuid::parse_str(PROJECT_ID).unwrap()
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
        other => panic!("expected a chart, got text: {}", other.text()),
    }
}

fn text(reply: &Reply) -> &str {
    match reply {
        Reply::Photo { caption, .. } => panic!("expected text, got a chart captioned: {caption}"),
        other => other.text(),
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
    assert_eq!(
        &png[..8],
        b"\x89PNG\r\n\x1a\n",
        "reply must carry a real PNG"
    );
    assert!(png.len() > 1_000, "suspiciously small chart: {}", png.len());

    match &reply {
        Reply::Photo { caption, .. } => {
            assert!(caption.contains("Upstream Station"), "caption: {caption}");
            assert!(caption.contains("Water Depth"), "caption: {caption}");
        }
        _ => unreachable!(),
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
            other => assert!(
                other.text().contains("No data"),
                "/{cmd} must render or report an empty window, got: {}",
                other.text()
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
        Reply::Photo { png, caption, .. } => {
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            assert!(
                caption.contains("1 note"),
                "an overlapping annotation should be reported: {caption}"
            );
        }
        other => panic!("expected a chart: {}", other.text()),
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
        Reply::Photo { png, caption, .. } => {
            let path =
                std::env::var("RIVER_PLOT_DUMP").unwrap_or_else(|_| "/tmp/real.png".to_string());
            std::fs::write(&path, &png).expect("write");
            eprintln!("wrote {path} ({} bytes) caption: {caption}", png.len());
        }
        other => panic!("expected a chart, got: {}", other.text()),
    }
}

/// Scenario: someone types `/plot` with nothing else, or names something that doesn't resolve.
///
/// Expected behaviour: the reply carries the choices rather than only naming the failure.
#[tokio::test]
#[serial]
async fn plot_without_arguments_offers_the_sites() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "plot", "").await;
    let buttons = flatten(&reply);
    assert!(
        buttons.iter().any(|b| b.text.contains("Upstream")),
        "the site picker must list the seeded site: {:?}",
        button_labels(&reply)
    );
}

#[tokio::test]
#[serial]
async fn an_unknown_site_offers_the_real_ones() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "plot", "Atlantis depth").await;
    assert!(text(&reply).contains("No site matches"), "{}", text(&reply));
    assert!(
        button_labels(&reply).iter().any(|l| l.contains("Upstream")),
        "the reply must offer the sites that do exist: {:?}",
        button_labels(&reply)
    );
}

#[tokio::test]
#[serial]
async fn an_unknown_parameter_offers_the_ones_the_site_has() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(
        &state,
        &AccessScope::Unrestricted,
        "plot",
        "Upstream nonsense",
    )
    .await;
    let labels = button_labels(&reply);
    assert!(
        labels.iter().any(|l| l.contains("Depth")),
        "a bad parameter must list the site's own parameters: {labels:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_site_on_its_own_draws_every_parameter() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 12).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "plot", "Upstream").await;
    assert_eq!(&png_bytes(&reply)[..8], b"\x89PNG\r\n\x1a\n");
    assert!(
        reply.text().contains("parameter"),
        "the caption should say how many panels it drew: {}",
        reply.text()
    );
    assert!(
        button_labels(&reply).iter().any(|l| l.contains("Depth")),
        "an overview must offer its parameters: {:?}",
        button_labels(&reply)
    );
}

#[tokio::test]
#[serial]
async fn a_chart_carries_a_window_switcher() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 12).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let reply = commands::plot(&state, &AccessScope::Unrestricted, "6h", "Upstream depth").await;
    let labels = button_labels(&reply);
    for window in keyboard::WINDOW_CHOICES {
        assert!(
            labels.iter().any(|l| l.contains(window)),
            "{window} must be one tap away: {labels:?}"
        );
    }
    assert!(
        labels.iter().any(|l| l == "• 6h"),
        "the window in view is marked: {labels:?}"
    );
}

/// A button is a shortcut, never an authority: its payload is re-resolved against the tapper's
/// scope, so one captured from an administrator's chat buys a member nothing.
#[tokio::test]
#[serial]
async fn a_button_cannot_reach_an_out_of_scope_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_project_b(&db).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let hidden = keyboard::short(Uuid::parse_str(SITE_B).unwrap());
    for action in [
        Action::Overview(hidden.clone()),
        Action::Parameters(hidden.clone()),
        Action::View {
            site: hidden,
            parameter: keyboard::short(Uuid::parse_str(GLOBAL_PARAM_DEPTH_ID).unwrap()),
            window: "6h".to_string(),
        },
    ] {
        let reply = commands::callback(&state, &scope_a(), "sub-test", action.clone()).await;
        assert!(
            text(&reply).contains("out of date"),
            "{action:?} must not resolve for a member: {}",
            text(&reply)
        );
    }

    let admin = commands::callback(
        &state,
        &AccessScope::Unrestricted,
        "sub-test",
        Action::Parameters(keyboard::short(Uuid::parse_str(SITE_B).unwrap())),
    )
    .await;
    assert!(
        !text(&admin).contains("out of date"),
        "an administrator resolves the same button: {}",
        text(&admin)
    );
}

#[tokio::test]
#[serial]
async fn a_button_round_trips_to_a_chart() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 12).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let sites = commands::plot(&state, &AccessScope::Unrestricted, "plot", "").await;
    let tapped = flatten(&sites)
        .into_iter()
        .find(|b| b.text.contains("Upstream"))
        .expect("a site button");
    let action = Action::parse(&tapped.data).expect("its payload parses");
    let overview = commands::callback(&state, &AccessScope::Unrestricted, "sub-test", action).await;

    let parameter = flatten(&overview)
        .into_iter()
        .find(|b| b.text.contains("Depth"))
        .expect("a parameter button");
    let chart = commands::callback(
        &state,
        &AccessScope::Unrestricted,
        "sub-test",
        Action::parse(&parameter.data).expect("its payload parses"),
    )
    .await;
    assert_eq!(&png_bytes(&chart)[..8], b"\x89PNG\r\n\x1a\n");
}

fn flatten(reply: &Reply) -> Vec<Button> {
    reply
        .keyboard()
        .map(|k| k.iter().flatten().cloned().collect())
        .unwrap_or_default()
}

fn button_labels(reply: &Reply) -> Vec<String> {
    flatten(reply).into_iter().map(|b| b.text).collect()
}

/// Scenario: an alarm alert carries a chart of the slot that breached.
///
/// Expected behaviour: the same readings render differently once a threshold makes them a breach,
/// which is what proves the severity classification reaches the renderer rather than stopping at
/// the limit lines.
#[tokio::test]
#[serial]
async fn an_alarm_chart_marks_the_breaching_stretch() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_recent_readings(&db, SITE1_ID, 6).await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let site = Uuid::parse_str(SITE1_ID).unwrap();
    let parameter = Uuid::parse_str(GLOBAL_PARAM_DEPTH_ID).unwrap();
    let window = chrono::Duration::hours(6);

    // Seeded depth sits around 400, so these bounds are nowhere near it.
    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds \
             (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max) \
             VALUES (gen_random_uuid(), '{GLOBAL_PARAM_DEPTH_ID}', '{SITE1_ID}', 0, 10000, -1, 20000)"
        ),
    )
    .await;
    let calm = commands::slot_plot_png(&state, site, parameter, window)
        .await
        .expect("a chart with no breach");

    crate::common::exec(
        &db,
        &format!(
            "UPDATE alarm_thresholds SET alarm_max = 100, warning_max = 50 \
             WHERE site_id = '{SITE1_ID}' AND parameter_id = '{GLOBAL_PARAM_DEPTH_ID}'"
        ),
    )
    .await;
    let breaching = commands::slot_plot_png(&state, site, parameter, window)
        .await
        .expect("a chart with a breach");

    assert_eq!(&breaching[..8], b"\x89PNG\r\n\x1a\n");
    assert_ne!(
        calm, breaching,
        "the same readings must draw differently once they breach"
    );
}
