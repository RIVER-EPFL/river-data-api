use sea_orm_migration::prelude::*;

/// Seeds the global parameter catalog with analytical/lab parameters used by tools.
///
/// These parameters use legacy CNET column names as the `name` (stable identifier)
/// and human-friendly labels as `display_name` (admin-editable).
/// All FK references use UUID, so display_name can be changed without breaking anything.
#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            INSERT INTO parameters (id, name, display_name, default_units, category, data_type, description)
            VALUES
                -- DOC Tool
                (gen_random_uuid(), 'DOC_avg_ppb',           'DOC Average',               'ppb',     'DOC',        'numeric', 'Dissolved Organic Carbon average of replicates'),
                (gen_random_uuid(), 'DOC_sd_ppb',            'DOC Std Dev',               'ppb',     'DOC',        'numeric', 'Dissolved Organic Carbon standard deviation'),

                -- TSS/AFDM Tool
                (gen_random_uuid(), 'TSS_dry_weight_mgL',    'TSS Dry Weight',            'mg/L',    'TSS',        'numeric', 'Total Suspended Solids from filter weights'),
                (gen_random_uuid(), 'AFDM_mgL',              'AFDM',                      'mg/L',    'TSS',        'numeric', 'Ash-Free Dry Mass'),

                -- Chlorophyll Tool
                (gen_random_uuid(), 'Chla_acid_ugL_avg',     'Chla Acid Average',         'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (acid method) average'),
                (gen_random_uuid(), 'Chla_acid_ugL_sd',      'Chla Acid Std Dev',         'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (acid method) standard deviation'),
                (gen_random_uuid(), 'Chla_noacid_ugL_avg',   'Chla Non-Acid Average',     'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (non-acid method) average'),
                (gen_random_uuid(), 'Chla_noacid_ugL_sd',    'Chla Non-Acid Std Dev',     'ug/L',    'Chla',       'numeric', 'Chlorophyll-a (non-acid method) standard deviation'),
                (gen_random_uuid(), 'Chla_acid_ugm2_avg',    'Chla Acid per m²',          'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (acid) per square meter average'),
                (gen_random_uuid(), 'Chla_acid_ugm2_sd',     'Chla Acid per m² SD',       'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (acid) per square meter std dev'),
                (gen_random_uuid(), 'Chla_noacid_ugm2_avg',  'Chla Non-Acid per m²',      'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (non-acid) per square meter average'),
                (gen_random_uuid(), 'Chla_noacid_ugm2_sd',   'Chla Non-Acid per m² SD',   'ug/m2',   'Chla',       'numeric', 'Chlorophyll-a (non-acid) per square meter std dev'),

                -- pCO2 Tool
                (gen_random_uuid(), 'CO2_HS_Um_avg',         'CO2 Headspace Average',     'umol/L',  'pCO2',       'numeric', 'CO2 headspace concentration average'),
                (gen_random_uuid(), 'CO2_HS_Um_sd',          'CO2 Headspace SD',          'umol/L',  'pCO2',       'numeric', 'CO2 headspace concentration std dev'),
                (gen_random_uuid(), 'pCO2_HS_uatm_avg',      'pCO2 Average',              'uatm',    'pCO2',       'numeric', 'Partial pressure CO2 average'),
                (gen_random_uuid(), 'pCO2_HS_uatm_sd',       'pCO2 SD',                   'uatm',    'pCO2',       'numeric', 'Partial pressure CO2 std dev'),
                (gen_random_uuid(), 'pCO2_HS_P1_uatm_avg',   'pCO2 P1 Average',           'uatm',    'pCO2',       'numeric', 'pCO2 method P1 average'),
                (gen_random_uuid(), 'pCO2_HS_P1_uatm_sd',    'pCO2 P1 SD',                'uatm',    'pCO2',       'numeric', 'pCO2 method P1 std dev'),
                (gen_random_uuid(), 'pCO2_HS_P2_uatm_avg',   'pCO2 P2 Average',           'uatm',    'pCO2',       'numeric', 'pCO2 method P2 average'),
                (gen_random_uuid(), 'pCO2_HS_P2_uatm_sd',    'pCO2 P2 SD',                'uatm',    'pCO2',       'numeric', 'pCO2 method P2 std dev'),
                (gen_random_uuid(), 'd13C_CO2_avg',          'δ13C-CO2 Average',          'permil',  'pCO2',       'numeric', 'δ13C of CO2 average'),
                (gen_random_uuid(), 'd13C_CO2_sd',           'δ13C-CO2 SD',               'permil',  'pCO2',       'numeric', 'δ13C of CO2 std dev'),
                (gen_random_uuid(), 'CH4_umol_L_avg',        'CH4 Dissolved Average',     'umol/L',  'pCO2',       'numeric', 'Dissolved CH4 average'),
                (gen_random_uuid(), 'CH4_umol_L_sd',         'CH4 Dissolved SD',          'umol/L',  'pCO2',       'numeric', 'Dissolved CH4 std dev'),

                -- DIC Tool
                (gen_random_uuid(), 'DIC_avg',               'DIC Average',               'umol/L',  'DIC',        'numeric', 'Dissolved Inorganic Carbon average'),
                (gen_random_uuid(), 'DIC_std',               'DIC Std Dev',               'umol/L',  'DIC',        'numeric', 'Dissolved Inorganic Carbon std dev'),
                (gen_random_uuid(), 'd13C_DIC_avg',          'δ13C-DIC Average',          'permil',  'DIC',        'numeric', 'δ13C of DIC average'),
                (gen_random_uuid(), 'd13C_DIC_std',          'δ13C-DIC Std Dev',          'permil',  'DIC',        'numeric', 'δ13C of DIC std dev'),

                -- DOM Tool
                (gen_random_uuid(), 'SUVA',                  'SUVA',                      'L/mg*m',  'DOM',        'numeric', 'Specific UV Absorbance at 254nm'),
                (gen_random_uuid(), 'A_T',                   'A/T Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio A/T'),
                (gen_random_uuid(), 'C_A',                   'C/A Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio C/A'),
                (gen_random_uuid(), 'C_M',                   'C/M Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio C/M'),
                (gen_random_uuid(), 'C_T',                   'C/T Ratio',                 'ratio',   'DOM',        'numeric', 'DOM fluorescence peak ratio C/T'),

                -- Nutrients Tool
                (gen_random_uuid(), 'NUT_P_avg',             'PO4 Average',               'ug/L',    'Nutrients',  'numeric', 'Phosphate average of replicates'),
                (gen_random_uuid(), 'NUT_P_sd',              'PO4 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Phosphate std dev'),
                (gen_random_uuid(), 'NUT_NH4_avg',           'NH4 Average',               'ug/L',    'Nutrients',  'numeric', 'Ammonium average of replicates'),
                (gen_random_uuid(), 'NUT_NH4_sd',            'NH4 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Ammonium std dev'),
                (gen_random_uuid(), 'NUT_NOx_avg',           'NOx Average',               'ug/L',    'Nutrients',  'numeric', 'Nitrate+Nitrite average of replicates'),
                (gen_random_uuid(), 'NUT_NOx_sd',            'NOx Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Nitrate+Nitrite std dev'),
                (gen_random_uuid(), 'NUT_NO2_avg',           'NO2 Average',               'ug/L',    'Nutrients',  'numeric', 'Nitrite average of replicates'),
                (gen_random_uuid(), 'NUT_NO2_sd',            'NO2 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Nitrite std dev'),
                (gen_random_uuid(), 'NUT_NO3_avg',           'NO3 Average',               'ug/L',    'Nutrients',  'numeric', 'Nitrate average (NOx - NO2)'),
                (gen_random_uuid(), 'NUT_NO3_sd',            'NO3 Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Nitrate std dev'),
                (gen_random_uuid(), 'NUT_TDP_avg',           'TDP Average',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Phosphorus average'),
                (gen_random_uuid(), 'NUT_TDP_sd',            'TDP Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Phosphorus std dev'),
                (gen_random_uuid(), 'NUT_TDN_avg',           'TDN Average',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Nitrogen average'),
                (gen_random_uuid(), 'NUT_TDN_sd',            'TDN Std Dev',               'ug/L',    'Nutrients',  'numeric', 'Total Dissolved Nitrogen std dev'),

                -- Field Data Tool
                (gen_random_uuid(), 'Field_BP_altitude',     'Barometric Pressure (alt)', 'hPa',     'Field data', 'numeric', 'Barometric pressure calculated from altitude'),
                (gen_random_uuid(), 'Vaisala_CO2_min_corr',  'Vaisala CO2 Min Corrected', 'ppm',     'Field data', 'numeric', 'Vaisala CO2 minimum corrected for T/P'),
                (gen_random_uuid(), 'Vaisala_CO2_avg_corr',  'Vaisala CO2 Avg Corrected', 'ppm',     'Field data', 'numeric', 'Vaisala CO2 average corrected for T/P'),
                (gen_random_uuid(), 'Vaisala_CO2_max_corr',  'Vaisala CO2 Max Corrected', 'ppm',     'Field data', 'numeric', 'Vaisala CO2 maximum corrected for T/P'),
                (gen_random_uuid(), 'Reach_depth_avg_cm',    'Reach Depth Average',       'cm',      'Field data', 'numeric', 'Average of reach depth replicates'),
                (gen_random_uuid(), 'Reach_depth_sd_cm',     'Reach Depth Std Dev',       'cm',      'Field data', 'numeric', 'Std dev of reach depth replicates'),

                -- CO2 Air Tool
                (gen_random_uuid(), 'lab_co2air_ch4_dry',    'CH4 Dry (Air)',             'ppm',     'CO2_air',    'numeric', 'CH4 dry concentration from wet measurement'),
                (gen_random_uuid(), 'lab_co2air_co2_dry',    'CO2 Dry (Air)',             'ppm',     'CO2_air',    'numeric', 'CO2 dry concentration from wet measurement'),

                -- Benthic Tool
                (gen_random_uuid(), 'benthic_AFDM_avg_gm2',  'Benthic AFDM Average',     'g/m2',    'Benthic',    'numeric', 'Benthic AFDM per square meter average'),
                (gen_random_uuid(), 'benthic_AFDM_sd_gm2',   'Benthic AFDM Std Dev',     'g/m2',    'Benthic',    'numeric', 'Benthic AFDM per square meter std dev'),

                -- Isotopes Tool (new, no legacy equivalent)
                (gen_random_uuid(), 'd_excess',              'Deuterium Excess',          'permil',  'Isotopes',   'numeric', 'Deuterium excess (dD - 8*d18O)'),
                (gen_random_uuid(), 'o17_excess_permeg',     '17O Excess',                'per_meg', 'Isotopes',   'numeric', '17-oxygen excess in per meg'),

                -- Alkalinity Tool (new, no legacy equivalent)
                (gen_random_uuid(), 'alkalinity_meq_l',      'Alkalinity (meq/L)',        'meq/L',   'Alkalinity', 'numeric', 'Gran titration alkalinity'),
                (gen_random_uuid(), 'alkalinity_mg_l_caco3', 'Alkalinity (CaCO3)',        'mg/L',    'Alkalinity', 'numeric', 'Alkalinity as mg/L CaCO3'),

                -- Ions Tool (new, no legacy equivalent)
                (gen_random_uuid(), 'sum_cations_meq',       'Cations Sum',               'meq/L',   'Ions',       'numeric', 'Sum of cation charge equivalents'),
                (gen_random_uuid(), 'sum_anions_meq',        'Anions Sum',                'meq/L',   'Ions',       'numeric', 'Sum of anion charge equivalents'),
                (gen_random_uuid(), 'balance_percent',       'Ion Balance',               '%',       'Ions',       'numeric', 'Ion charge balance percentage')
            ON CONFLICT (LOWER(name)) DO NOTHING;
            "#,
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"
            DELETE FROM parameters WHERE category IN (
                'DOC', 'TSS', 'Chla', 'pCO2', 'DIC', 'DOM', 'Nutrients',
                'Field data', 'CO2_air', 'Benthic', 'Isotopes', 'Alkalinity', 'Ions'
            );
            "#,
        )
        .await?;

        Ok(())
    }
}
