//! Scenario: a fresh database carries the CNET and METALP portals' categories as parameter
//! groups.
//!
//! Expected behaviour: thirteen groups in the registries' order, CNET's first and then the four
//! METALP alone has entry rows for, a replicate family as one member carrying its columns on a
//! spec, the DOC calculation attached to the DOC group, and a re-run that changes nothing.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use migration::m20260908_000007_seed_portal_parameter_groups::SEED;

async fn reseed(db: &DatabaseConnection) {
    db.execute_unprepared(SEED).await.expect("the seed applies");
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> String {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_string(),
    ))
    .await
    .expect("query")
    .expect("a row")
    .try_get_by_index::<String>(0)
    .expect("a text column")
}

async fn setup() -> DatabaseConnection {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    reseed(&db).await;
    db
}

#[tokio::test]
#[serial]
async fn portal_categories_seed_as_groups() {
    let db = setup().await;

    let codes = scalar(
        &db,
        "SELECT string_agg(code, ',' ORDER BY ordinal) FROM parameter_groups",
    )
    .await;
    assert_eq!(
        codes,
        "field_data,doc,dom,alkalinity,co2_air,pco2,dic,ions,nutrients,old_nutrients,tss,chl_a,\
         unused",
        "the groups are both registries' categories, CNET's order first"
    );

    let members = scalar(&db, "SELECT count(*)::text FROM parameter_group_members").await;
    assert_eq!(members, "192");

    // Every member resolves to a catalog parameter, which is what the FK would refuse; the
    // count is what a member insert silently skipping its lookup would move.
    let orphans = scalar(
        &db,
        "SELECT count(*)::text FROM parameter_group_members m \
         LEFT JOIN parameters p ON p.id = m.parameter_id WHERE p.id IS NULL",
    )
    .await;
    assert_eq!(orphans, "0");

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn a_replicate_family_is_one_member_carrying_its_spec() {
    let db = setup().await;

    let spec = scalar(
        &db,
        "SELECT m.replicates::text FROM parameter_group_members m \
         JOIN parameters p ON p.id = m.parameter_id WHERE p.code = 'DOC_ppb'",
    )
    .await;
    for column in [
        "DOC_rep_1",
        "DOC_rep_2",
        "DOC_rep_3",
        "DOC_avg_ppb",
        "DOC_sd_ppb",
    ] {
        assert!(spec.contains(column), "{column} is not on the spec: {spec}");
    }

    let role = scalar(
        &db,
        "SELECT m.role FROM parameter_group_members m JOIN parameters p ON p.id = m.parameter_id \
         WHERE p.code = 'DOC_ppb'",
    )
    .await;
    assert_eq!(role, "measured");

    // The replicate columns are readings of the member, not parameters of their own.
    let replicate_rows = scalar(
        &db,
        "SELECT count(*)::text FROM parameters WHERE code LIKE 'DOC\\_rep\\_%'",
    )
    .await;
    assert_eq!(replicate_rows, "0");

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn the_doc_calculation_lands_in_the_doc_group() {
    let db = setup().await;

    let group = scalar(
        &db,
        "SELECT g.code FROM tool_scripts s JOIN parameter_groups g ON g.id = s.parameter_group_id \
         WHERE s.name = 'doc'",
    )
    .await;
    assert_eq!(group, "doc");

    crate::common::cleanup_test_db(&db).await;
}

#[tokio::test]
#[serial]
async fn re_running_the_seed_changes_nothing() {
    let db = setup().await;

    let before = scalar(&db, "SELECT count(*)::text FROM change_audit WHERE subject LIKE 'parameter_group:%'").await;
    reseed(&db).await;
    let after = scalar(&db, "SELECT count(*)::text FROM change_audit WHERE subject LIKE 'parameter_group:%'").await;
    assert_eq!(before, after, "a second run appended history, so it wrote");

    crate::common::cleanup_test_db(&db).await;
}
