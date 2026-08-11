use async_trait::async_trait;
use axum::{Json, extract::{Path, State}};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use crate::common::AppState;
use crate::error::{AppError, AppResult};

/// Reads a named constant from the `constants` table. A missing row falls back
/// to the default with a warning; a database error propagates as 500.
async fn get_constant(db: &DatabaseConnection, name: &str, default: f64) -> AppResult<f64> {
    use crate::routes::private::constants;
    match constants::Entity::find()
        .filter(constants::Column::Name.eq(name))
        .one(db)
        .await?
    {
        Some(c) => Ok(c.value),
        None => {
            tracing::warn!(constant = name, default, "constant missing, using default");
            Ok(default)
        }
    }
}

async fn load_gas_constants(
    db: &DatabaseConnection,
) -> AppResult<river_data_core::toolbox::GasConstants> {
    let defaults = river_data_core::toolbox::GasConstants::default();
    Ok(river_data_core::toolbox::GasConstants {
        c_const: get_constant(db, "c_const", defaults.c_const).await?,
        gas_const_r_atm: get_constant(db, "gas_const_r_atm", defaults.gas_const_r_atm).await?,
        gas_const_r_mol: get_constant(db, "gas_const_r_mol", defaults.gas_const_r_mol).await?,
        h_ch4_29815k: get_constant(db, "h_ch4_29815k", defaults.h_ch4_29815k).await?,
        ch4_in_sa: get_constant(db, "ch4_in_sa", defaults.ch4_in_sa).await?,
    })
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> AppResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("Invalid request body: {e}")))
}

fn require_field(val: Option<f64>, name: &str) -> AppResult<f64> {
    val.filter(|v| v.is_finite())
        .ok_or_else(|| AppError::BadRequest(format!("{name} is required and must be a finite number")))
}

type ResultMap = serde_json::Map<String, serde_json::Value>;

/// Inserts a numeric result, omitting non-finite values. This encodes the
/// portal's 'KEEP OLD' semantics: an uncomputable value is absent from the
/// response so a save cannot clobber stored data.
fn insert_num(results: &mut ResultMap, key: &str, value: f64) {
    if value.is_finite() {
        results.insert(key.to_string(), serde_json::json!(value));
    }
}

fn insert_opt(results: &mut ResultMap, key: &str, value: Option<f64>) {
    if let Some(v) = value {
        insert_num(results, key, v);
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolResult {
    pub tool: String,
    #[schema(value_type = Object)]
    pub results: serde_json::Value,
    pub inputs_used: Vec<String>,
    pub inputs_ignored: Vec<String>,
}

impl ToolResult {
    fn new(tool: &str, results: ResultMap) -> Self {
        Self {
            tool: tool.to_string(),
            results: serde_json::Value::Object(results),
            inputs_used: vec![],
            inputs_ignored: vec![],
        }
    }
}

#[derive(Debug, Serialize, Clone, utoipa::ToSchema)]
pub struct ToolParamInfo {
    pub name: &'static str,
    pub label: &'static str,
    pub required: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ToolInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub endpoint: &'static str,
    pub params: &'static [ToolParamInfo],
    pub match_keywords: &'static [&'static str],
}

#[async_trait]
pub trait AnalyticalTool: Send + Sync {
    fn info(&self) -> &'static ToolInfo;
    async fn calculate(&self, body: &[u8], db: &DatabaseConnection) -> AppResult<ToolResult>;
}

static REGISTRY: OnceLock<Vec<Box<dyn AnalyticalTool>>> = OnceLock::new();

fn registry() -> &'static [Box<dyn AnalyticalTool>] {
    REGISTRY.get_or_init(|| vec![
        Box::new(DocTool),
        Box::new(TssAfdmTool),
        Box::new(ChlorophyllTool),
        Box::new(NutrientsTool),
        Box::new(AlkalinityTool),
        Box::new(Pco2Tool),
        Box::new(DicTool),
        Box::new(DomTool),
        Box::new(FieldDataTool),
        Box::new(Co2AirTool),
        Box::new(BenthicTool),
        Box::new(ChlaBenthicTool),
    ])
}

/// List all available analytical tools (DOC, DIC, pCO2, etc.) with their parameter schemas.
/// Requires `read_data`.
#[utoipa::path(
    get,
    path = "/tools",
    responses(
        (status = 200, description = "List of tool descriptors", body = [ToolInfo]),
    ),
    tag = "tools"
)]
pub async fn list_tools() -> Json<Vec<&'static ToolInfo>> {
    Json(registry().iter().map(|t| t.info()).collect())
}

