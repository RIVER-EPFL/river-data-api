use axum::{Json, extract::{Path, State}};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::common::AppState;
use crate::error::{AppError, AppResult};

// ============================================================================
// Constant lookup helper
// ============================================================================

async fn get_constant(db: &DatabaseConnection, name: &str, default: f64) -> f64 {
    use crate::entity::constants;
    constants::Entity::find()
        .filter(constants::Column::Name.eq(name))
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|c| c.value)
        .unwrap_or(default)
}

// ============================================================================
// Common types
// ============================================================================

#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub tool: String,
    pub results: serde_json::Value,
}

// ============================================================================
// DOC Tool
// ============================================================================

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
    let avg = river_data_toolbox::doc_average(&payload.replicates, curve);
    let sd = river_data_toolbox::doc_std_dev(&payload.replicates, curve);

    Ok(Json(ToolResult {
        tool: "doc".to_string(),
        results: serde_json::json!({
            "doc_avg": avg,
            "doc_sd": sd,
        }),
    }))
}

// ============================================================================
// TSS / AFDM Tool
// ============================================================================

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
    let tss = river_data_toolbox::tss_mg_l(
        payload.wgt_dried_g,
        payload.wgt_prefilt_g,
        payload.vol_filtered_ml,
    );

    let (afdm, pct_organic) = match payload.wgt_ashed_g {
        Some(ashed) => {
            let a = river_data_toolbox::afdm_mg_l(
                payload.wgt_dried_g,
                ashed,
                payload.vol_filtered_ml,
            );
            let pct = river_data_toolbox::percent_organic(tss, a);
            (Some(a), Some(pct))
        }
        None => (None, None),
    };

    Ok(Json(ToolResult {
        tool: "tss_afdm".to_string(),
        results: serde_json::json!({
            "tss_mg_l": tss,
            "afdm_mg_l": afdm,
            "percent_organic": pct_organic,
        }),
    }))
}

// ============================================================================
// Chlorophyll Tool
// ============================================================================

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
            river_data_toolbox::chla_acid(
                payload.fluorescence_before,
                after,
                payload.slope,
                payload.intercept,
            )
        }
        ChlorophyllMethod::NoAcid => river_data_toolbox::chla_no_acid(
            payload.fluorescence_before,
            payload.slope,
            payload.intercept,
        ),
    };

    Ok(Json(ToolResult {
        tool: "chlorophyll".to_string(),
        results: serde_json::json!({
            "chla_ug_l": chla,
        }),
    }))
}

// ============================================================================
// Nutrients Tool
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct NutrientsRequest {
    pub replicates: Vec<f64>,
    pub nox: Option<f64>,
    pub no2: Option<f64>,
}

pub async fn calculate_nutrients(
    Json(payload): Json<NutrientsRequest>,
) -> AppResult<Json<ToolResult>> {
    let result = river_data_toolbox::nutrient_from_replicates(&payload.replicates);
    let no3 = match (payload.nox, payload.no2) {
        (Some(nox), Some(no2)) => Some(river_data_toolbox::nitrate_from_nox_no2(nox, no2)),
        _ => None,
    };

    Ok(Json(ToolResult {
        tool: "nutrients".to_string(),
        results: serde_json::json!({
            "mean": result.mean,
            "std_dev": result.std_dev,
            "no3": no3,
        }),
    }))
}

// ============================================================================
// Ions (Charge Balance) Tool
// ============================================================================

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

    let result = river_data_toolbox::charge_balance(&cations, &anions);

    Ok(Json(ToolResult {
        tool: "ions".to_string(),
        results: serde_json::json!({
            "sum_cations_meq": result.sum_cations_meq,
            "sum_anions_meq": result.sum_anions_meq,
            "balance_percent": result.balance_percent,
        }),
    }))
}

// ============================================================================
// Alkalinity Tool
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct AlkalinityRequest {
    pub sample_weight_g: f64,
    pub acid_normality: f64,
    pub titrant_volume_ml: f64,
}

pub async fn calculate_alkalinity(
    Json(payload): Json<AlkalinityRequest>,
) -> AppResult<Json<ToolResult>> {
    let result = river_data_toolbox::gran_titration(
        payload.sample_weight_g,
        payload.acid_normality,
        payload.titrant_volume_ml,
    );

    Ok(Json(ToolResult {
        tool: "alkalinity".to_string(),
        results: serde_json::json!({
            "alkalinity_meq_l": result.alkalinity_meq_l,
            "alkalinity_mg_l_caco3": result.alkalinity_mg_l_caco3,
        }),
    }))
}

