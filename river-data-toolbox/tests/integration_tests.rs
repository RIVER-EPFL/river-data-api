use river_data_toolbox::*;
use serde::Deserialize;

// ---- Generic fixture types ----

#[derive(Deserialize)]
struct Fixture<C> {
    #[allow(dead_code)]
    tool: String,
    cases: Vec<C>,
}

// ---- pCO2 ----

#[derive(Deserialize)]
struct Pco2Case {
    name: String,
    function: String,
    inputs: Pco2Inputs,
    expected: Pco2Expected,
    tolerance: f64,
}

#[derive(Deserialize)]
struct Pco2Inputs {
    co2_aq_umol: f64,
    water_temp_c: f64,
    pressure_hpa: Option<f64>,
}

#[derive(Deserialize)]
struct Pco2Expected {
    pco2_uatm: f64,
}

#[test]
fn test_pco2_fixtures() {
    let json = include_str!("fixtures/pco2.json");
    let fixture: Fixture<Pco2Case> = serde_json::from_str(json).expect("parse pco2.json");
    let constants = GasConstants::default();

    for case in &fixture.cases {
        let result = match case.function.as_str() {
            "pco2_from_co2aq" => pco2_from_co2aq(
                case.inputs.co2_aq_umol,
                case.inputs.water_temp_c,
                &constants,
            ),
            "pco2_p1" => pco2_p1(
                case.inputs.co2_aq_umol,
                case.inputs.water_temp_c,
                case.inputs.pressure_hpa.unwrap(),
                &constants,
            ),
            "pco2_p2" => pco2_p2(
                case.inputs.co2_aq_umol,
                case.inputs.water_temp_c,
                case.inputs.pressure_hpa.unwrap(),
                &constants,
            ),
            f => panic!("unknown pco2 function: {f}"),
        };

        let diff = (result - case.expected.pco2_uatm).abs();
        assert!(
            diff < case.tolerance,
            "[pco2:{}] expected {:.4}, got {:.4} (diff {:.6} > tol {})",
            case.name, case.expected.pco2_uatm, result, diff, case.tolerance
        );
    }
}

// ---- DIC ----

#[derive(Deserialize)]
struct DicFixture {
    #[allow(dead_code)]
    tool: String,
    constants: DicFixtureConstants,
    cases: Vec<DicCase>,
}

#[derive(Deserialize)]
struct DicFixtureConstants {
    h_co2_29815k: f64,
    gas_const_r_mol: f64,
    vial_volume: f64,
    h3po4_added: f64,
}

#[derive(Deserialize)]
struct DicCase {
    name: String,
    function: String,
    inputs: DicInputs,
    expected: serde_json::Value,
    tolerance: f64,
}

#[derive(Deserialize)]
struct DicInputs {
    acid_sample_weight_g: f64,
    acid_weight_g: f64,
    vol_overpressure_ml: f64,
    sa_added_ml: Option<f64>,
    co2_dry_ppm: Option<f64>,
    d13co2_permil: Option<f64>,
    lab_temp_c: f64,
}

#[test]
fn test_dic_fixtures() {
    let json = include_str!("fixtures/dic.json");
    let fixture: DicFixture = serde_json::from_str(json).expect("parse dic.json");
    let c = &fixture.constants;
    let constants = DICConstants {
        h_co2_29815k: c.h_co2_29815k,
        gas_const_r_mol: c.gas_const_r_mol,
        vial_volume: c.vial_volume,
        h3po4_added: c.h3po4_added,
    };

    for case in &fixture.cases {
        match case.function.as_str() {
            "dic_concentration" => {
                let result = dic_concentration(
                    case.inputs.acid_sample_weight_g,
                    case.inputs.acid_weight_g,
                    case.inputs.vol_overpressure_ml,
                    case.inputs.sa_added_ml.unwrap(),
                    case.inputs.co2_dry_ppm.unwrap(),
                    case.inputs.lab_temp_c,
                    &constants,
                );
                let expected = case.expected["dic_umol_l"].as_f64().unwrap();
                let diff = (result - expected).abs();
                assert!(
                    diff < case.tolerance,
                    "[dic:{}] expected {:.6}, got {:.6} (diff {:.8} > tol {})",
                    case.name, expected, result, diff, case.tolerance
                );
            }
            "d13c_dic" => {
                let result = d13c_dic(
                    case.inputs.acid_sample_weight_g,
                    case.inputs.acid_weight_g,
                    case.inputs.vol_overpressure_ml,
                    case.inputs.d13co2_permil.unwrap(),
                    case.inputs.lab_temp_c,
                    &constants,
                );
                let expected = case.expected["d13c_dic_permil"].as_f64().unwrap();
                let diff = (result - expected).abs();
                assert!(
                    diff < case.tolerance,
                    "[dic:{}] expected {:.4}, got {:.4} (diff {:.6} > tol {})",
                    case.name, expected, result, diff, case.tolerance
                );
            }
            f => panic!("unknown dic function: {f}"),
        }
    }
}

