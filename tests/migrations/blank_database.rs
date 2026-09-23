//! Scenario: an operator installs river-data and opens it before validating anything.
//!
//! Expected behaviour: the database the migrations build is blank. Every parameter, group,
//! calculation, formula, curve and instrument arrives through a plan or a form somebody at this
//! lab agreed to (Q134), so a migration that seeds one puts rows nobody reviewed in front of them
//! with nothing on the page saying where they came from.
//!
//! The twelve portal constants are Q102's exception: they are physical values the formula palette
//! offers by name, identical at every deployment, so the baseline seeds them and this suite holds
//! them to the portal's own numbers instead of to an empty table.
//!
//! Run: cargo test --test migrations blank_database -- --test-threads=1

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

use crate::common::scratch;

/// The tables an operator fills, in the order a reader would ask about them.
const MUST_BE_EMPTY: &[&str] = &[
    "parameters",
    "parameter_groups",
    "parameter_group_members",
    "tool_scripts",
    "tool_script_versions",
    "calculation_formulas",
    "calculation_shared_steps",
    "standard_curves",
    "sensors",
    "projects",
    "sites",
];

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    db.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT count(*) AS n FROM public.{table}"),
    ))
    .await
    .expect("count")
    .expect("a row")
    .try_get::<i64>("", "n")
    .expect("n")
}

#[tokio::test]
#[serial]
async fn a_migrated_database_holds_no_rows_nobody_asked_for() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    // Its own database: the shared one carries the reference rows the suites install, which is
    // exactly the state this test exists to distinguish from what the migrations build.
    let name = format!("river_blank_{}", std::process::id());

    let server = scratch::server(&base).await;
    let db = scratch::build(&base, &server, &name).await;

    let mut filled = Vec::new();
    for table in MUST_BE_EMPTY {
        let n = count(&db, table).await;
        if n > 0 {
            filled.push(format!("{table}: {n}"));
        }
    }
    let filled_report = filled.join(", ");

    db.close().await.expect("close the scratch connection");
    scratch::discard(&server, &name).await;

    assert!(
        filled.is_empty(),
        "a migration seeded rows an operator never validated: {filled_report}"
    );
}

/// The twelve, at the values `cnet-data-portal/app/utils/calculation_functions.R` reads.
const PORTAL_CONSTANTS: &[(&str, f64)] = &[
    ("gas_const_r_atm", 0.0820574),
    ("gas_const_r_mol", 8.31446),
    ("h_co2_29815k", 0.034733),
    ("h_ch4_29815k", 0.00213),
    ("c_const", 2400.0),
    ("ch4_in_sa", 2e-06),
    ("vol_sa", 0.03),
    ("vol_water", 0.03),
    ("vial_volume", 12.168),
    ("h3po4_added", 0.3),
    ("lab_press_avg_atm", 0.957237),
    ("lab_temp_avg_degC", 22.5),
];

#[tokio::test]
#[serial]
async fn a_migrated_database_holds_the_twelve_portal_constants() {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let name = format!("river_constants_{}", std::process::id());

    let server = scratch::server(&base).await;
    let db = scratch::build(&base, &server, &name).await;

    let mut wrong = Vec::new();
    for (constant, expected) in PORTAL_CONSTANTS {
        let found = db
            .query_one_raw(Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                format!("SELECT value FROM public.constants WHERE name = '{constant}'"),
            ))
            .await
            .expect("query the constant")
            .map(|row| row.try_get::<f64>("", "value").expect("value"));
        match found {
            Some(value) if (value - expected).abs() < f64::EPSILON * expected.abs().max(1.0) => {}
            Some(value) => wrong.push(format!("{constant}: {value} not {expected}")),
            None => wrong.push(format!("{constant}: missing")),
        }
    }
    let total = count(&db, "constants").await;
    let wrong_report = wrong.join(", ");

    db.close().await.expect("close the scratch connection");
    scratch::discard(&server, &name).await;

    assert!(
        wrong.is_empty(),
        "the baseline does not seed the portal constants: {wrong_report}"
    );
    assert_eq!(
        total,
        PORTAL_CONSTANTS.len() as i64,
        "the baseline seeds constants beyond the twelve the portal reads"
    );
}
