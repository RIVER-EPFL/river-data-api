//! Every command a phone can reach by tapping.
//!
//! The bot's charts were fully tappable while the rest of it demanded a site name typed accurately
//! enough to disambiguate. These cover the pickers that closed that gap, and the two gates a
//! tappable *write* opens: a button carries no authority, so the role and the chat type are
//! re-checked on the tap rather than trusted from the send.
//!
//! Run: cargo test --test notifications -- --test-threads=1

use std::collections::HashSet;
use std::sync::Arc;

use river_db::common::authz::AccessScope;
use river_db::routes::private::notifications::keyboard::{self, Action};
use river_db::routes::private::notifications::{Reply, commands};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

use crate::common::fixtures::{GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_TEMP_ID, PROJECT_ID, SITE1_ID};

const BY: &str = "keycloak:sub-test";

fn admin() -> AccessScope {
    AccessScope::Unrestricted
}

fn member() -> AccessScope {
    AccessScope::Projects(Arc::new(HashSet::from([
        Uuid::parse_str(PROJECT_ID).unwrap()
    ])))
}

fn buttons(reply: &Reply) -> Vec<keyboard::Button> {
    reply
        .keyboard()
        .map(|k| k.iter().flatten().cloned().collect())
        .unwrap_or_default()
}

fn labels(reply: &Reply) -> Vec<String> {
    buttons(reply).into_iter().map(|b| b.text).collect()
}

async fn mute_count(db: &DatabaseConnection) -> i64 {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT COUNT(*) AS c FROM notification_mutes".to_string(),
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "c")
    .unwrap()
}

async fn setup() -> DatabaseConnection {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    db
}

/// Sending a command bare is the entry to its menu, not a usage error.
#[tokio::test]
#[serial]
async fn a_bare_command_offers_the_sites_to_choose_from() {
    let db = setup().await;

    for reply in [
        commands::latest(&db, &admin(), "").await,
        commands::mute(&db, &admin(), "", BY).await,
    ] {
        assert!(
            matches!(reply, Reply::Menu { .. }),
            "expected a picker, got: {}",
            reply.text()
        );
        assert!(
            labels(&reply).iter().any(|t| t.contains("Upstream")),
            "the picker names the seeded sites: {:?}",
            labels(&reply)
        );
    }
}

/// A name that resolves to nothing answers with what does exist, rather than only saying no.
#[tokio::test]
#[serial]
async fn an_unresolvable_name_falls_back_to_the_picker() {
    let db = setup().await;

    let reply = commands::latest(&db, &admin(), "nowhere-at-all").await;
    assert!(reply.text().contains("No site matches"), "{}", reply.text());
    assert!(
        labels(&reply).iter().any(|t| t.contains("Upstream")),
        "the refusal carries the sites that do resolve: {:?}",
        labels(&reply)
    );
}

/// The site picker is built from the caller's own grants, exactly as a typed name is resolved.
#[tokio::test]
#[serial]
async fn a_picker_is_confined_to_the_callers_scope() {
    let db = setup().await;
    crate::common::exec(
        &db,
        "INSERT INTO projects (id, name, data_source) \
         VALUES ('00000000-0000-4000-c000-0000000000f1', 'Project F', 'test')",
    )
    .await;
    crate::common::exec(
        &db,
        "INSERT INTO sites (id, project_id, name, latitude, longitude, altitude_m) \
         VALUES ('00000000-0000-4000-c000-0000000000f2', '00000000-0000-4000-c000-0000000000f1', \
                 'OutOfScopeSite', 46.0, 7.0, 500.0)",
    )
    .await;

    let confined = labels(&commands::latest(&db, &member(), "").await);
    assert!(
        !confined.iter().any(|t| t.contains("OutOfScopeSite")),
        "a member's picker must not name another project's site: {confined:?}"
    );
    let unrestricted = labels(&commands::latest(&db, &admin(), "").await);
    assert!(
        unrestricted.iter().any(|t| t.contains("OutOfScopeSite")),
        "an administrator sees every site: {unrestricted:?}"
    );
}