// ---- TSS/AFDM ----

#[derive(Deserialize)]
struct TssAfdmCase {
    name: String,
    inputs: TssAfdmInputs,
    expected: TssAfdmExpected,
    tolerance: f64,
}

#[derive(Deserialize)]
struct TssAfdmInputs {
    wgt_dried_g: f64,
    wgt_prefilt_g: f64,
    wgt_ashed_g: f64,
    vol_filtered_ml: f64,
}

#[derive(Deserialize)]
struct TssAfdmExpected {
    tss_mg_l: f64,
    afdm_mg_l: f64,
    percent_organic: f64,
}

#[test]
fn test_tss_afdm_fixtures() {
    let json = include_str!("fixtures/tss_afdm.json");
    let fixture: Fixture<TssAfdmCase> = serde_json::from_str(json).expect("parse tss_afdm.json");

    for case in &fixture.cases {
        let tss = tss_mg_l(
            case.inputs.wgt_dried_g,
            case.inputs.wgt_prefilt_g,
            case.inputs.vol_filtered_ml,
        );
        let afdm = afdm_mg_l(
            case.inputs.wgt_dried_g,
            case.inputs.wgt_ashed_g,
            case.inputs.vol_filtered_ml,
        );
        let pct = percent_organic(tss, afdm);

        let diff_tss = (tss - case.expected.tss_mg_l).abs();
        let diff_afdm = (afdm - case.expected.afdm_mg_l).abs();
        let diff_pct = (pct - case.expected.percent_organic).abs();

        assert!(
            diff_tss < case.tolerance,
            "[tss_afdm:{}] TSS expected {}, got {} (diff {})",
            case.name, case.expected.tss_mg_l, tss, diff_tss
        );
        assert!(
            diff_afdm < case.tolerance,
            "[tss_afdm:{}] AFDM expected {}, got {} (diff {})",
            case.name, case.expected.afdm_mg_l, afdm, diff_afdm
        );
        assert!(
            diff_pct < case.tolerance,
            "[tss_afdm:{}] %organic expected {}, got {} (diff {})",
            case.name, case.expected.percent_organic, pct, diff_pct
        );
    }
}

// ---- DOC ----

#[derive(Deserialize)]
struct DocCase {
    name: String,
    inputs: DocInputs,
    expected: DocExpected,
    tolerance: f64,
}

#[derive(Deserialize)]
struct DocInputs {
    replicates: Vec<f64>,
    std_curve: Option<StdCurveInput>,
}

#[derive(Deserialize)]
struct StdCurveInput {
    slope: f64,
    intercept: f64,
}

#[derive(Deserialize)]
struct DocExpected {
    doc_average: f64,
    doc_std_dev: f64,
}