/// Run an analytical tool calculation. Request body schema is per-tool (call `GET /tools`
/// to discover required fields). Requires `read_data`.
#[utoipa::path(
    post,
    path = "/tools/{tool_name}/calculate",
    params(("tool_name" = String, Path, description = "Tool name (e.g. 'doc', 'dic', 'pco2')")),
    request_body(content = Object, description = "Per-tool request body (see GET /tools for schemas)"),
    responses(
        (status = 200, description = "Calculation result with `inputs_used` / `inputs_ignored` accounting", body = ToolResult),
        (status = 404, description = "Unknown tool name"),
        (status = 400, description = "Invalid input for this tool"),
    ),
    tag = "tools"
)]
pub async fn calculate_tool(
    State(state): State<AppState>,
    Path(tool_name): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<Json<ToolResult>> {
    let tools = registry();
    let tool = tools.iter()
        .find(|t| t.info().name == tool_name.as_str())
        .ok_or_else(|| AppError::NotFound(format!("Unknown tool: {tool_name}")))?;

    let tool_info = tool.info();

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

    let mut result = tool.calculate(&body, &state.db).await?;
    result.inputs_used = inputs_used;
    result.inputs_ignored = inputs_ignored;

    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
pub struct DocRequest {
    pub replicates: Vec<f64>,
    pub std_curve: Option<StdCurve>,
}

#[derive(Debug, Deserialize)]
pub struct StdCurve {
    pub slope: f64,
    pub intercept: f64,
}

struct DocTool;

#[async_trait]
impl AnalyticalTool for DocTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "doc",
            description: "Dissolved Organic Carbon (replicate avg/sd with optional standard curve)",
            endpoint: "/api/tools/doc/calculate",
            params: &[
                ToolParamInfo { name: "replicates", label: "Replicates (array)", required: true },
                ToolParamInfo { name: "std_curve", label: "Standard Curve (slope/intercept)", required: false },
            ],
            match_keywords: &["doc", "organic carbon", "dissolved organic"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: DocRequest = parse_body(body)?;
        let curve = payload.std_curve.as_ref().map(|c| (c.slope, c.intercept));
        let avg = river_data_core::toolbox::doc_average(&payload.replicates, curve);
        let sd = river_data_core::toolbox::doc_std_dev(&payload.replicates, curve);

        let mut results = ResultMap::new();
        insert_num(&mut results, "DOC_avg_ppb", avg);
        insert_num(&mut results, "DOC_sd_ppb", sd);
        Ok(ToolResult::new("doc", results))
    }
}

#[derive(Debug, Deserialize)]
pub struct TssAfdmRequest {
    pub wgt_dried_g: f64,
    pub wgt_prefilt_g: f64,
    pub wgt_ashed_g: Option<f64>,
    pub vol_filtered_ml: f64,
}

struct TssAfdmTool;

#[async_trait]
impl AnalyticalTool for TssAfdmTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "tss_afdm",
            description: "Total Suspended Solids & Ash-Free Dry Mass",
            endpoint: "/api/tools/tss_afdm/calculate",
            params: &[
                ToolParamInfo { name: "wgt_dried_g", label: "Dried Weight (g)", required: true },
                ToolParamInfo { name: "wgt_prefilt_g", label: "Pre-filter Weight (g)", required: true },
                ToolParamInfo { name: "wgt_ashed_g", label: "Ashed Weight (g)", required: false },
                ToolParamInfo { name: "vol_filtered_ml", label: "Volume Filtered (mL)", required: true },
            ],
            match_keywords: &["tss", "afdm", "suspended solid", "dry mass"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: TssAfdmRequest = parse_body(body)?;
        let tss = river_data_core::toolbox::tss_mg_l(
            payload.wgt_dried_g,
            payload.wgt_prefilt_g,
            payload.vol_filtered_ml,
        );

        let mut results = ResultMap::new();
        insert_num(&mut results, "TSS_dry_weight_mgL", tss);
        if let Some(ashed) = payload.wgt_ashed_g {
            let afdm = river_data_core::toolbox::afdm_mg_l(
                payload.wgt_dried_g,
                ashed,
                payload.vol_filtered_ml,
            );
            insert_num(&mut results, "AFDM_mgL", afdm);
        }
        Ok(ToolResult::new("tss_afdm", results))
    }
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

struct ChlorophyllTool;

#[async_trait]
impl AnalyticalTool for ChlorophyllTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "chlorophyll",
            description: "Chlorophyll-a (acid and no-acid methods)",
            endpoint: "/api/tools/chlorophyll/calculate",
            params: &[
                ToolParamInfo { name: "method", label: "Method (acid/no_acid)", required: true },
                ToolParamInfo { name: "fluorescence_before", label: "Fluorescence Before", required: true },
                ToolParamInfo { name: "fluorescence_after", label: "Fluorescence After", required: false },
                ToolParamInfo { name: "slope", label: "Slope", required: true },
                ToolParamInfo { name: "intercept", label: "Intercept", required: true },
            ],
            match_keywords: &["chlorophyll", "chla", "chl"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: ChlorophyllRequest = parse_body(body)?;
        let mut results = ResultMap::new();
        match payload.method {
            ChlorophyllMethod::Acid => {
                let after = payload.fluorescence_after.ok_or_else(|| {
                    AppError::BadRequest(
                        "fluorescence_after required for acid method".to_string(),
                    )
                })?;
                let chla = river_data_core::toolbox::chla_acid(
                    payload.fluorescence_before,
                    after,
                    payload.slope,
                    payload.intercept,
                );
                insert_num(&mut results, "Chla_acid_ugL_avg", chla);
            }
            ChlorophyllMethod::NoAcid => {
                let chla = river_data_core::toolbox::chla_no_acid(
                    payload.fluorescence_before,
                    payload.slope,
                    payload.intercept,
                );
                insert_num(&mut results, "Chla_noacid_ugL_avg", chla);
            }
        }
        Ok(ToolResult::new("chlorophyll", results))
    }
}