/// The whole mute flow by tapping: site, then parameter, then how long. The last tap writes.
#[tokio::test]
#[serial]
async fn muting_completes_by_tapping_alone() {
    let db = setup().await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());

    let sites = commands::mute(&db, &admin(), "", BY).await;
    let site_button = buttons(&sites)
        .into_iter()
        .find(|b| b.text.contains("Upstream"))
        .expect("a site button");
    let parameters = commands::callback(
        &state,
        &admin(),
        "sub-test",
        Action::parse(&site_button.data).expect("its payload parses"),
    )
    .await;

    let parameter_button = buttons(&parameters)
        .into_iter()
        .find(|b| b.text.contains("Depth"))
        .expect("a parameter button");
    let durations = commands::callback(
        &state,
        &admin(),
        "sub-test",
        Action::parse(&parameter_button.data).expect("its payload parses"),
    )
    .await;
    // Choosing a parameter offers the lengths; it must not mute anything on its own, or a mistap
    // half way through the flow silences a slot for good.
    assert_eq!(
        mute_count(&db).await,
        0,
        "no mute before a duration is picked"
    );
    assert!(
        labels(&durations).iter().any(|t| t.contains("7 days")),
        "the durations are offered: {:?}",
        labels(&durations)
    );

    let seven_days = buttons(&durations)
        .into_iter()
        .find(|b| b.text.contains("7 days"))
        .expect("a 7-day button");
    let written = commands::callback(
        &state,
        &admin(),
        "sub-test",
        Action::parse(&seven_days.data).expect("its payload parses"),
    )
    .await;
    assert!(written.text().contains("Muted"), "{}", written.text());
    assert_eq!(mute_count(&db).await, 1, "exactly one mute written");

    // The confirmation carries its own undo, and it lifts exactly the mute just written.
    let undo = buttons(&written)
        .into_iter()
        .find(|b| b.text.contains("Unmute"))
        .expect("an Unmute button on the confirmation");
    let lifted = commands::callback(
        &state,
        &admin(),
        "sub-test",
        Action::parse(&undo.data).expect("its payload parses"),
    )
    .await;
    assert!(lifted.text().contains("Unmuted"), "{}", lifted.text());
    assert_eq!(mute_count(&db).await, 0, "the mute is gone");
}

/// `/muted` is the unmute menu, and lifting one mute leaves its neighbour alone.
#[tokio::test]
#[serial]
async fn unmuting_from_the_listing_lifts_only_that_mute() {
    let db = setup().await;
    let (_app, state) = crate::common::build_test_app_with_state(db.clone());
    for parameter in [GLOBAL_PARAM_DEPTH_ID, GLOBAL_PARAM_TEMP_ID] {
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO notification_mutes (site_id, parameter_id, expires_at) \
                 VALUES ('{SITE1_ID}', '{parameter}', NULL)"
            ),
        )
        .await;
    }

    let listing = commands::unmute(&db, &admin(), "").await;
    assert_eq!(
        buttons(&listing).len(),
        2,
        "one button per mute in force: {:?}",
        labels(&listing)
    );

    let depth = buttons(&listing)
        .into_iter()
        .find(|b| b.text.contains("Depth"))
        .expect("a button for the depth mute");
    commands::callback(
        &state,
        &admin(),
        "sub-test",
        Action::parse(&depth.data).expect("its payload parses"),
    )
    .await;

    assert_eq!(
        mute_count(&db).await,
        1,
        "the neighbouring mute survives its neighbour being lifted"
    );
    let remaining = commands::muted(&db).await;
    assert!(
        remaining.text().contains("Temperature"),
        "the surviving mute is the one not tapped: {}",
        remaining.text()
    );
}

/// The gates a button has to clear. Both are decided from the payload alone, before any query runs,
/// so they hold for a button that predates the tapper's role change.
#[tokio::test]
#[serial]
async fn a_mute_button_is_marked_as_an_administrator_write() {
    let site = keyboard::short(Uuid::parse_str(SITE1_ID).unwrap());
    let parameter = keyboard::short(Uuid::parse_str(GLOBAL_PARAM_DEPTH_ID).unwrap());

    let write = Action::MuteSet {
        site: site.clone(),
        parameter: parameter.clone(),
        days: 7,
    };
    assert!(write.is_write(), "refused outside a 1:1 chat");
    assert!(write.requires_admin(), "refused below administrator");

    // Reaching the write counts too: the pickers exist only to get there.
    assert!(Action::MuteSites.is_write());
    assert!(Action::MuteParams(site.clone()).is_write());
    assert!(Action::UnmuteSet { site, parameter }.is_write());

    // A chart is neither, or every member loses the feature the bot is mostly used for.
    assert!(!Action::Sites.is_write());
    assert!(!Action::Sites.requires_admin());
    assert!(!Action::LatestSites.requires_admin());
}

/// `/stations` was the name this listing shipped under, so it keeps answering.
#[tokio::test]
#[serial]
async fn the_old_stations_name_still_reaches_the_site_listing() {
    let db = setup().await;

    let renamed = commands::sites(&db, &admin()).await;
    assert!(
        renamed.text().starts_with("Sites:"),
        "the listing is headed by the word the rest of the system uses: {}",
        renamed.text()
    );

    // Both names are recorded under their own token rather than collapsing to `unknown`, which is
    // what keeps the audit trail readable across the rename.
    for name in ["sites", "stations"] {
        assert_eq!(
            river_db::routes::private::notifications::audit::command_name(name),
            name
        );
    }
}
