//! The seeded `constants` table matches the CNET/METALP portal production dump: the calculators
//! read these by name with a silent Rust-default fallback, so a drifted or missing row changes
//! served numbers without an error anywhere.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;

/// (name, value, units) as the portal dump holds them. `ch4_in_sa` is a dimensionless fraction;
/// its portal '%' label was corrected because the mismatch is 10^6-sensitive in the dissolved-CH4
/// formula.
const PORTAL_CONSTANTS: &[(&str, f64, Option<&str>)] = &[
    ("gas_const_r_atm", 0.0820574, Some("L*atm/(mol*K)")),
    ("h_co2_29815k", 0.034733, Some("M/atm")),
    ("c_const", 2400.0, Some("K")),
    ("vol_sa", 0.03, Some("L")),
    ("vol_water", 0.03, Some("L")),
    ("lab_press_avg_atm", 0.957237, Some("atm")),
    ("lab_temp_avg_degC", 22.5, Some("degC")),
    ("h_ch4_29815k", 0.00213, Some("M/atm")),
    ("ch4_in_sa", 0.000002, None),
    ("gas_const_r_mol", 8.31446, Some("J/(K*mol)")),
    ("vial_volume", 12.168, Some("mL")),
    ("h3po4_added", 0.3, Some("mL")),
];

async fn constant_row(db: &DatabaseConnection, name: &str) -> Option<(f64, Option<String>)> {
    db.query_one(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT value, units FROM constants WHERE name = $1",
        [name.into()],
    ))
    .await
    .unwrap()
    .map(|row| {
        (
            row.try_get("", "value").unwrap(),
            row.try_get("", "units").unwrap(),
        )
    })
}

#[tokio::test]
#[serial]
async fn seeded_constants_match_the_portal_dump() {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    for (name, value, units) in PORTAL_CONSTANTS {
        let (stored_value, stored_units) = constant_row(&db, name)
            .await
            .unwrap_or_else(|| panic!("constant '{name}' is missing: get_constant would silently fall back to a Rust default"));
        assert!(
            (stored_value - value).abs() < 1e-12,
            "constant '{name}': stored {stored_value}, portal dump {value}"
        );
        assert_eq!(
            stored_units.as_deref(),
            *units,
            "constant '{name}' units drifted"
        );
    }

    for retired in ["kh_co2", "kh_ch4", "ch4_temp_const"] {
        assert!(
            constant_row(&db, retired).await.is_none(),
            "'{retired}' predates the portal migration and shadows the portal value if present"
        );
    }
}