#[derive(Debug, Deserialize)]
pub struct NutrientsRequest {
    pub species: Option<std::collections::HashMap<String, Vec<f64>>>,
    pub replicates: Option<Vec<f64>>,
    pub nox: Option<f64>,
    pub no2: Option<f64>,
}

struct NutrientsTool;

#[async_trait]
impl AnalyticalTool for NutrientsTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "nutrients",
            description: "Nutrient replicates (P, NH4, SRP, NOx, NO2, TDP, TDN)",
            endpoint: "/api/tools/nutrients/calculate",
            params: &[
                ToolParamInfo { name: "species", label: "Species (multi-species map)", required: false },
                ToolParamInfo { name: "replicates", label: "Replicates (single-species)", required: false },
                ToolParamInfo { name: "nox", label: "NOx", required: false },
                ToolParamInfo { name: "no2", label: "NO2", required: false },
            ],
            match_keywords: &["po4", "nh4", "srp", "nox", "no2", "no3", "tdp", "tdn", "nutrient", "phosph", "nitrate", "nitrite", "ammonium"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: NutrientsRequest = parse_body(body)?;
        let mut results = ResultMap::new();

        if let Some(species) = &payload.species {
            let multi = river_data_core::toolbox::multi_nutrient_replicates(species);
            for (name, nr) in &multi {
                // Portal column casing: NUT_NOx_avg, NUT_P_avg, ...; the METALP
                // portal stores NH4/SRP as standalone ugL columns.
                let (avg_key, sd_key) = match name.as_str() {
                    "NH4" => ("NH4_avg_ugL".to_string(), "NH4_sd_ugL".to_string()),
                    "SRP" => ("SRP_avg_ugL".to_string(), "SRP_sd_ugL".to_string()),
                    _ => (format!("NUT_{name}_avg"), format!("NUT_{name}_sd")),
                };
                insert_num(&mut results, &avg_key, nr.mean);
                insert_num(&mut results, &sd_key, nr.std_dev);
            }
        } else if let Some(replicates) = &payload.replicates {
            let result = river_data_core::toolbox::nutrient_from_replicates(replicates);
            insert_num(&mut results, "NUT_avg", result.mean);
            insert_num(&mut results, "NUT_sd", result.std_dev);
            if let (Some(nox), Some(no2)) = (payload.nox, payload.no2) {
                let no3 = river_data_core::toolbox::nitrate_from_nox_no2(nox, no2);
                insert_num(&mut results, "NUT_NO3_avg", no3);
            }
        }

        Ok(ToolResult::new("nutrients", results))
    }
}

/// Raw alkalinity entry. The portal computes nothing from these columns; its
/// only calculation is filling WTW_pH_1 from Alk_init_pH when missing
/// (`calcEquals`). Raw values echo through so the save path can persist them.
#[derive(Debug, Deserialize)]
pub struct AlkalinityRequest {
    #[serde(rename = "Alk_meqL")]
    pub alk_meq_l: Option<f64>,
    #[serde(rename = "Alk_mgL")]
    pub alk_mg_l: Option<f64>,
    #[serde(rename = "Alk_w_weight_g")]
    pub alk_w_weight_g: Option<f64>,
    #[serde(rename = "Alk_dyn_pH")]
    pub alk_dyn_ph: Option<f64>,
    #[serde(rename = "Alk_dyn_trit")]
    pub alk_dyn_trit: Option<f64>,
    #[serde(rename = "Alk_temp_degC")]
    pub alk_temp_deg_c: Option<f64>,
    #[serde(rename = "Alk_init_pH")]
    pub alk_init_ph: Option<f64>,
    #[serde(rename = "WTW_pH_1")]
    pub wtw_ph_1: Option<f64>,
}

struct AlkalinityTool;

