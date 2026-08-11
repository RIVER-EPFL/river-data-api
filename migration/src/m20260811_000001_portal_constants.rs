use sea_orm_migration::prelude::*;

/// Aligns the `constants` table with the CNET/METALP portal production values
/// and names. Drops the invented `kh_co2`/`kh_ch4`/`ch4_temp_const` entries
/// (the portal hardcodes those literals in its calculation functions).
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            "DELETE FROM constants WHERE name IN ('kh_co2', 'kh_ch4', 'ch4_temp_const')",
        )
        .await?;

        db.execute_unprepared(
            r#"
            INSERT INTO constants (name, value, units, description) VALUES
                ('gas_const_r_atm',   0.0820574, 'L*atm/(mol*K)', 'Ideal gas constant (R) in L*atm/(mol*K)'),
                ('h_co2_29815k',      0.034733,  'M/atm',         'Henry volatility constant for CO2 at 298.15K'),
                ('c_const',           2400,      'K',             'Constant C of van''t Hoff equation (K)'),
                ('vol_sa',            0.03,      'L',             'Volume of SA in syringe'),
                ('vol_water',         0.03,      'L',             'Volume of water in syringe'),
                ('lab_press_avg_atm', 0.957237,  'atm',           'Lab pressure (average of past years)'),
                ('lab_temp_avg_degC', 22.5,      'degC',          'Lab temp (average of past years)'),
                ('h_ch4_29815k',      0.00213,   'M/atm',         'Henry constant for CH4 at 298.15K'),
                ('ch4_in_sa',         0.000002,  '%',             'Fraction of CH4 in SA'),
                ('gas_const_r_mol',   8.31446,   'J/(K*mol)',     'Ideal gas constant (R) in J/(K*mol)'),
                ('vial_volume',       12.168,    'mL',            'Max DIC vial volume'),
                ('h3po4_added',       0.3,       'mL',            'Volume of added H3PO4')
            ON CONFLICT (name) DO UPDATE SET
                value = EXCLUDED.value,
                units = EXCLUDED.units,
                description = EXCLUDED.description
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
                'vol_sa', 'vol_water', 'lab_press_avg_atm', 'lab_temp_avg_degC', 'h_ch4_29815k'
            )
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            INSERT INTO constants (name, value, units, description) VALUES
                ('kh_co2',          0.034,   'mol/(L*atm)',   'Henry''s law constant for CO2 at 298.15K'),
                ('kh_ch4',          0.0014,  'mol/(L*atm)',   'Henry''s law constant for CH4 at 298.15K'),
                ('ch4_temp_const',  1750.0,  NULL,            'Temperature dependence constant for CH4 Henry''s law')
            ON CONFLICT (name) DO NOTHING
            "#,
        )
        .await?;

        db.execute_unprepared(
            r#"
            UPDATE constants SET value = v.value FROM (VALUES
                ('c_const',          2392.86),
                ('gas_const_r_atm',  0.08206),
                ('gas_const_r_mol',  8.314),
                ('ch4_in_sa',        1.9),
                ('h_co2_29815k',     0.034),
                ('vial_volume',      12.0),
                ('h3po4_added',      0.1)
            ) AS v(name, value) WHERE constants.name = v.name
            "#,
        )
        .await?;

        Ok(())
    }
}
