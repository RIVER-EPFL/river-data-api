//! H4, bot read commands are confined to the caller's project scope. A member scoped to project A
//! sees only A's stations and cannot resolve a site in project B (it reads as "no match"), so the bot
//! never surfaces data the member couldn't see in the portal. Administrators (Unrestricted) see all.
//!
//! These drive the command handlers directly with a constructed `AccessScope`, so no Keycloak is
//! needed. Run: cargo test --test notifications -- --test-threads=1

use std::collections::HashSet;
use std::sync::Arc;

use river_db::common::authz::AccessScope;
use river_db::routes::private::notifications::commands;
use sea_orm::DatabaseConnection;
use serial_test::serial;
use uuid::Uuid;

const PROJECT_B: &str = "00000000-0000-4000-c000-0000000000b1";
const SITE_B: &str = "00000000-0000-4000-c000-0000000000b2";

async fn seed_project_b(db: &DatabaseConnection) {
    crate::common::exec(
        db,
        &format!("INSERT INTO projects (id, name, data_source) VALUES ('{PROJECT_B}', 'Project B', 'test')"),
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

fn scope_a() -> AccessScope {
    let project_a = Uuid::parse_str(crate::common::fixtures::PROJECT_ID).unwrap();
    AccessScope::Projects(Arc::new(HashSet::from([project_a])))
}

#[tokio::test]
#[serial]
async fn stations_hides_ungranted_projects() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_project_b(&db).await;

    let confined = commands::stations(&db, &scope_a()).await;
    assert!(
        !confined.contains("HiddenStationB"),
        "member must not see project B's site: {confined}"
    );

    let unrestricted = commands::stations(&db, &AccessScope::Unrestricted).await;
    assert!(
        unrestricted.contains("HiddenStationB"),
        "an admin sees every site: {unrestricted}"
    );
}

#[tokio::test]
#[serial]
async fn latest_cannot_resolve_out_of_scope_site() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    seed_project_b(&db).await;

    // By name and by id, an out-of-scope site is indistinguishable from a non-existent one.
    let by_name = commands::latest(&db, &scope_a(), "HiddenStationB").await;
    assert!(
        by_name.contains("No site matches"),
        "member cannot target project B by name: {by_name}"
    );

    let by_id = commands::latest(&db, &scope_a(), SITE_B).await;
    assert!(
        by_id.contains("No site matches"),
        "member cannot target project B by id: {by_id}"
    );

    // The admin can resolve it (no data seeded, so it reports no readings rather than "no match").
    let admin = commands::latest(&db, &AccessScope::Unrestricted, SITE_B).await;
    assert!(
        !admin.contains("No site matches"),
        "an admin resolves the site: {admin}"
    );
}