#[async_trait]
impl AnalyticalTool for AlkalinityTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "alkalinity",
            description: "Alkalinity raw entry (fills WTW_pH_1 from Alk_init_pH when missing)",
            endpoint: "/api/tools/alkalinity/calculate",
            params: &[
                ToolParamInfo { name: "Alk_meqL", label: "Alkalinity (meq/L)", required: false },
                ToolParamInfo { name: "Alk_mgL", label: "Alkalinity (mg/L)", required: false },
                ToolParamInfo { name: "Alk_w_weight_g", label: "Water Weight (g)", required: false },
                ToolParamInfo { name: "Alk_dyn_pH", label: "Dynamic pH", required: false },
                ToolParamInfo { name: "Alk_dyn_trit", label: "Dynamic Titrant", required: false },
                ToolParamInfo { name: "Alk_temp_degC", label: "Temperature (degC)", required: false },
                ToolParamInfo { name: "Alk_init_pH", label: "Initial pH", required: false },
                ToolParamInfo { name: "WTW_pH_1", label: "Existing WTW pH", required: false },
            ],
            match_keywords: &["alkalinity", "alk", "titration", "caco3"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: AlkalinityRequest = parse_body(body)?;
        let mut results = ResultMap::new();

        insert_opt(&mut results, "Alk_meqL", payload.alk_meq_l);
        insert_opt(&mut results, "Alk_mgL", payload.alk_mg_l);
        insert_opt(&mut results, "Alk_w_weight_g", payload.alk_w_weight_g);
        insert_opt(&mut results, "Alk_dyn_pH", payload.alk_dyn_ph);
        insert_opt(&mut results, "Alk_dyn_trit", payload.alk_dyn_trit);
        insert_opt(&mut results, "Alk_temp_degC", payload.alk_temp_deg_c);

        let ph = river_data_core::toolbox::equals(
            payload.wtw_ph_1.unwrap_or(f64::NAN),
            payload.alk_init_ph.unwrap_or(f64::NAN),
        );
        insert_num(&mut results, "WTW_pH_1", ph);

        Ok(ToolResult::new("alkalinity", results))
    }
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
    pub co2_aq_umol: Option<f64>,
    pub water_temp_c: f64,
    pub pressure_hpa: Option<f64>,
    #[serde(default)]
    pub variant: Pco2Variant,
    #[serde(default)]
    pub mode: Pco2Mode,
    pub co2_ppm: Option<f64>,
    pub h2o_percent: Option<f64>,
    pub ch4_ppm: Option<f64>,
    pub d13co2_permil: Option<f64>,
    pub lab_temp_c: Option<f64>,
    pub lab_pressure_atm: Option<f64>,
    pub vol_sa_ml: Option<f64>,
    pub vol_water_ml: Option<f64>,
    pub replicate_b: Option<Pco2ReplicateBInput>,
}

#[derive(Debug, Deserialize)]
pub struct Pco2ReplicateBInput {
    pub co2_ppm: f64,
    pub h2o_percent: f64,
    pub ch4_ppm: f64,
    pub d13co2_permil: Option<f64>,
}

struct Pco2Tool;