// ============================================================================
// pCO2 Tool
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct Pco2Request {
    pub co2_aq_umol: f64,
    pub water_temp_c: f64,
    pub pressure_hpa: Option<f64>,
    #[serde(default)]
    pub variant: Pco2Variant,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pco2Variant {
    #[default]
    Simple,
    P1,
    P2,
}

pub async fn calculate_pco2(db: &DatabaseConnection, Json(payload): Json<Pco2Request>) -> AppResult<Json<ToolResult>> {
    let defaults = river_data_toolbox::GasConstants::default();
    let constants = river_data_toolbox::GasConstants {
        kh_co2: get_constant(db, "kh_co2", defaults.kh_co2).await,
        c_const: get_constant(db, "c_const", defaults.c_const).await,
        gas_const_r_atm: get_constant(db, "gas_const_r_atm", defaults.gas_const_r_atm).await,
        gas_const_r_mol: get_constant(db, "gas_const_r_mol", defaults.gas_const_r_mol).await,
        kh_ch4: get_constant(db, "kh_ch4", defaults.kh_ch4).await,
        ch4_temp_const: get_constant(db, "ch4_temp_const", defaults.ch4_temp_const).await,
        ch4_in_sa: get_constant(db, "ch4_in_sa", defaults.ch4_in_sa).await,
    };

    let pco2 = match payload.variant {
        Pco2Variant::Simple => {
            river_data_toolbox::pco2_from_co2aq(payload.co2_aq_umol, payload.water_temp_c, &constants)
        }
        Pco2Variant::P1 => {
            let bp = payload.pressure_hpa.ok_or_else(|| {
                AppError::BadRequest("pressure_hpa required for P1 variant".to_string())
            })?;
            river_data_toolbox::pco2_p1(payload.co2_aq_umol, payload.water_temp_c, bp, &constants)
        }
        Pco2Variant::P2 => {
            let bp = payload.pressure_hpa.ok_or_else(|| {
                AppError::BadRequest("pressure_hpa required for P2 variant".to_string())
            })?;
            river_data_toolbox::pco2_p2(payload.co2_aq_umol, payload.water_temp_c, bp, &constants)
        }
    };

    Ok(Json(ToolResult {
        tool: "pco2".to_string(),
        results: serde_json::json!({
            "pco2_uatm": pco2,
        }),
    }))
}

// ============================================================================
// DIC Tool
// ============================================================================

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
}

pub async fn calculate_dic(db: &DatabaseConnection, Json(payload): Json<DicRequest>) -> AppResult<Json<ToolResult>> {
    let constants = river_data_toolbox::DICConstants {
        h_co2_29815k: payload.h_co2_29815k.unwrap_or(get_constant(db, "h_co2_29815k", 0.034).await),
        gas_const_r_mol: payload.gas_const_r_mol.unwrap_or(get_constant(db, "gas_const_r_mol", 8.314).await),
        vial_volume: payload.vial_volume.unwrap_or(get_constant(db, "vial_volume", 12.0).await),
        h3po4_added: payload.h3po4_added.unwrap_or(get_constant(db, "h3po4_added", 0.1).await),
    };

    let dic = river_data_toolbox::dic_concentration(
        payload.acid_sample_weight_g,
        payload.acid_weight_g,
        payload.vol_overpressure_ml,
        payload.sa_added_ml,
        payload.co2_dry_ppm,
        payload.lab_temp_c,
        &constants,
    );

    let d13c = payload.d13co2_permil.map(|d13| {
        river_data_toolbox::d13c_dic(
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
            "dic_umol_l": dic,
            "d13c_dic_permil": d13c,
        }),
    }))
}

// ============================================================================
// DOM (SUVA, ratios) Tool
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct DomRequest {
    pub a254: Option<f64>,
    pub doc_avg_ppb: Option<f64>,
    pub abs_numerator: Option<f64>,
    pub abs_denominator: Option<f64>,
}

pub async fn calculate_dom(Json(payload): Json<DomRequest>) -> AppResult<Json<ToolResult>> {
    let suva = match (payload.a254, payload.doc_avg_ppb) {
        (Some(a), Some(d)) => Some(river_data_toolbox::suva(a, d)),
        _ => None,
    };
    let ratio = match (payload.abs_numerator, payload.abs_denominator) {
        (Some(n), Some(d)) => Some(river_data_toolbox::absorbance_ratio(n, d)),
        _ => None,
    };

    Ok(Json(ToolResult {
        tool: "dom".to_string(),
        results: serde_json::json!({
            "suva": suva,
            "absorbance_ratio": ratio,
        }),
    }))
}

// ============================================================================
// Field Data Tool
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct FieldDataRequest {
    pub elevation_m: Option<f64>,
    pub temp_c: Option<f64>,
    pub raw_co2: Option<f64>,
    pub pressure_hpa: Option<f64>,
    pub std_curve: Option<StdCurve>,
}

