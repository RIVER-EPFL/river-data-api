//! The bot's `/thresholds <site>` answers with the same numbers `GET /api/alarms/thresholds`
//! resolves: the site row wins, then the global row, then the parameter defaults. These cover each
//! tier and the two states that are not a number, disabled and inactive.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use river_db::common::authz::AccessScope;
use river_db::routes::private::notifications::commands;
use sea_orm::DatabaseConnection;
use serial_test::serial;

const DEFAULT_TIER_PARAM: &str = "00000000-0000-4000-b000-0000000000d1";
const DEFAULT_TIER_SLOT: &str = "00000000-0000-4000-a000-0000000000d1";

async fn ask(db: &DatabaseConnection) -> String {
    commands::thresholds(db, &AccessScope::Unrestricted, crate::common::SITE1_ID).await
}

/// The line the reply devotes to one parameter.
fn line_for<'a>(reply: &'a str, parameter_name: &str) -> &'a str {
    reply
        .lines()
        .find(|l| l.starts_with(&format!("{parameter_name}:")))
        .unwrap_or_else(|| panic!("no line for {parameter_name} in reply:\n{reply}"))
}

/// A parameter carrying default bounds and no `alarm_thresholds` row of any kind, assigned to
/// site 1.
async fn seed_default_tier_slot(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category, \
                 default_warning_min, default_warning_max, default_alarm_min, default_alarm_max) \
             VALUES ('{DEFAULT_TIER_PARAM}', 'Rd049Default', 'Default Tier', 'mm', 'measurement', \
                 5, 20, 1, 30)"
        ),
    )
    .await;
    crate::common::exec(
        db,
        &format!(
            "INSERT INTO site_parameters (id, site_id, parameter_id, name, sensor_type, is_active) \
             VALUES ('{DEFAULT_TIER_SLOT}', '{site}', '{DEFAULT_TIER_PARAM}', 'Default Tier', \
                 'test', true)",
            site = crate::common::SITE1_ID,
        ),
    )
    .await;
}

#[tokio::test]
#[serial]
async fn a_site_reports_the_thresholds_that_actually_apply_to_it() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_default_tier_slot(&db).await;

    let reply = ask(&db).await;

    assert!(
        !reply.contains("(none configured)"),
        "the seeded site has effective thresholds: {reply}"
    );
    // Seeded thresholds are global rows (site_id IS NULL); they apply to this site.
    let turbidity = line_for(&reply, "Turbidity");
    assert!(
        turbidity.contains("100.0") && turbidity.contains("500.0"),
        "the global tier is reported for a site with no row of its own: {turbidity}"
    );
    let default_tier = line_for(&reply, "Default Tier");
    for bound in ["5.0", "20.0", "1.0", "30.0"] {
        assert!(
            default_tier.contains(bound),
            "the parameter-default tier is reported ({bound}): {default_tier}"
        );
    }
}

#[tokio::test]
#[serial]
async fn a_site_row_wins_over_the_global_row() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds \
                 (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max) \
             VALUES (gen_random_uuid(), '{param}', '{site}', 2, 8, 0.5, 9.5)",
            param = crate::common::GLOBAL_PARAM_TURB_ID,
            site = crate::common::SITE1_ID,
        ),
    )
    .await;

    let turbidity = ask(&db).await;
    let turbidity = line_for(&turbidity, "Turbidity").to_string();
    assert!(
        turbidity.contains("2.0") && turbidity.contains("9.5"),
        "the site row supplies the bounds: {turbidity}"
    );
    assert!(
        !turbidity.contains("500.0"),
        "the global row it overrides is not also reported: {turbidity}"
    );
}

#[tokio::test]
#[serial]
async fn an_all_null_site_row_reads_as_disabled_rather_than_falling_back() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "INSERT INTO alarm_thresholds \
                 (id, parameter_id, site_id, warning_min, warning_max, alarm_min, alarm_max) \
             VALUES (gen_random_uuid(), '{param}', '{site}', NULL, NULL, NULL, NULL)",
            param = crate::common::GLOBAL_PARAM_TURB_ID,
            site = crate::common::SITE1_ID,
        ),
    )
    .await;

    let reply = ask(&db).await;
    let turbidity = line_for(&reply, "Turbidity");
    assert!(
        !turbidity.contains("500.0") && !turbidity.contains("100.0"),
        "a disabled slot does not fall back to the global row: {turbidity}"
    );
}

#[tokio::test]
#[serial]
async fn an_inactive_slot_is_not_listed() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_active = false WHERE id = '{}'",
            crate::common::PARAM_S1_TURB_ID
        ),
    )
    .await;

    let reply = ask(&db).await;
    assert!(
        !reply.contains("Turbidity:"),
        "a deactivated slot raises no alarms, so it reports no thresholds: {reply}"
    );
    assert!(
        reply.contains("Water Temperature:"),
        "the site's other slots are unaffected: {reply}"
    );
}

#[tokio::test]
#[serial]
async fn a_site_with_no_active_slots_reports_none_configured() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    crate::common::exec(
        &db,
        &format!(
            "UPDATE site_parameters SET is_active = false WHERE site_id = '{}'",
            crate::common::SITE1_ID
        ),
    )
    .await;

    let reply = ask(&db).await;
    assert!(
        reply.contains("(none configured)"),
        "nothing resolves for the site, so the empty answer stands: {reply}"
    );
}