#[async_trait]
impl AnalyticalTool for Pco2Tool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "pco2",
            description: "pCO2 from headspace CO2aq (simple, P1, P2 variants)",
            endpoint: "/api/tools/pco2/calculate",
            params: &[
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
            ],
            match_keywords: &["co2", "pco2", "headspace", "carbon dioxide"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: Pco2Request = parse_body(body)?;
        let constants = load_gas_constants(db).await?;

        match payload.mode {
            Pco2Mode::Simple => {
                let co2_aq = payload.co2_aq_umol.ok_or_else(|| {
                    AppError::BadRequest("co2_aq_umol is required for simple mode".to_string())
                })?;

                let pco2 = match payload.variant {
                    Pco2Variant::Simple => {
                        river_data_core::toolbox::pco2_from_co2aq(co2_aq, payload.water_temp_c, &constants)
                    }
                    Pco2Variant::P1 => {
                        let bp = payload.pressure_hpa.ok_or_else(|| {
                            AppError::BadRequest("pressure_hpa required for P1 variant".to_string())
                        })?;
                        river_data_core::toolbox::pco2_p1(co2_aq, payload.water_temp_c, bp, &constants)
                    }
                    Pco2Variant::P2 => {
                        let bp = payload.pressure_hpa.ok_or_else(|| {
                            AppError::BadRequest("pressure_hpa required for P2 variant".to_string())
                        })?;
                        river_data_core::toolbox::pco2_p2(co2_aq, payload.water_temp_c, bp, &constants)
                    }
                };

                let key = match payload.variant {
                    Pco2Variant::Simple => "pCO2_HS_uatm_avg",
                    Pco2Variant::P1 => "pCO2_HS_P1_uatm_avg",
                    Pco2Variant::P2 => "pCO2_HS_P2_uatm_avg",
                };
                let mut results = ResultMap::new();
                insert_num(&mut results, key, pco2);
                Ok(ToolResult::new("pco2", results))
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

                let mut results = ResultMap::new();

                if let Some(rep_b) = &payload.replicate_b {
                    let input_b = river_data_core::toolbox::Pco2FullInput {
                        co2_ppm: rep_b.co2_ppm,
                        h2o_percent: rep_b.h2o_percent,
                        ch4_ppm: rep_b.ch4_ppm,
                        d13co2_permil: rep_b.d13co2_permil,
                        lab_temp_c,
                        lab_pressure_atm,
                        vol_sa_ml,
                        vol_water_ml,
                        water_temp_c: payload.water_temp_c,
                        field_pressure_hpa,
                    };

                    let rep = river_data_core::toolbox::pco2_replicates(&input_a, &input_b, &constants);

                    insert_num(&mut results, "CO2_HS_Um_A", rep.a.co2_hs_umol);
                    insert_num(&mut results, "CO2_HS_Um_B", rep.b.co2_hs_umol);
                    insert_num(&mut results, "CO2_HS_Um_avg", rep.co2_hs_umol_avg);
                    insert_num(&mut results, "CO2_HS_Um_sd", rep.co2_hs_umol_sd);
                    insert_num(&mut results, "pCO2_HS_uatm_avg", rep.pco2_uatm_avg);
                    insert_num(&mut results, "pCO2_HS_uatm_sd", rep.pco2_uatm_sd);
                    insert_num(&mut results, "pCO2_HS_P1_uatm_avg", rep.pco2_p1_uatm_avg);
                    insert_num(&mut results, "pCO2_HS_P1_uatm_sd", rep.pco2_p1_uatm_sd);
                    insert_num(&mut results, "pCO2_HS_P2_uatm_avg", rep.pco2_p2_uatm_avg);
                    insert_num(&mut results, "pCO2_HS_P2_uatm_sd", rep.pco2_p2_uatm_sd);
                    insert_opt(&mut results, "d13C_CO2_avg", rep.d13co2_permil_avg);
                    insert_opt(&mut results, "d13C_CO2_sd", rep.d13co2_permil_sd);
                    insert_num(&mut results, "CH4_umol_L_avg", rep.ch4_dissolved_umol_avg);
                    insert_num(&mut results, "CH4_umol_L_sd", rep.ch4_dissolved_umol_sd);
                } else {
                    let r = river_data_core::toolbox::pco2_full_pipeline(&input_a, &constants);

                    insert_num(&mut results, "CO2_HS_Um_avg", r.co2_hs_umol);
                    insert_num(&mut results, "pCO2_HS_uatm_avg", r.pco2_uatm);
                    insert_num(&mut results, "pCO2_HS_P1_uatm_avg", r.pco2_p1_uatm);
                    insert_num(&mut results, "pCO2_HS_P2_uatm_avg", r.pco2_p2_uatm);
                    insert_opt(&mut results, "d13C_CO2_avg", r.d13co2_permil);
                    insert_num(&mut results, "CH4_umol_L_avg", r.ch4_dissolved_umol);
                }

                Ok(ToolResult::new("pco2", results))
            }
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
    pub lab_temp_c: Option<f64>,
    pub h_co2_29815k: Option<f64>,
    pub gas_const_r_mol: Option<f64>,
    pub vial_volume: Option<f64>,
    pub h3po4_added: Option<f64>,
    pub replicate_b: Option<DicReplicateBInput>,
}

struct DicTool;

#[async_trait]
impl AnalyticalTool for DicTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "dic",
            description: "DIC concentration and d13C-DIC from acid digestion",
            endpoint: "/api/tools/dic/calculate",
            params: &[
                ToolParamInfo { name: "acid_sample_weight_g", label: "Acid Sample Weight (g)", required: true },
                ToolParamInfo { name: "acid_weight_g", label: "Acid Weight (g)", required: true },
                ToolParamInfo { name: "vol_overpressure_ml", label: "Vol Overpressure (mL)", required: true },
                ToolParamInfo { name: "sa_added_ml", label: "SA Added (mL)", required: true },
                ToolParamInfo { name: "co2_dry_ppm", label: "CO2 Dry (ppm)", required: true },
                ToolParamInfo { name: "d13co2_permil", label: "d13CO2 (permil)", required: false },
                ToolParamInfo { name: "lab_temp_c", label: "Lab Temp (°C, defaults to lab_temp_avg_degC constant)", required: false },
                ToolParamInfo { name: "h_co2_29815k", label: "H CO2 @298.15K", required: false },
                ToolParamInfo { name: "gas_const_r_mol", label: "Gas Const R (mol)", required: false },
                ToolParamInfo { name: "vial_volume", label: "Vial Volume (mL)", required: false },
                ToolParamInfo { name: "h3po4_added", label: "H3PO4 Added (mL)", required: false },
                ToolParamInfo { name: "replicate_b", label: "Replicate B", required: false },
            ],
            match_keywords: &["dic", "d13c", "inorganic carbon"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: DicRequest = parse_body(body)?;
        let constants = river_data_core::toolbox::DICConstants {
            h_co2_29815k: match payload.h_co2_29815k {
                Some(v) => v,
                None => get_constant(db, "h_co2_29815k", 0.034733).await?,
            },
            gas_const_r_mol: match payload.gas_const_r_mol {
                Some(v) => v,
                None => get_constant(db, "gas_const_r_mol", 8.31446).await?,
            },
            vial_volume: match payload.vial_volume {
                Some(v) => v,
                None => get_constant(db, "vial_volume", 12.168).await?,
            },
            h3po4_added: match payload.h3po4_added {
                Some(v) => v,
                None => get_constant(db, "h3po4_added", 0.3).await?,
            },
        };
        // Portal behavior: missing lab temp falls back to the lab_temp_avg_degC constant.
        let lab_temp_c = match payload.lab_temp_c {
            Some(t) => t,
            None => get_constant(db, "lab_temp_avg_degC", 22.5).await?,
        };

        let mut results = ResultMap::new();
        if let Some(ref rep_b) = payload.replicate_b {
            let rep = river_data_core::toolbox::dic_replicates(
                payload.acid_sample_weight_g, payload.acid_weight_g, payload.vol_overpressure_ml, payload.sa_added_ml, payload.co2_dry_ppm, payload.d13co2_permil,
                rep_b.acid_sample_weight_g, rep_b.acid_weight_g, rep_b.vol_overpressure_ml, rep_b.sa_added_ml, rep_b.co2_dry_ppm, rep_b.d13co2_permil,
                lab_temp_c,
                &constants,
            );

            insert_num(&mut results, "DIC_A", rep.dic_a);
            insert_num(&mut results, "DIC_B", rep.dic_b);
            insert_num(&mut results, "DIC_avg", rep.dic_avg);
            insert_num(&mut results, "DIC_std", rep.dic_std);
            insert_opt(&mut results, "d13C_DIC_A", rep.d13c_a);
            insert_opt(&mut results, "d13C_DIC_B", rep.d13c_b);
            insert_opt(&mut results, "d13C_DIC_avg", rep.d13c_avg);
            insert_opt(&mut results, "d13C_DIC_std", rep.d13c_std);
        } else {
            let dic = river_data_core::toolbox::dic_concentration(
                payload.acid_sample_weight_g,
                payload.acid_weight_g,
                payload.vol_overpressure_ml,
                payload.sa_added_ml,
                payload.co2_dry_ppm,
                lab_temp_c,
                &constants,
            );
            insert_num(&mut results, "DIC_avg", dic);

            if let Some(d13) = payload.d13co2_permil {
                let d13c = river_data_core::toolbox::d13c_dic(
                    payload.acid_sample_weight_g,
                    payload.acid_weight_g,
                    payload.vol_overpressure_ml,
                    d13,
                    lab_temp_c,
                    &constants,
                );
                insert_num(&mut results, "d13C_DIC_avg", d13c);
            }
        }
        Ok(ToolResult::new("dic", results))
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

struct DomTool;

#[async_trait]
impl AnalyticalTool for DomTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "dom",
            description: "SUVA and absorbance/fluorescence peak ratios",
            endpoint: "/api/tools/dom/calculate",
            params: &[
                ToolParamInfo { name: "a254", label: "Absorbance @254nm", required: false },
                ToolParamInfo { name: "doc_avg_ppb", label: "DOC Avg (ppb)", required: false },
                ToolParamInfo { name: "abs_numerator", label: "Absorbance Numerator", required: false },
                ToolParamInfo { name: "abs_denominator", label: "Absorbance Denominator", required: false },
                ToolParamInfo { name: "peak_a", label: "Peak A", required: false },
                ToolParamInfo { name: "peak_c", label: "Peak C", required: false },
                ToolParamInfo { name: "peak_m", label: "Peak M", required: false },
                ToolParamInfo { name: "peak_t", label: "Peak T", required: false },
            ],
            match_keywords: &["dom", "suva", "a254", "absorbance", "dissolved organic"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: DomRequest = parse_body(body)?;
        let mut results = ResultMap::new();

        if let (Some(a), Some(d)) = (payload.a254, payload.doc_avg_ppb) {
            insert_num(&mut results, "SUVA", river_data_core::toolbox::suva(a, d));
        }
        if let (Some(n), Some(d)) = (payload.abs_numerator, payload.abs_denominator) {
            insert_num(&mut results, "absorbance_ratio", river_data_core::toolbox::absorbance_ratio(n, d));
        }
        if let (Some(pa), Some(pt)) = (payload.peak_a, payload.peak_t) {
            insert_num(&mut results, "A_T", river_data_core::toolbox::absorbance_ratio(pa, pt));
        }
        if let (Some(pc), Some(pa)) = (payload.peak_c, payload.peak_a) {
            insert_num(&mut results, "C_A", river_data_core::toolbox::absorbance_ratio(pc, pa));
        }
        if let (Some(pc), Some(pm)) = (payload.peak_c, payload.peak_m) {
            insert_num(&mut results, "C_M", river_data_core::toolbox::absorbance_ratio(pc, pm));
        }
        if let (Some(pc), Some(pt)) = (payload.peak_c, payload.peak_t) {
            insert_num(&mut results, "C_T", river_data_core::toolbox::absorbance_ratio(pc, pt));
        }

        Ok(ToolResult::new("dom", results))
    }
}

#[derive(Debug, Deserialize)]
pub struct FieldDataRequest {
    pub elevation_m: Option<f64>,
    pub temp_c: Option<f64>,
    pub raw_co2: Option<f64>,
    pub pressure_hpa: Option<f64>,
    pub field_bp: Option<f64>,
    pub std_curve: Option<StdCurve>,
    pub raw_co2_min: Option<f64>,
    pub raw_co2_avg: Option<f64>,
    pub raw_co2_max: Option<f64>,
    pub reach_depths: Option<Vec<f64>>,
}

struct FieldDataTool;

#[async_trait]
impl AnalyticalTool for FieldDataTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "field_data",
            description: "Barometric pressure from altitude, pressure selection, CO2 correction, reach depths",
            endpoint: "/api/tools/field_data/calculate",
            params: &[
                ToolParamInfo { name: "elevation_m", label: "Elevation (m)", required: false },
                ToolParamInfo { name: "temp_c", label: "Temperature (°C)", required: false },
                ToolParamInfo { name: "pressure_hpa", label: "Pressure (hPa, explicit override)", required: false },
                ToolParamInfo { name: "field_bp", label: "Field BP (hPa, used when in 700-1050 range)", required: false },
                ToolParamInfo { name: "raw_co2", label: "Raw CO2 (ppm)", required: false },
                ToolParamInfo { name: "raw_co2_min", label: "Raw CO2 Min (ppm)", required: false },
                ToolParamInfo { name: "raw_co2_avg", label: "Raw CO2 Avg (ppm)", required: false },
                ToolParamInfo { name: "raw_co2_max", label: "Raw CO2 Max (ppm)", required: false },
                ToolParamInfo { name: "std_curve", label: "Standard Curve (slope/intercept)", required: false },
                ToolParamInfo { name: "reach_depths", label: "Reach Depths (cm)", required: false },
            ],
            match_keywords: &["field", "elevation", "altitude", "baro", "pressure", "co2", "depth", "reach"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: FieldDataRequest = parse_body(body)?;
        let mut results = ResultMap::new();

        let alt_bp = match (payload.elevation_m, payload.temp_c) {
            (Some(e), Some(t)) => {
                Some(river_data_core::toolbox::barometric_pressure_from_altitude(e, t))
            }
            _ => None,
        };
        insert_opt(&mut results, "Field_BP_altitude", alt_bp);

        // Portal pressure rule: field BP wins when within 700-1050 hPa, else the
        // altitude-derived BP. An explicit pressure_hpa overrides both.
        let pressure = payload
            .pressure_hpa
            .or_else(|| river_data_core::toolbox::select_pressure(payload.field_bp, alt_bp));

        let curve = payload.std_curve.as_ref().map(|c| (c.slope, c.intercept));

        if payload.raw_co2_min.is_some() || payload.raw_co2_avg.is_some() || payload.raw_co2_max.is_some() {
            if let (Some(p), Some(t)) = (pressure, payload.temp_c) {
                if let Some(co2_min) = payload.raw_co2_min {
                    insert_num(&mut results, "Vaisala_CO2_min_corr", river_data_core::toolbox::co2_correction(co2_min, p, t, curve));
                }
                if let Some(co2_avg) = payload.raw_co2_avg {
                    insert_num(&mut results, "Vaisala_CO2_avg_corr", river_data_core::toolbox::co2_correction(co2_avg, p, t, curve));
                }
                if let Some(co2_max) = payload.raw_co2_max {
                    insert_num(&mut results, "Vaisala_CO2_max_corr", river_data_core::toolbox::co2_correction(co2_max, p, t, curve));
                }
            }
        } else if let (Some(co2), Some(p), Some(t)) = (payload.raw_co2, pressure, payload.temp_c) {
            insert_num(&mut results, "Vaisala_CO2_avg_corr", river_data_core::toolbox::co2_correction(co2, p, t, curve));
        }

        if let Some(ref depths) = payload.reach_depths
            && !depths.is_empty()
        {
            let (avg, sd) = river_data_core::toolbox::reach_depth_stats(depths);
            insert_num(&mut results, "Reach_depth_avg_cm", avg);
            insert_num(&mut results, "Reach_depth_sd_cm", sd);
        }

        Ok(ToolResult::new("field_data", results))
    }
}

