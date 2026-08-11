use chrono::{DateTime, Utc};

// ============================================================================
// Fixed IDs for deterministic tests
// ============================================================================

pub const PROJECT_ID: &str = "00000000-0000-4000-a000-000000000001";

pub const SITE1_ID: &str = "00000000-0000-4000-a000-000000000010";
pub const SITE2_ID: &str = "00000000-0000-4000-a000-000000000020";

pub const GLOBAL_PARAM_TEMP_ID: &str = "00000000-0000-4000-b000-000000000001";
pub const GLOBAL_PARAM_DO_ID: &str = "00000000-0000-4000-b000-000000000002";
pub const GLOBAL_PARAM_COND_ID: &str = "00000000-0000-4000-b000-000000000003";
pub const GLOBAL_PARAM_TURB_ID: &str = "00000000-0000-4000-b000-000000000004";
pub const GLOBAL_PARAM_DEPTH_ID: &str = "00000000-0000-4000-b000-000000000005";

pub const PARAM_S1_TEMP_ID: &str = "00000000-0000-4000-a000-000000000101";
pub const PARAM_S1_DO_ID: &str = "00000000-0000-4000-a000-000000000102";
pub const PARAM_S1_COND_ID: &str = "00000000-0000-4000-a000-000000000103";
pub const PARAM_S1_TURB_ID: &str = "00000000-0000-4000-a000-000000000104";
pub const PARAM_S1_DEPTH_ID: &str = "00000000-0000-4000-a000-000000000105";

pub const PARAM_S2_TEMP_ID: &str = "00000000-0000-4000-a000-000000000201";
pub const PARAM_S2_DO_ID: &str = "00000000-0000-4000-a000-000000000202";
pub const PARAM_S2_COND_ID: &str = "00000000-0000-4000-a000-000000000203";
pub const PARAM_S2_TURB_ID: &str = "00000000-0000-4000-a000-000000000204";

pub const STREAM1_ID: &str = "00000000-0000-4000-c000-000000000001";
pub const STREAM2_ID: &str = "00000000-0000-4000-c000-000000000002";

pub fn base_time() -> DateTime<Utc> {
    "2025-01-15T00:00:00Z".parse().unwrap()
}

pub const READINGS_PER_PARAM: usize = 288;

pub struct ParamConfig {
    pub site_param_id: &'static str,
    pub site_id: &'static str,
    pub global_param_id: &'static str,
    pub name: &'static str,
    pub sensor_type: &'static str,
    pub display_units: &'static str,
    pub units_name: &'static str,
    pub units_min: f64,
    pub units_max: f64,
    pub decimal_places: i16,
    pub value_mean: f64,
    pub value_amplitude: f64,
}

pub fn param_configs() -> Vec<ParamConfig> {
    vec![
        ParamConfig {
            site_param_id: PARAM_S1_TEMP_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_TEMP_ID,
            name: "DO_Temperature",
            sensor_type: "DO_Temperature",
            display_units: "°C",
            units_name: "Degrees Celsius",
            units_min: -10.0,
            units_max: 50.0,
            decimal_places: 2,
            value_mean: 13.0,
            value_amplitude: 9.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_DO_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_DO_ID,
            name: "Dissolved_O2",
            sensor_type: "Dissolved_O2",
            display_units: "µM",
            units_name: "Micromolar",
            units_min: 0.0,
            units_max: 625.0,
            decimal_places: 1,
            value_mean: 250.0,
            value_amplitude: 100.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_COND_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_COND_ID,
            name: "Conductivity",
            sensor_type: "Conductivity",
            display_units: "µS/cm",
            units_name: "Microsiemens per centimeter",
            units_min: 0.0,
            units_max: 2000.0,
            decimal_places: 1,
            value_mean: 450.0,
            value_amplitude: 350.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_TURB_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_TURB_ID,
            name: "Turbidity",
            sensor_type: "Turbidity",
            display_units: "NTU",
            units_name: "Nephelometric Turbidity Units",
            units_min: 0.0,
            units_max: 1000.0,
            decimal_places: 1,
            value_mean: 50.0,
            value_amplitude: 45.0,
        },
        ParamConfig {
            site_param_id: PARAM_S1_DEPTH_ID,
            site_id: SITE1_ID,
            global_param_id: GLOBAL_PARAM_DEPTH_ID,
            name: "Depth",
            sensor_type: "Depth",
            display_units: "mm",
            units_name: "Millimeters",
            units_min: 0.0,
            units_max: 3000.0,
            decimal_places: 0,
            value_mean: 500.0,
            value_amplitude: 450.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_TEMP_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_TEMP_ID,
            name: "DO_Temperature",
            sensor_type: "DO_Temperature",
            display_units: "°C",
            units_name: "Degrees Celsius",
            units_min: -10.0,
            units_max: 50.0,
            decimal_places: 2,
            value_mean: 14.0,
            value_amplitude: 8.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_DO_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_DO_ID,
            name: "Dissolved_O2",
            sensor_type: "Dissolved_O2",
            display_units: "µM",
            units_name: "Micromolar",
            units_min: 0.0,
            units_max: 625.0,
            decimal_places: 1,
            value_mean: 230.0,
            value_amplitude: 90.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_COND_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_COND_ID,
            name: "Conductivity",
            sensor_type: "Conductivity",
            display_units: "µS/cm",
            units_name: "Microsiemens per centimeter",
            units_min: 0.0,
            units_max: 2000.0,
            decimal_places: 1,
            value_mean: 500.0,
            value_amplitude: 300.0,
        },
        ParamConfig {
            site_param_id: PARAM_S2_TURB_ID,
            site_id: SITE2_ID,
            global_param_id: GLOBAL_PARAM_TURB_ID,
            name: "Turbidity",
            sensor_type: "Turbidity",
            display_units: "NTU",
            units_name: "Nephelometric Turbidity Units",
            units_min: 0.0,
            units_max: 1000.0,
            decimal_places: 1,
            value_mean: 60.0,
            value_amplitude: 50.0,
        },
    ]
}

pub struct ThresholdConfig {
    pub global_param_id: &'static str,
    pub site_id: Option<&'static str>,
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
    pub description: &'static str,
}

pub fn threshold_configs() -> Vec<ThresholdConfig> {
    vec![
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_TEMP_ID,
            site_id: None,
            warning_min: Some(0.5),
            warning_max: Some(20.0),
            alarm_min: Some(0.0),
            alarm_max: Some(25.0),
            description: "Water temperature thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_DO_ID,
            site_id: None,
            warning_min: Some(120.0),
            warning_max: Some(360.0),
            alarm_min: Some(0.0),
            alarm_max: Some(625.0),
            description: "Dissolved oxygen thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_COND_ID,
            site_id: None,
            warning_min: Some(100.0),
            warning_max: Some(900.0),
            alarm_min: Some(0.0),
            alarm_max: Some(1000.0),
            description: "Conductivity thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_TURB_ID,
            site_id: None,
            warning_min: None,
            warning_max: Some(100.0),
            alarm_min: Some(0.0),
            alarm_max: Some(500.0),
            description: "Turbidity thresholds",
        },
        ThresholdConfig {
            global_param_id: GLOBAL_PARAM_DEPTH_ID,
            site_id: None,
            warning_min: Some(100.0),
            warning_max: Some(1000.0),
            alarm_min: Some(0.0),
            alarm_max: Some(2000.0),
            description: "Depth thresholds",
        },
    ]
}
