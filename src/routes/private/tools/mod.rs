use axum::{Json, extract::{Path, State}};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Cached gas constants — loaded from DB once, then reused for all pCO2 calculations.
static GAS_CONSTANTS: OnceCell<river_data_core::toolbox::GasConstants> = OnceCell::const_new();

async fn get_constant(db: &DatabaseConnection, name: &str, default: f64) -> f64 {
    use crate::routes::private::constants;
    constants::Entity::find()
        .filter(constants::Column::Name.eq(name))
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.value)
        .unwrap_or(default)
}

#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub tool: String,
    pub results: serde_json::Value,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ToolParamInfo {
    pub name: &'static str,
    pub label: &'static str,
    pub required: bool,
}

#[derive(Debug, Deserialize)]
pub struct DocRequest {
    pub replicates: Vec<f64>,
    /// Optional standard curve (slope, intercept)
    pub std_curve: Option<StdCurve>,
}

#[derive(Debug, Deserialize)]
pub struct StdCurve {
    pub slope: f64,
    pub intercept: f64,
}

pub async fn calculate_doc(Json(payload): Json<DocRequest>) -> AppResult<Json<ToolResult>> {
    let curve = payload
        .std_curve
        .as_ref()
        .map(|c| (c.slope, c.intercept));
    let avg = river_data_core::toolbox::doc_average(&payload.replicates, curve);
    let sd = river_data_core::toolbox::doc_std_dev(&payload.replicates, curve);

    Ok(Json(ToolResult {
        tool: "doc".to_string(),
        results: serde_json::json!({
            "DOC_avg_ppb": avg,
            "DOC_sd_ppb": sd,
        }),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct TssAfdmRequest {
    pub wgt_dried_g: f64,
    pub wgt_prefilt_g: f64,
    pub wgt_ashed_g: Option<f64>,
    pub vol_filtered_ml: f64,
}

pub async fn calculate_tss_afdm(
    Json(payload): Json<TssAfdmRequest>,
) -> AppResult<Json<ToolResult>> {
    let tss = river_data_core::toolbox::tss_mg_l(
        payload.wgt_dried_g,
        payload.wgt_prefilt_g,
        payload.vol_filtered_ml,
    );

    let afdm = payload.wgt_ashed_g.map(|ashed| {
        river_data_core::toolbox::afdm_mg_l(payload.wgt_dried_g, ashed, payload.vol_filtered_ml)
    });

    Ok(Json(ToolResult {
        tool: "tss_afdm".to_string(),
        results: serde_json::json!({
            "TSS_dry_weight_mgL": tss,
            "AFDM_mgL": afdm,
        }),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct ChlorophyllRequest {
    pub method: ChlorophyllMethod,
    pub fluorescence_before: f64,
    pub fluorescence_after: Option<f64>,
    pub slope: f64,
    pub intercept: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChlorophyllMethod {
    Acid,
    NoAcid,
}

pub async fn calculate_chlorophyll(
    Json(payload): Json<ChlorophyllRequest>,
) -> AppResult<Json<ToolResult>> {
    let chla = match payload.method {
        ChlorophyllMethod::Acid => {
            let after = payload.fluorescence_after.ok_or_else(|| {
                AppError::BadRequest(
                    "fluorescence_after required for acid method".to_string(),
                )
            })?;
            river_data_core::toolbox::chla_acid(
                payload.fluorescence_before,
                after,
                payload.slope,
                payload.intercept,
            )
        }
        ChlorophyllMethod::NoAcid => river_data_core::toolbox::chla_no_acid(
            payload.fluorescence_before,
            payload.slope,
            payload.intercept,
        ),
    };

    let mut results = serde_json::Map::new();
    match payload.method {
        ChlorophyllMethod::Acid => {
            results.insert("Chla_acid_ugL_avg".into(), serde_json::json!(chla));
        }
        ChlorophyllMethod::NoAcid => {
            results.insert("Chla_noacid_ugL_avg".into(), serde_json::json!(chla));
        }
    }

    Ok(Json(ToolResult {
        tool: "chlorophyll".to_string(),
        results: serde_json::Value::Object(results),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct NutrientsRequest {
    /// Multi-species mode: each key is a species name, value is replicates
    pub species: Option<std::collections::HashMap<String, Vec<f64>>>,
    /// Legacy single-species mode
    pub replicates: Option<Vec<f64>>,
    pub nox: Option<f64>,
    pub no2: Option<f64>,
}

pub async fn calculate_nutrients(
    Json(payload): Json<NutrientsRequest>,
) -> AppResult<Json<ToolResult>> {
    let mut results = serde_json::Map::new();

    if let Some(species) = &payload.species {
        // Multi-species mode
        let multi = river_data_core::toolbox::multi_nutrient_replicates(species);
        for (name, nr) in &multi {
            let upper = name.to_uppercase();
            results.insert(format!("NUT_{upper}_avg"), serde_json::json!(nr.mean));
            results.insert(format!("NUT_{upper}_sd"), serde_json::json!(nr.std_dev));
        }
    } else if let Some(replicates) = &payload.replicates {
        // Legacy single-species mode
        let result = river_data_core::toolbox::nutrient_from_replicates(replicates);
        let no3 = match (payload.nox, payload.no2) {
            (Some(nox), Some(no2)) => Some(river_data_core::toolbox::nitrate_from_nox_no2(nox, no2)),
            _ => None,
        };
        results.insert("NUT_avg".into(), serde_json::json!(result.mean));
        results.insert("NUT_sd".into(), serde_json::json!(result.std_dev));
        results.insert("NUT_NO3_avg".into(), serde_json::json!(no3));
    }

    Ok(Json(ToolResult {
        tool: "nutrients".to_string(),
        results: serde_json::Value::Object(results),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct IonsRequest {
    pub cations: Vec<IonEntry>,
    pub anions: Vec<IonEntry>,
}

#[derive(Debug, Deserialize)]
pub struct IonEntry {
    pub name: String,
    pub concentration_mg_l: f64,
}

pub async fn calculate_ions(Json(payload): Json<IonsRequest>) -> AppResult<Json<ToolResult>> {
    let cations: Vec<(&str, f64)> = payload
        .cations
        .iter()
        .map(|e| (e.name.as_str(), e.concentration_mg_l))
        .collect();
    let anions: Vec<(&str, f64)> = payload
        .anions
        .iter()
        .map(|e| (e.name.as_str(), e.concentration_mg_l))
        .collect();

    let result = river_data_core::toolbox::charge_balance(&cations, &anions);

    Ok(Json(ToolResult {
        tool: "ions".to_string(),
        results: serde_json::json!({
            "sum_cations_meq": result.sum_cations_meq,
            "sum_anions_meq": result.sum_anions_meq,
            "balance_percent": result.balance_percent
        }),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct AlkalinityRequest {
    pub sample_weight_g: f64,
    pub acid_normality: f64,
    pub titrant_volume_ml: f64,
    pub initial_ph: Option<f64>,
}

pub async fn calculate_alkalinity(
    Json(payload): Json<AlkalinityRequest>,
) -> AppResult<Json<ToolResult>> {
    let result = river_data_core::toolbox::gran_titration(
        payload.sample_weight_g,
        payload.acid_normality,
        payload.titrant_volume_ml,
    );

    let mut results = serde_json::Map::new();
    results.insert("alkalinity_meq_l".into(), serde_json::json!(result.alkalinity_meq_l));
    results.insert("alkalinity_mg_l_caco3".into(), serde_json::json!(result.alkalinity_mg_l_caco3));

    if let Some(ph) = payload.initial_ph {
        results.insert("WTW_pH_1".into(), serde_json::json!(ph));
    }

    Ok(Json(ToolResult {
        tool: "alkalinity".to_string(),
        results: serde_json::Value::Object(results),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pco2Mode {
    #[default]
    Simple,
    FullPipeline,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pco2Variant {
    #[default]
    Simple,
    P1,
    P2,
}

#[derive(Debug, Deserialize)]
pub struct Pco2Request {
    // Simple mode fields (existing)
    pub co2_aq_umol: Option<f64>,
    pub water_temp_c: f64,
    pub pressure_hpa: Option<f64>,
    #[serde(default)]
    pub variant: Pco2Variant,

    // Mode selector
    #[serde(default)]
    pub mode: Pco2Mode,

    // Full pipeline fields (new)
    pub co2_ppm: Option<f64>,
    pub h2o_percent: Option<f64>,
    pub ch4_ppm: Option<f64>,
    pub d13co2_permil: Option<f64>,
    pub lab_temp_c: Option<f64>,
    pub lab_pressure_atm: Option<f64>,
    pub vol_sa_ml: Option<f64>,
    pub vol_water_ml: Option<f64>,

    // Replicate B (optional, for full pipeline mode)
    pub replicate_b: Option<Pco2ReplicateBInput>,
}

#[derive(Debug, Deserialize)]
pub struct Pco2ReplicateBInput {
    pub co2_ppm: f64,
    pub h2o_percent: f64,
    pub ch4_ppm: f64,
    pub d13co2_permil: Option<f64>,
}

async fn load_gas_constants(db: &DatabaseConnection) -> river_data_core::toolbox::GasConstants {
    let defaults = river_data_core::toolbox::GasConstants::default();
    river_data_core::toolbox::GasConstants {
        kh_co2: get_constant(db, "kh_co2", defaults.kh_co2).await,
        c_const: get_constant(db, "c_const", defaults.c_const).await,
        gas_const_r_atm: get_constant(db, "gas_const_r_atm", defaults.gas_const_r_atm).await,
        gas_const_r_mol: get_constant(db, "gas_const_r_mol", defaults.gas_const_r_mol).await,
        kh_ch4: get_constant(db, "kh_ch4", defaults.kh_ch4).await,
        ch4_temp_const: get_constant(db, "ch4_temp_const", defaults.ch4_temp_const).await,
        ch4_in_sa: get_constant(db, "ch4_in_sa", defaults.ch4_in_sa).await,
    }
}

/// Helper to require an `Option<f64>` field, rejecting NaN and Infinity.
fn require_field(val: Option<f64>, name: &str) -> AppResult<f64> {
    val.filter(|v| v.is_finite())
        .ok_or_else(|| AppError::BadRequest(format!("{name} is required and must be a finite number")))
}

pub async fn calculate_pco2(db: &DatabaseConnection, Json(payload): Json<Pco2Request>) -> AppResult<Json<ToolResult>> {
    let constants = GAS_CONSTANTS
        .get_or_init(|| load_gas_constants(db))
        .await;

    match payload.mode {
        Pco2Mode::Simple => {
            let co2_aq = payload.co2_aq_umol.ok_or_else(|| {
                AppError::BadRequest("co2_aq_umol is required for simple mode".to_string())
            })?;

            let pco2 = match payload.variant {
                Pco2Variant::Simple => {
                    river_data_core::toolbox::pco2_from_co2aq(co2_aq, payload.water_temp_c, constants)
                }
                Pco2Variant::P1 => {
                    let bp = payload.pressure_hpa.ok_or_else(|| {
                        AppError::BadRequest("pressure_hpa required for P1 variant".to_string())
                    })?;
                    river_data_core::toolbox::pco2_p1(co2_aq, payload.water_temp_c, bp, constants)
                }
                Pco2Variant::P2 => {
                    let bp = payload.pressure_hpa.ok_or_else(|| {
                        AppError::BadRequest("pressure_hpa required for P2 variant".to_string())
                    })?;
                    river_data_core::toolbox::pco2_p2(co2_aq, payload.water_temp_c, bp, constants)
                }
            };

            let key = match payload.variant {
                Pco2Variant::Simple => "pCO2_HS_uatm_avg",
                Pco2Variant::P1 => "pCO2_HS_P1_uatm_avg",
                Pco2Variant::P2 => "pCO2_HS_P2_uatm_avg",
            };
            let mut results = serde_json::Map::new();
            results.insert(key.into(), serde_json::json!(pco2));

            Ok(Json(ToolResult {
                tool: "pco2".to_string(),
                results: serde_json::Value::Object(results),
                inputs_used: vec![],
                inputs_ignored: vec![],
            }))
        }

        Pco2Mode::FullPipeline => {
            let co2_ppm = require_field(payload.co2_ppm, "co2_ppm")?;
            let h2o_percent = require_field(payload.h2o_percent, "h2o_percent")?;
            let ch4_ppm = require_field(payload.ch4_ppm, "ch4_ppm")?;
            let lab_temp_c = require_field(payload.lab_temp_c, "lab_temp_c")?;
            let lab_pressure_atm = require_field(payload.lab_pressure_atm, "lab_pressure_atm")?;
            let vol_sa_ml = require_field(payload.vol_sa_ml, "vol_sa_ml")?;
            let vol_water_ml = require_field(payload.vol_water_ml, "vol_water_ml")?;
            let field_pressure_hpa = payload.pressure_hpa.ok_or_else(|| {
                AppError::BadRequest("pressure_hpa is required for full_pipeline mode".to_string())
            })?;

            let input_a = river_data_core::toolbox::Pco2FullInput {
                co2_ppm,
                h2o_percent,
                ch4_ppm,
                d13co2_permil: payload.d13co2_permil,
                lab_temp_c,
                lab_pressure_atm,
                vol_sa_ml,
                vol_water_ml,
                water_temp_c: payload.water_temp_c,
                field_pressure_hpa,
            };

            let mut results = serde_json::Map::new();

            if let Some(rep_b) = &payload.replicate_b {
                let input_b = river_data_core::toolbox::Pco2FullInput {
                    co2_ppm: rep_b.co2_ppm,
                    h2o_percent: rep_b.h2o_percent,
                    ch4_ppm: rep_b.ch4_ppm,
                    d13co2_permil: rep_b.d13co2_permil,
                    // Shared lab/field conditions
                    lab_temp_c,
                    lab_pressure_atm,
                    vol_sa_ml,
                    vol_water_ml,
                    water_temp_c: payload.water_temp_c,
                    field_pressure_hpa,
                };

                let rep = river_data_core::toolbox::pco2_replicates(&input_a, &input_b, constants);

                results.insert("CO2_HS_Um_A".into(), serde_json::json!(rep.a.co2_hs_umol));
                results.insert("CO2_HS_Um_B".into(), serde_json::json!(rep.b.co2_hs_umol));
                results.insert("CO2_HS_Um_avg".into(), serde_json::json!(rep.co2_hs_umol_avg));
                results.insert("CO2_HS_Um_sd".into(), serde_json::json!(rep.co2_hs_umol_sd));
                results.insert("pCO2_HS_uatm_avg".into(), serde_json::json!(rep.pco2_uatm_avg));
                results.insert("pCO2_HS_uatm_sd".into(), serde_json::json!(rep.pco2_uatm_sd));
                results.insert("pCO2_HS_P1_uatm_avg".into(), serde_json::json!(rep.pco2_p1_uatm_avg));
                results.insert("pCO2_HS_P1_uatm_sd".into(), serde_json::json!(rep.pco2_p1_uatm_sd));
                results.insert("pCO2_HS_P2_uatm_avg".into(), serde_json::json!(rep.pco2_p2_uatm_avg));
                results.insert("pCO2_HS_P2_uatm_sd".into(), serde_json::json!(rep.pco2_p2_uatm_sd));
                results.insert("d13C_CO2_avg".into(), serde_json::json!(rep.d13co2_permil_avg));
                results.insert("d13C_CO2_sd".into(), serde_json::json!(rep.d13co2_permil_sd));
                results.insert("CH4_umol_L_avg".into(), serde_json::json!(rep.ch4_dissolved_umol_avg));
                results.insert("CH4_umol_L_sd".into(), serde_json::json!(rep.ch4_dissolved_umol_sd));
            } else {
                let r = river_data_core::toolbox::pco2_full_pipeline(&input_a, constants);

                results.insert("CO2_HS_Um_avg".into(), serde_json::json!(r.co2_hs_umol));
                results.insert("pCO2_HS_uatm_avg".into(), serde_json::json!(r.pco2_uatm));
                results.insert("pCO2_HS_P1_uatm_avg".into(), serde_json::json!(r.pco2_p1_uatm));
                results.insert("pCO2_HS_P2_uatm_avg".into(), serde_json::json!(r.pco2_p2_uatm));
                results.insert("d13C_CO2_avg".into(), serde_json::json!(r.d13co2_permil));
                results.insert("CH4_umol_L_avg".into(), serde_json::json!(r.ch4_dissolved_umol));
            }

            Ok(Json(ToolResult {
                tool: "pco2".to_string(),
                results: serde_json::Value::Object(results),
                inputs_used: vec![],
                inputs_ignored: vec![],
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DicReplicateBInput {
    pub acid_sample_weight_g: f64,
    pub acid_weight_g: f64,
    pub vol_overpressure_ml: f64,
    pub sa_added_ml: f64,
    pub co2_dry_ppm: f64,
    pub d13co2_permil: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct DicRequest {
    pub acid_sample_weight_g: f64,
    pub acid_weight_g: f64,
    pub vol_overpressure_ml: f64,
    pub sa_added_ml: f64,
    pub co2_dry_ppm: f64,
    pub d13co2_permil: Option<f64>,
    pub lab_temp_c: f64,
    pub h_co2_29815k: Option<f64>,
    pub gas_const_r_mol: Option<f64>,
    pub vial_volume: Option<f64>,
    pub h3po4_added: Option<f64>,
    pub replicate_b: Option<DicReplicateBInput>,
}

pub async fn calculate_dic(db: &DatabaseConnection, Json(payload): Json<DicRequest>) -> AppResult<Json<ToolResult>> {
    let constants = river_data_core::toolbox::DICConstants {
        h_co2_29815k: payload.h_co2_29815k.unwrap_or(get_constant(db, "h_co2_29815k", 0.034).await),
        gas_const_r_mol: payload.gas_const_r_mol.unwrap_or(get_constant(db, "gas_const_r_mol", 8.314).await),
        vial_volume: payload.vial_volume.unwrap_or(get_constant(db, "vial_volume", 12.0).await),
        h3po4_added: payload.h3po4_added.unwrap_or(get_constant(db, "h3po4_added", 0.1).await),
    };

    if let Some(ref rep_b) = payload.replicate_b {
        let rep = river_data_core::toolbox::dic_replicates(
            payload.acid_sample_weight_g, payload.acid_weight_g, payload.vol_overpressure_ml, payload.sa_added_ml, payload.co2_dry_ppm, payload.d13co2_permil,
            rep_b.acid_sample_weight_g, rep_b.acid_weight_g, rep_b.vol_overpressure_ml, rep_b.sa_added_ml, rep_b.co2_dry_ppm, rep_b.d13co2_permil,
            payload.lab_temp_c,
            &constants,
        );

        Ok(Json(ToolResult {
            tool: "dic".to_string(),
            results: serde_json::json!({
                "DIC_A": rep.dic_a,
                "DIC_B": rep.dic_b,
                "DIC_avg": rep.dic_avg,
                "DIC_std": rep.dic_std,
                "d13C_DIC_A": rep.d13c_a,
                "d13C_DIC_B": rep.d13c_b,
                "d13C_DIC_avg": rep.d13c_avg,
                "d13C_DIC_std": rep.d13c_std,
            }),
            inputs_used: vec![],
            inputs_ignored: vec![],
        }))
    } else {
        let dic = river_data_core::toolbox::dic_concentration(
            payload.acid_sample_weight_g,
            payload.acid_weight_g,
            payload.vol_overpressure_ml,
            payload.sa_added_ml,
            payload.co2_dry_ppm,
            payload.lab_temp_c,
            &constants,
        );

        let d13c = payload.d13co2_permil.map(|d13| {
            river_data_core::toolbox::d13c_dic(
                payload.acid_sample_weight_g,
                payload.acid_weight_g,
                payload.vol_overpressure_ml,
                d13,
                payload.lab_temp_c,
                &constants,
            )
        });

        Ok(Json(ToolResult {
            tool: "dic".to_string(),
            results: serde_json::json!({
                "DIC_avg": dic,
                "d13C_DIC_avg": d13c,
            }),
            inputs_used: vec![],
            inputs_ignored: vec![],
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct DomRequest {
    pub a254: Option<f64>,
    pub doc_avg_ppb: Option<f64>,
    pub abs_numerator: Option<f64>,
    pub abs_denominator: Option<f64>,
    pub peak_a: Option<f64>,
    pub peak_c: Option<f64>,
    pub peak_m: Option<f64>,
    pub peak_t: Option<f64>,
}

pub async fn calculate_dom(Json(payload): Json<DomRequest>) -> AppResult<Json<ToolResult>> {
    let mut results = serde_json::Map::new();

    let suva = match (payload.a254, payload.doc_avg_ppb) {
        (Some(a), Some(d)) => Some(river_data_core::toolbox::suva(a, d)),
        _ => None,
    };
    results.insert("SUVA".into(), serde_json::json!(suva));

    let ratio = match (payload.abs_numerator, payload.abs_denominator) {
        (Some(n), Some(d)) => Some(river_data_core::toolbox::absorbance_ratio(n, d)),
        _ => None,
    };
    results.insert("absorbance_ratio".into(), serde_json::json!(ratio));

    // Named fluorescence peak ratios
    if let (Some(pa), Some(pt)) = (payload.peak_a, payload.peak_t) {
        results.insert("A_T".into(), serde_json::json!(river_data_core::toolbox::absorbance_ratio(pa, pt)));
    }
    if let (Some(pc), Some(pa)) = (payload.peak_c, payload.peak_a) {
        results.insert("C_A".into(), serde_json::json!(river_data_core::toolbox::absorbance_ratio(pc, pa)));
    }
    if let (Some(pc), Some(pm)) = (payload.peak_c, payload.peak_m) {
        results.insert("C_M".into(), serde_json::json!(river_data_core::toolbox::absorbance_ratio(pc, pm)));
    }
    if let (Some(pc), Some(pt)) = (payload.peak_c, payload.peak_t) {
        results.insert("C_T".into(), serde_json::json!(river_data_core::toolbox::absorbance_ratio(pc, pt)));
    }

    Ok(Json(ToolResult {
        tool: "dom".to_string(),
        results: serde_json::Value::Object(results),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct FieldDataRequest {
    pub elevation_m: Option<f64>,
    pub temp_c: Option<f64>,
    pub raw_co2: Option<f64>,
    pub pressure_hpa: Option<f64>,
    pub std_curve: Option<StdCurve>,
    pub raw_co2_min: Option<f64>,
    pub raw_co2_avg: Option<f64>,
    pub raw_co2_max: Option<f64>,
    pub reach_depths: Option<Vec<f64>>,
}

pub async fn calculate_field_data(
    Json(payload): Json<FieldDataRequest>,
) -> AppResult<Json<ToolResult>> {
    let mut results = serde_json::Map::new();

    let bp = match (payload.elevation_m, payload.temp_c) {
        (Some(e), Some(t)) => Some(river_data_core::toolbox::barometric_pressure_from_altitude(e, t)),
        _ => None,
    };
    results.insert("Field_BP_altitude".into(), serde_json::json!(bp));

    let curve = payload.std_curve.as_ref().map(|c| (c.slope, c.intercept));

    // Multi-point CO2: min/avg/max
    if payload.raw_co2_min.is_some() || payload.raw_co2_avg.is_some() || payload.raw_co2_max.is_some() {
        if let (Some(p), Some(t)) = (payload.pressure_hpa, payload.temp_c) {
            if let Some(co2_min) = payload.raw_co2_min {
                results.insert("Vaisala_CO2_min_corr".into(), serde_json::json!(river_data_core::toolbox::co2_correction(co2_min, p, t, curve)));
            }
            if let Some(co2_avg) = payload.raw_co2_avg {
                results.insert("Vaisala_CO2_avg_corr".into(), serde_json::json!(river_data_core::toolbox::co2_correction(co2_avg, p, t, curve)));
            }
            if let Some(co2_max) = payload.raw_co2_max {
                results.insert("Vaisala_CO2_max_corr".into(), serde_json::json!(river_data_core::toolbox::co2_correction(co2_max, p, t, curve)));
            }
        }
    } else {
        // Legacy single CO2 mode
        let co2_corr = match (payload.raw_co2, payload.pressure_hpa, payload.temp_c) {
            (Some(co2), Some(p), Some(t)) => {
                Some(river_data_core::toolbox::co2_correction(co2, p, t, curve))
            }
            _ => None,
        };
        results.insert("Vaisala_CO2_avg_corr".into(), serde_json::json!(co2_corr));
    }

    // Reach depth stats
    if let Some(ref depths) = payload.reach_depths
        && !depths.is_empty()
    {
        let (avg, sd) = river_data_core::toolbox::reach_depth_stats(depths);
        results.insert("Reach_depth_avg_cm".into(), serde_json::json!(avg));
        results.insert("Reach_depth_sd_cm".into(), serde_json::json!(sd));
    }

    Ok(Json(ToolResult {
        tool: "field_data".to_string(),
        results: serde_json::Value::Object(results),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct Co2AirRequest {
    pub co2_wet: Option<f64>,
    pub ch4_wet: Option<f64>,
    pub h2o_percent: f64,
}

pub async fn calculate_co2_air(
    Json(payload): Json<Co2AirRequest>,
) -> AppResult<Json<ToolResult>> {
    let co2 = payload
        .co2_wet
        .map(|c| river_data_core::toolbox::co2_air::co2_dry(c, payload.h2o_percent));
    let ch4 = payload
        .ch4_wet
        .map(|c| river_data_core::toolbox::co2_air::ch4_dry_air(c, payload.h2o_percent));

    Ok(Json(ToolResult {
        tool: "co2_air".to_string(),
        results: serde_json::json!({
            "lab_co2air_co2_dry": co2,
            "lab_co2air_ch4_dry": ch4,
        }),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct IsotopesRequest {
    pub d_d: Option<f64>,
    pub d18o: Option<f64>,
    pub d17o: Option<f64>,
}

pub async fn calculate_isotopes(
    Json(payload): Json<IsotopesRequest>,
) -> AppResult<Json<ToolResult>> {
    let d_excess = match (payload.d_d, payload.d18o) {
        (Some(dd), Some(d18)) => Some(river_data_core::toolbox::deuterium_excess(dd, d18)),
        _ => None,
    };
    let o17_excess = match (payload.d17o, payload.d18o) {
        (Some(d17), Some(d18)) => Some(river_data_core::toolbox::o17_excess(d17, d18)),
        _ => None,
    };

    Ok(Json(ToolResult {
        tool: "isotopes".to_string(),
        results: serde_json::json!({
            "d_excess": d_excess,
            "o17_excess_permeg": o17_excess,
        }),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct BenthicRequest {
    pub diameters_cm: Vec<f64>,
    pub afdm_g_filter: Option<f64>,
    pub chla_ug_l: Option<f64>,
    pub volume_filtered_ml: f64,
    pub total_volume_ml: f64,
}

pub async fn calculate_benthic(
    Json(payload): Json<BenthicRequest>,
) -> AppResult<Json<ToolResult>> {
    let area = river_data_core::toolbox::rock_surface_area_m2(&payload.diameters_cm);

    let afdm_per_m2 = payload.afdm_g_filter.map(|afdm| {
        river_data_core::toolbox::per_m2(
            afdm,
            payload.total_volume_ml,
            payload.volume_filtered_ml,
            area,
        )
    });

    let chla_per_m2 = payload.chla_ug_l.map(|chla| {
        river_data_core::toolbox::per_m2(
            chla * 0.005,
            payload.total_volume_ml,
            payload.volume_filtered_ml,
            area,
        )
    });

    Ok(Json(ToolResult {
        tool: "benthic".to_string(),
        results: serde_json::json!({
            "rock_surface_area_m2": area,
            "benthic_AFDM_avg_gm2": afdm_per_m2,
            "chla_per_m2": chla_per_m2,
        }),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Deserialize)]
pub struct ChlaBenthicRequest {
    pub acid_slope: f64,
    pub acid_intercept: f64,
    pub noacid_slope: f64,
    pub noacid_intercept: f64,
    pub replicates: Vec<ChlaBenthicReplicateInput>,
}

#[derive(Debug, Deserialize)]
pub struct ChlaBenthicReplicateInput {
    pub fluor_before: f64,
    pub fluor_after: Option<f64>,
    pub vol_total_ml: f64,
    pub vol_after_ml: f64,
    pub diameters_cm: Vec<f64>,
    pub afdm_g_filter: Option<f64>,
}

pub async fn calculate_chla_benthic(
    Json(payload): Json<ChlaBenthicRequest>,
) -> AppResult<Json<ToolResult>> {
    let inputs: Vec<river_data_core::toolbox::ChlaReplicateInput> = payload
        .replicates
        .into_iter()
        .map(|r| river_data_core::toolbox::ChlaReplicateInput {
            fluor_before: r.fluor_before,
            fluor_after: r.fluor_after,
            vol_total_ml: r.vol_total_ml,
            vol_after_ml: r.vol_after_ml,
            diameters_cm: r.diameters_cm,
            afdm_g_filter: r.afdm_g_filter,
        })
        .collect();

    let result = river_data_core::toolbox::chla_benthic_replicates(
        &inputs,
        payload.acid_slope,
        payload.acid_intercept,
        payload.noacid_slope,
        payload.noacid_intercept,
    );

    let mut results = serde_json::Map::new();
    results.insert("Chla_acid_ugL_avg".into(), serde_json::json!(result.chla_acid_ug_l_avg));
    results.insert("Chla_acid_ugL_sd".into(), serde_json::json!(result.chla_acid_ug_l_sd));
    results.insert("Chla_noacid_ugL_avg".into(), serde_json::json!(result.chla_noacid_ug_l_avg));
    results.insert("Chla_noacid_ugL_sd".into(), serde_json::json!(result.chla_noacid_ug_l_sd));
    results.insert("Chla_acid_ugm2_avg".into(), serde_json::json!(result.chla_acid_ug_m2_avg));
    results.insert("Chla_acid_ugm2_sd".into(), serde_json::json!(result.chla_acid_ug_m2_sd));
    results.insert("Chla_noacid_ugm2_avg".into(), serde_json::json!(result.chla_noacid_ug_m2_avg));
    results.insert("Chla_noacid_ugm2_sd".into(), serde_json::json!(result.chla_noacid_ug_m2_sd));
    results.insert("benthic_AFDM_avg_gm2".into(), serde_json::json!(result.afdm_g_m2_avg));
    results.insert("benthic_AFDM_sd_gm2".into(), serde_json::json!(result.afdm_g_m2_sd));

    Ok(Json(ToolResult {
        tool: "chla_benthic".to_string(),
        results: serde_json::Value::Object(results),
        inputs_used: vec![],
        inputs_ignored: vec![],
    }))
}

#[derive(Debug, Serialize)]
pub struct ToolInfo {
    name: &'static str,
    description: &'static str,
    endpoint: &'static str,
    params: &'static [ToolParamInfo],
    match_keywords: &'static [&'static str],
}

const DOC_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "replicates", label: "Replicates (array)", required: true },
    ToolParamInfo { name: "std_curve", label: "Standard Curve (slope/intercept)", required: false },
];

const TSS_AFDM_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "wgt_dried_g", label: "Dried Weight (g)", required: true },
    ToolParamInfo { name: "wgt_prefilt_g", label: "Pre-filter Weight (g)", required: true },
    ToolParamInfo { name: "wgt_ashed_g", label: "Ashed Weight (g)", required: false },
    ToolParamInfo { name: "vol_filtered_ml", label: "Volume Filtered (mL)", required: true },
];

const CHLOROPHYLL_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "method", label: "Method (acid/no_acid)", required: true },
    ToolParamInfo { name: "fluorescence_before", label: "Fluorescence Before", required: true },
    ToolParamInfo { name: "fluorescence_after", label: "Fluorescence After", required: false },
    ToolParamInfo { name: "slope", label: "Slope", required: true },
    ToolParamInfo { name: "intercept", label: "Intercept", required: true },
];

const NUTRIENTS_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "species", label: "Species (multi-species map)", required: false },
    ToolParamInfo { name: "replicates", label: "Replicates (single-species)", required: false },
    ToolParamInfo { name: "nox", label: "NOx", required: false },
    ToolParamInfo { name: "no2", label: "NO2", required: false },
];

const IONS_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "cations", label: "Cations (array)", required: true },
    ToolParamInfo { name: "anions", label: "Anions (array)", required: true },
];

const ALKALINITY_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "sample_weight_g", label: "Sample Weight (g)", required: true },
    ToolParamInfo { name: "acid_normality", label: "Acid Normality", required: true },
    ToolParamInfo { name: "titrant_volume_ml", label: "Titrant Volume (mL)", required: true },
    ToolParamInfo { name: "initial_ph", label: "Initial pH", required: false },
];

const PCO2_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "mode", label: "Mode (simple/full_pipeline)", required: false },
    ToolParamInfo { name: "variant", label: "Variant (simple/p1/p2)", required: false },
    ToolParamInfo { name: "co2_aq_umol", label: "CO2aq (umol)", required: false },
    ToolParamInfo { name: "water_temp_c", label: "Water Temp (°C)", required: true },
    ToolParamInfo { name: "pressure_hpa", label: "Pressure (hPa)", required: false },
    ToolParamInfo { name: "co2_ppm", label: "CO2 (ppm)", required: false },
    ToolParamInfo { name: "h2o_percent", label: "H2O (%)", required: false },
    ToolParamInfo { name: "ch4_ppm", label: "CH4 (ppm)", required: false },
    ToolParamInfo { name: "d13co2_permil", label: "d13CO2 (permil)", required: false },
    ToolParamInfo { name: "lab_temp_c", label: "Lab Temp (°C)", required: false },
    ToolParamInfo { name: "lab_pressure_atm", label: "Lab Pressure (atm)", required: false },
    ToolParamInfo { name: "vol_sa_ml", label: "Vol SA (mL)", required: false },
    ToolParamInfo { name: "vol_water_ml", label: "Vol Water (mL)", required: false },
    ToolParamInfo { name: "replicate_b", label: "Replicate B", required: false },
];

const DIC_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "acid_sample_weight_g", label: "Acid Sample Weight (g)", required: true },
    ToolParamInfo { name: "acid_weight_g", label: "Acid Weight (g)", required: true },
    ToolParamInfo { name: "vol_overpressure_ml", label: "Vol Overpressure (mL)", required: true },
    ToolParamInfo { name: "sa_added_ml", label: "SA Added (mL)", required: true },
    ToolParamInfo { name: "co2_dry_ppm", label: "CO2 Dry (ppm)", required: true },
    ToolParamInfo { name: "d13co2_permil", label: "d13CO2 (permil)", required: false },
    ToolParamInfo { name: "lab_temp_c", label: "Lab Temp (°C)", required: true },
    ToolParamInfo { name: "h_co2_29815k", label: "H CO2 @298.15K", required: false },
    ToolParamInfo { name: "gas_const_r_mol", label: "Gas Const R (mol)", required: false },
    ToolParamInfo { name: "vial_volume", label: "Vial Volume (mL)", required: false },
    ToolParamInfo { name: "h3po4_added", label: "H3PO4 Added (mL)", required: false },
    ToolParamInfo { name: "replicate_b", label: "Replicate B", required: false },
];

const DOM_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "a254", label: "Absorbance @254nm", required: false },
    ToolParamInfo { name: "doc_avg_ppb", label: "DOC Avg (ppb)", required: false },
    ToolParamInfo { name: "abs_numerator", label: "Absorbance Numerator", required: false },
    ToolParamInfo { name: "abs_denominator", label: "Absorbance Denominator", required: false },
    ToolParamInfo { name: "peak_a", label: "Peak A", required: false },
    ToolParamInfo { name: "peak_c", label: "Peak C", required: false },
    ToolParamInfo { name: "peak_m", label: "Peak M", required: false },
    ToolParamInfo { name: "peak_t", label: "Peak T", required: false },
];

const FIELD_DATA_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "elevation_m", label: "Elevation (m)", required: false },
    ToolParamInfo { name: "temp_c", label: "Temperature (°C)", required: false },
    ToolParamInfo { name: "pressure_hpa", label: "Pressure (hPa)", required: false },
    ToolParamInfo { name: "raw_co2", label: "Raw CO2 (ppm)", required: false },
    ToolParamInfo { name: "raw_co2_min", label: "Raw CO2 Min (ppm)", required: false },
    ToolParamInfo { name: "raw_co2_avg", label: "Raw CO2 Avg (ppm)", required: false },
    ToolParamInfo { name: "raw_co2_max", label: "Raw CO2 Max (ppm)", required: false },
    ToolParamInfo { name: "std_curve", label: "Standard Curve (slope/intercept)", required: false },
    ToolParamInfo { name: "reach_depths", label: "Reach Depths (cm)", required: false },
];

const CO2_AIR_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "co2_wet", label: "CO2 Wet (ppm)", required: false },
    ToolParamInfo { name: "ch4_wet", label: "CH4 Wet (ppm)", required: false },
    ToolParamInfo { name: "h2o_percent", label: "H2O (%)", required: true },
];

const ISOTOPES_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "d_d", label: "dD (permil)", required: false },
    ToolParamInfo { name: "d18o", label: "d18O (permil)", required: false },
    ToolParamInfo { name: "d17o", label: "d17O (permil)", required: false },
];

const BENTHIC_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "diameters_cm", label: "Rock Diameters (cm)", required: true },
    ToolParamInfo { name: "afdm_g_filter", label: "AFDM per Filter (g)", required: false },
    ToolParamInfo { name: "chla_ug_l", label: "Chl-a (ug/L)", required: false },
    ToolParamInfo { name: "volume_filtered_ml", label: "Volume Filtered (mL)", required: true },
    ToolParamInfo { name: "total_volume_ml", label: "Total Volume (mL)", required: true },
];

const CHLA_BENTHIC_PARAMS: &[ToolParamInfo] = &[
    ToolParamInfo { name: "acid_slope", label: "Acid Slope", required: true },
    ToolParamInfo { name: "acid_intercept", label: "Acid Intercept", required: true },
    ToolParamInfo { name: "noacid_slope", label: "No-acid Slope", required: true },
    ToolParamInfo { name: "noacid_intercept", label: "No-acid Intercept", required: true },
    ToolParamInfo { name: "replicates", label: "Replicates (array)", required: true },
];

const TOOLS: &[ToolInfo] = &[
    ToolInfo { name: "doc", description: "Dissolved Organic Carbon (replicate avg/sd with optional standard curve)", endpoint: "/api/service/tools/doc/calculate", params: DOC_PARAMS, match_keywords: &["doc", "organic carbon", "dissolved organic"] },
    ToolInfo { name: "tss_afdm", description: "Total Suspended Solids & Ash-Free Dry Mass", endpoint: "/api/service/tools/tss_afdm/calculate", params: TSS_AFDM_PARAMS, match_keywords: &["tss", "afdm", "suspended solid", "dry mass"] },
    ToolInfo { name: "chlorophyll", description: "Chlorophyll-a (acid and no-acid methods)", endpoint: "/api/service/tools/chlorophyll/calculate", params: CHLOROPHYLL_PARAMS, match_keywords: &["chlorophyll", "chla", "chl"] },
    ToolInfo { name: "nutrients", description: "Nutrient replicates (PO4, NH4, NOx, NO2, TDP, TDN)", endpoint: "/api/service/tools/nutrients/calculate", params: NUTRIENTS_PARAMS, match_keywords: &["po4", "nh4", "nox", "no2", "no3", "tdp", "tdn", "nutrient", "phosph", "nitrate", "nitrite", "ammonium"] },
    ToolInfo { name: "ions", description: "IC ion charge balance verification", endpoint: "/api/service/tools/ions/calculate", params: IONS_PARAMS, match_keywords: &["ion", "anion", "cation", "charge balance", "ca2", "mg2", "na", "cl", "so4", "hco3"] },
    ToolInfo { name: "alkalinity", description: "Gran titration alkalinity (meq/L, mg/L CaCO3)", endpoint: "/api/service/tools/alkalinity/calculate", params: ALKALINITY_PARAMS, match_keywords: &["alkalinity", "alk", "titration", "caco3"] },
    ToolInfo { name: "pco2", description: "pCO2 from headspace CO2aq (simple, P1, P2 variants)", endpoint: "/api/service/tools/pco2/calculate", params: PCO2_PARAMS, match_keywords: &["co2", "pco2", "headspace", "carbon dioxide"] },
    ToolInfo { name: "dic", description: "DIC concentration and d13C-DIC from acid digestion", endpoint: "/api/service/tools/dic/calculate", params: DIC_PARAMS, match_keywords: &["dic", "d13c", "inorganic carbon"] },
    ToolInfo { name: "dom", description: "SUVA, absorbance ratios, spectral slopes", endpoint: "/api/service/tools/dom/calculate", params: DOM_PARAMS, match_keywords: &["dom", "suva", "a254", "absorbance", "dissolved organic"] },
    ToolInfo { name: "field_data", description: "Barometric pressure from altitude, CO2 correction", endpoint: "/api/service/tools/field_data/calculate", params: FIELD_DATA_PARAMS, match_keywords: &["field", "elevation", "altitude", "baro", "pressure", "co2", "depth", "reach"] },
    ToolInfo { name: "co2_air", description: "CO2/CH4 dry concentrations from wet measurements", endpoint: "/api/service/tools/co2_air/calculate", params: CO2_AIR_PARAMS, match_keywords: &["co2_air", "ch4", "methane", "co2"] },
    ToolInfo { name: "isotopes", description: "Deuterium excess, 17O excess", endpoint: "/api/service/tools/isotopes/calculate", params: ISOTOPES_PARAMS, match_keywords: &["isotop", "d18o", "deuterium", "d17o", "o18", "o17"] },
    ToolInfo { name: "benthic", description: "Rock surface area, per-m2 normalizations", endpoint: "/api/service/tools/benthic/calculate", params: BENTHIC_PARAMS, match_keywords: &["benthic", "rock", "surface area", "periphyton"] },
    ToolInfo { name: "chla_benthic", description: "Unified Chlorophyll-Benthic multi-replicate (acid + no-acid Chl-a, AFDM, per-m2)", endpoint: "/api/service/tools/chla_benthic/calculate", params: CHLA_BENTHIC_PARAMS, match_keywords: &["chla_benthic", "chlorophyll", "benthic", "afdm"] },
];

pub async fn list_tools() -> Json<Vec<&'static ToolInfo>> {
    Json(TOOLS.iter().collect())
}

/// Dynamic dispatcher for /`tools/{tool_name}/calculate`
pub async fn calculate_tool(
    State(state): State<AppState>,
    Path(tool_name): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<Json<ToolResult>> {
    let tool_info = TOOLS.iter().find(|t| t.name == tool_name.as_str())
        .ok_or_else(|| AppError::NotFound(format!("Unknown tool: {tool_name}")))?;

    let input_keys: Vec<String> = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();

    let known_names: Vec<&str> = tool_info.params.iter().map(|p| p.name).collect();
    let inputs_used: Vec<String> = input_keys.iter()
        .filter(|k| known_names.contains(&k.as_str()))
        .cloned().collect();
    let inputs_ignored: Vec<String> = input_keys.iter()
        .filter(|k| !known_names.contains(&k.as_str()))
        .cloned().collect();

    let mut result = match tool_name.as_str() {
        "doc" => calculate_doc(Json(parse_body(&body)?)).await,
        "tss_afdm" => calculate_tss_afdm(Json(parse_body(&body)?)).await,
        "chlorophyll" => calculate_chlorophyll(Json(parse_body(&body)?)).await,
        "nutrients" => calculate_nutrients(Json(parse_body(&body)?)).await,
        "ions" => calculate_ions(Json(parse_body(&body)?)).await,
        "alkalinity" => calculate_alkalinity(Json(parse_body(&body)?)).await,
        "pco2" => calculate_pco2(&state.db, Json(parse_body(&body)?)).await,
        "dic" => calculate_dic(&state.db, Json(parse_body(&body)?)).await,
        "dom" => calculate_dom(Json(parse_body(&body)?)).await,
        "field_data" => calculate_field_data(Json(parse_body(&body)?)).await,
        "co2_air" => calculate_co2_air(Json(parse_body(&body)?)).await,
        "isotopes" => calculate_isotopes(Json(parse_body(&body)?)).await,
        "benthic" => calculate_benthic(Json(parse_body(&body)?)).await,
        "chla_benthic" => calculate_chla_benthic(Json(parse_body(&body)?)).await,
        _ => Err(AppError::NotFound(format!("Unknown tool: {tool_name}"))),
    }?.0;

    result.inputs_used = inputs_used;
    result.inputs_ignored = inputs_ignored;

    Ok(Json(result))
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> AppResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request body: {e}")))
}