#[derive(Debug, Deserialize)]
pub struct Co2AirRequest {
    pub ch4_wet: f64,
    pub h2o_percent: f64,
}

struct Co2AirTool;

#[async_trait]
impl AnalyticalTool for Co2AirTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "co2_air",
            description: "CH4 dry concentration from wet measurement",
            endpoint: "/api/tools/co2_air/calculate",
            params: &[
                ToolParamInfo { name: "ch4_wet", label: "CH4 Wet (ppm)", required: true },
                ToolParamInfo { name: "h2o_percent", label: "H2O (%)", required: true },
            ],
            match_keywords: &["co2_air", "ch4", "methane"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: Co2AirRequest = parse_body(body)?;
        let ch4 = river_data_core::toolbox::co2_air::ch4_dry_air(payload.ch4_wet, payload.h2o_percent);

        let mut results = ResultMap::new();
        insert_num(&mut results, "lab_co2air_ch4_dry", ch4);
        Ok(ToolResult::new("co2_air", results))
    }
}

#[derive(Debug, Deserialize)]
pub struct BenthicRequest {
    pub diameters_cm: Vec<f64>,
    pub afdm_g_filter: Option<f64>,
    pub chla_ug_l: Option<f64>,
    pub volume_filtered_ml: f64,
    pub total_volume_ml: f64,
}

