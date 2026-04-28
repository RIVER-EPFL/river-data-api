use sea_orm_migration::prelude::*;

/// Seeds the `constants` table with physical values used by the toolbox.
///
/// Values mirror `river_data_toolbox::GasConstants::default()` and the hardcoded
/// fallbacks in `routes/service/tools.rs::get_constant()` default arms, so tool
/// calculations produce identical results whether constants are DB-backed or
/// falling back to code defaults.
///
/// Idempotent: `ON CONFLICT (name) DO NOTHING` preserves any operator overrides.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            INSERT INTO constants (name, value, units, description) VALUES
                ('kh_co2',           0.034,    'mol/(L*atm)',   'Henry''s law constant for CO2 at 298.15K'),
                ('c_const',          2392.86,  NULL,            'Temperature dependence constant for CO2 Henry''s law'),
                ('gas_const_r_atm',  0.08206,  'L*atm/(mol*K)', 'Universal gas constant in atm units'),
                ('gas_const_r_mol',  8.314,    'J/(mol*K)',     'Universal gas constant in SI units'),
                ('kh_ch4',           0.0014,   'mol/(L*atm)',   'Henry''s law constant for CH4 at 298.15K'),
                ('ch4_temp_const',   1750.0,   NULL,            'Temperature dependence constant for CH4 Henry''s law'),
                ('ch4_in_sa',        1.9,      'ppm',           'CH4 concentration in standard atmosphere'),
                ('h_co2_29815k',     0.034,    'mol/(L*atm)',   'Henry''s law for CO2 at 298.15K (DIC tool)'),
                ('vial_volume',      12.0,     'mL',            'Standard DIC vial volume'),
                ('h3po4_added',      0.1,      'mL',            'Volume of H3PO4 added in DIC protocol')
            ON CONFLICT (name) DO NOTHING
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            DELETE FROM constants WHERE name IN (
                'kh_co2', 'c_const', 'gas_const_r_atm', 'gas_const_r_mol',
                'kh_ch4', 'ch4_temp_const', 'ch4_in_sa',
                'h_co2_29815k', 'vial_volume', 'h3po4_added'
            )
            "#,
        )
        .await?;

        Ok(())
    }
}