pub async fn calculate_field_data(
    Json(payload): Json<FieldDataRequest>,
) -> AppResult<Json<ToolResult>> {
    let bp = match (payload.elevation_m, payload.temp_c) {
        (Some(e), Some(t)) => Some(river_data_toolbox::barometric_pressure_from_altitude(e, t)),
        _ => None,
    };

    let co2_corr = match (payload.raw_co2, payload.pressure_hpa, payload.temp_c) {
        (Some(co2), Some(p), Some(t)) => {
            let curve = payload.std_curve.as_ref().map(|c| (c.slope, c.intercept));
            Some(river_data_toolbox::co2_correction(co2, p, t, curve))
        }
        _ => None,
    };

    Ok(Json(ToolResult {
        tool: "field_data".to_string(),
        results: serde_json::json!({
            "barometric_pressure_hpa": bp,
            "co2_corrected": co2_corr,
        }),
    }))
}

// ============================================================================
// CO2 Air Tool
// ============================================================================

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
        .map(|c| river_data_toolbox::co2_air::co2_dry(c, payload.h2o_percent));
    let ch4 = payload
        .ch4_wet
        .map(|c| river_data_toolbox::co2_air::ch4_dry_air(c, payload.h2o_percent));

    Ok(Json(ToolResult {
        tool: "co2_air".to_string(),
        results: serde_json::json!({
            "co2_dry": co2,
            "ch4_dry": ch4,
        }),
    }))
}

// ============================================================================
// Isotopes Tool
// ============================================================================

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
        (Some(dd), Some(d18)) => Some(river_data_toolbox::deuterium_excess(dd, d18)),
        _ => None,
    };
    let o17_excess = match (payload.d17o, payload.d18o) {
        (Some(d17), Some(d18)) => Some(river_data_toolbox::o17_excess(d17, d18)),
        _ => None,
    };

    Ok(Json(ToolResult {
        tool: "isotopes".to_string(),
        results: serde_json::json!({
            "d_excess": d_excess,
            "o17_excess_permeg": o17_excess,
        }),
    }))
}

// ============================================================================
// Benthic Tool
// ============================================================================

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
    let area = river_data_toolbox::rock_surface_area_m2(&payload.diameters_cm);

    let afdm_per_m2 = payload.afdm_g_filter.map(|afdm| {
        river_data_toolbox::per_m2(
            afdm,
            payload.total_volume_ml,
            payload.volume_filtered_ml,
            area,
        )
    });

    let chla_per_m2 = payload.chla_ug_l.map(|chla| {
        river_data_toolbox::per_m2(
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
            "afdm_per_m2": afdm_per_m2,
            "chla_per_m2": chla_per_m2,
        }),
    }))
}

// ============================================================================
// Tool list endpoint
// ============================================================================

#[derive(Debug, Serialize)]
pub struct ToolInfo {
    name: &'static str,
    description: &'static str,
    endpoint: &'static str,
}

const TOOLS: &[ToolInfo] = &[
    ToolInfo { name: "doc", description: "Dissolved Organic Carbon (replicate avg/sd with optional standard curve)", endpoint: "/api/service/tools/doc/calculate" },
    ToolInfo { name: "tss_afdm", description: "Total Suspended Solids & Ash-Free Dry Mass", endpoint: "/api/service/tools/tss_afdm/calculate" },
    ToolInfo { name: "chlorophyll", description: "Chlorophyll-a (acid and no-acid methods)", endpoint: "/api/service/tools/chlorophyll/calculate" },
    ToolInfo { name: "nutrients", description: "Nutrient replicates (PO4, NH4, NOx, NO2, TDP, TDN)", endpoint: "/api/service/tools/nutrients/calculate" },
    ToolInfo { name: "ions", description: "IC ion charge balance verification", endpoint: "/api/service/tools/ions/calculate" },
    ToolInfo { name: "alkalinity", description: "Gran titration alkalinity (meq/L, mg/L CaCO3)", endpoint: "/api/service/tools/alkalinity/calculate" },
    ToolInfo { name: "pco2", description: "pCO2 from headspace CO2aq (simple, P1, P2 variants)", endpoint: "/api/service/tools/pco2/calculate" },
    ToolInfo { name: "dic", description: "DIC concentration and d13C-DIC from acid digestion", endpoint: "/api/service/tools/dic/calculate" },
    ToolInfo { name: "dom", description: "SUVA, absorbance ratios, spectral slopes", endpoint: "/api/service/tools/dom/calculate" },
    ToolInfo { name: "field_data", description: "Barometric pressure from altitude, CO2 correction", endpoint: "/api/service/tools/field_data/calculate" },
    ToolInfo { name: "co2_air", description: "CO2/CH4 dry concentrations from wet measurements", endpoint: "/api/service/tools/co2_air/calculate" },
    ToolInfo { name: "isotopes", description: "Deuterium excess, 17O excess", endpoint: "/api/service/tools/isotopes/calculate" },
    ToolInfo { name: "benthic", description: "Rock surface area, per-m2 normalizations", endpoint: "/api/service/tools/benthic/calculate" },
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
    match tool_name.as_str() {
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
        _ => Err(AppError::NotFound(format!("Unknown tool: {tool_name}"))),
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> AppResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request body: {e}")))
}