struct BenthicTool;

#[async_trait]
impl AnalyticalTool for BenthicTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "benthic",
            description: "Rock surface area, per-m2 normalizations",
            endpoint: "/api/tools/benthic/calculate",
            params: &[
                ToolParamInfo { name: "diameters_cm", label: "Rock Diameters (cm)", required: true },
                ToolParamInfo { name: "afdm_g_filter", label: "AFDM per Filter (g)", required: false },
                ToolParamInfo { name: "chla_ug_l", label: "Chl-a (ug/L)", required: false },
                ToolParamInfo { name: "volume_filtered_ml", label: "Volume Filtered (mL)", required: true },
                ToolParamInfo { name: "total_volume_ml", label: "Total Volume (mL)", required: true },
            ],
            match_keywords: &["benthic", "rock", "surface area", "periphyton"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: BenthicRequest = parse_body(body)?;
        let area = river_data_core::toolbox::rock_surface_area_m2(&payload.diameters_cm);

        let mut results = ResultMap::new();
        insert_num(&mut results, "rock_surface_area_m2", area);
        if let Some(afdm) = payload.afdm_g_filter {
            insert_num(&mut results, "benthic_AFDM_avg_gm2", river_data_core::toolbox::benthic_afdm_per_m2(
                afdm,
                &payload.diameters_cm,
                payload.volume_filtered_ml,
                payload.total_volume_ml,
            ));
        }
        if let Some(chla) = payload.chla_ug_l {
            insert_num(&mut results, "chla_per_m2", river_data_core::toolbox::benthic_chla_per_m2(
                chla,
                &payload.diameters_cm,
                payload.volume_filtered_ml,
                payload.total_volume_ml,
            ));
        }
        Ok(ToolResult::new("benthic", results))
    }
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