#[test]
fn test_doc_fixtures() {
    let json = include_str!("fixtures/doc.json");
    let fixture: Fixture<DocCase> = serde_json::from_str(json).expect("parse doc.json");

    for case in &fixture.cases {
        let curve = case
            .inputs
            .std_curve
            .as_ref()
            .map(|c| (c.slope, c.intercept));
        let avg = doc_average(&case.inputs.replicates, curve);
        let sd = doc_std_dev(&case.inputs.replicates, curve);

        let diff_avg = (avg - case.expected.doc_average).abs();
        let diff_sd = (sd - case.expected.doc_std_dev).abs();

        assert!(
            diff_avg < case.tolerance,
            "[doc:{}] avg expected {}, got {} (diff {})",
            case.name, case.expected.doc_average, avg, diff_avg
        );
        assert!(
            diff_sd < case.tolerance,
            "[doc:{}] sd expected {}, got {} (diff {})",
            case.name, case.expected.doc_std_dev, sd, diff_sd
        );
    }
}

// ---- Chlorophyll ----

#[derive(Deserialize)]
struct ChlorophyllCase {
    name: String,
    function: String,
    inputs: serde_json::Value,
    expected: ChlorophyllExpected,
    tolerance: f64,
}

#[derive(Deserialize)]
struct ChlorophyllExpected {
    chla: f64,
}

#[test]
fn test_chlorophyll_fixtures() {
    let json = include_str!("fixtures/chlorophyll.json");
    let fixture: Fixture<ChlorophyllCase> =
        serde_json::from_str(json).expect("parse chlorophyll.json");

    for case in &fixture.cases {
        let result = match case.function.as_str() {
            "chla_acid" => chla_acid(
                case.inputs["fluor_before"].as_f64().unwrap(),
                case.inputs["fluor_after"].as_f64().unwrap(),
                case.inputs["slope"].as_f64().unwrap(),
                case.inputs["intercept"].as_f64().unwrap(),
            ),
            "chla_no_acid" => chla_no_acid(
                case.inputs["fluorescence"].as_f64().unwrap(),
                case.inputs["slope"].as_f64().unwrap(),
                case.inputs["intercept"].as_f64().unwrap(),
            ),
            f => panic!("unknown chlorophyll function: {f}"),
        };

        let diff = (result - case.expected.chla).abs();
        assert!(
            diff < case.tolerance,
            "[chlorophyll:{}] expected {}, got {} (diff {})",
            case.name, case.expected.chla, result, diff
        );
    }
}

// ---- Ions ----

#[derive(Deserialize)]
struct IonsCase {
    name: String,
    inputs: IonsInputs,
    expected: IonsExpected,
    tolerance: f64,
}

#[derive(Deserialize)]
struct IonsInputs {
    cations: Vec<(String, f64)>,
    anions: Vec<(String, f64)>,
}

#[derive(Deserialize)]
struct IonsExpected {
    sum_cations_meq: f64,
    sum_anions_meq: f64,
    balance_percent: f64,
}

#[test]
fn test_ions_fixtures() {
    let json = include_str!("fixtures/ions.json");
    let fixture: Fixture<IonsCase> = serde_json::from_str(json).expect("parse ions.json");

    for case in &fixture.cases {
        let cat_refs: Vec<(&str, f64)> = case
            .inputs
            .cations
            .iter()
            .map(|(n, c)| (n.as_str(), *c))
            .collect();
        let an_refs: Vec<(&str, f64)> = case
            .inputs
            .anions
            .iter()
            .map(|(n, c)| (n.as_str(), *c))
            .collect();

        let result = charge_balance(&cat_refs, &an_refs);

        let diff_cat = (result.sum_cations_meq - case.expected.sum_cations_meq).abs();
        let diff_an = (result.sum_anions_meq - case.expected.sum_anions_meq).abs();
        let diff_bal = (result.balance_percent - case.expected.balance_percent).abs();

        assert!(
            diff_cat < case.tolerance,
            "[ions:{}] cations expected {}, got {} (diff {})",
            case.name, case.expected.sum_cations_meq, result.sum_cations_meq, diff_cat
        );
        assert!(
            diff_an < case.tolerance,
            "[ions:{}] anions expected {}, got {} (diff {})",
            case.name, case.expected.sum_anions_meq, result.sum_anions_meq, diff_an
        );
        assert!(
            diff_bal < case.tolerance,
            "[ions:{}] balance expected {}%, got {}% (diff {})",
            case.name, case.expected.balance_percent, result.balance_percent, diff_bal
        );
    }
}