struct ChlaBenthicTool;

#[async_trait]
impl AnalyticalTool for ChlaBenthicTool {
    fn info(&self) -> &'static ToolInfo {
        static INFO: ToolInfo = ToolInfo {
            name: "chla_benthic",
            description: "Unified Chlorophyll-Benthic multi-replicate (acid + no-acid Chl-a, AFDM, per-m2)",
            endpoint: "/api/tools/chla_benthic/calculate",
            params: &[
                ToolParamInfo { name: "acid_slope", label: "Acid Slope", required: true },
                ToolParamInfo { name: "acid_intercept", label: "Acid Intercept", required: true },
                ToolParamInfo { name: "noacid_slope", label: "No-acid Slope", required: true },
                ToolParamInfo { name: "noacid_intercept", label: "No-acid Intercept", required: true },
                ToolParamInfo { name: "replicates", label: "Replicates (array)", required: true },
            ],
            match_keywords: &["chla_benthic", "chlorophyll", "benthic", "afdm"],
        };
        &INFO
    }

    async fn calculate(&self, body: &[u8], _db: &DatabaseConnection) -> AppResult<ToolResult> {
        let payload: ChlaBenthicRequest = parse_body(body)?;
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

        let mut results = ResultMap::new();
        insert_opt(&mut results, "Chla_acid_ugL_avg", result.chla_acid_ug_l_avg);
        insert_opt(&mut results, "Chla_acid_ugL_sd", result.chla_acid_ug_l_sd);
        insert_num(&mut results, "Chla_noacid_ugL_avg", result.chla_noacid_ug_l_avg);
        insert_num(&mut results, "Chla_noacid_ugL_sd", result.chla_noacid_ug_l_sd);
        insert_opt(&mut results, "Chla_acid_ugm2_avg", result.chla_acid_ug_m2_avg);
        insert_opt(&mut results, "Chla_acid_ugm2_sd", result.chla_acid_ug_m2_sd);
        insert_num(&mut results, "Chla_noacid_ugm2_avg", result.chla_noacid_ug_m2_avg);
        insert_num(&mut results, "Chla_noacid_ugm2_sd", result.chla_noacid_ug_m2_sd);
        insert_opt(&mut results, "benthic_AFDM_avg_gm2", result.afdm_g_m2_avg);
        insert_opt(&mut results, "benthic_AFDM_sd_gm2", result.afdm_g_m2_sd);

        Ok(ToolResult::new("chla_benthic", results))
    }
}
