//! Scenario: an author saves a calculation whose output code is already a catalog column, one a
//! decommissioned calculation published or one the portal computed (Q299).
//!
//! Expected behaviour: the save is refused with the column and what computed it until the author
//! confirms; confirmed, the formula continues the column and the takeover is on the change audit.
//! A typed column, a live calculation's output and a unit mismatch stay refused.

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_unprepared(sql).await.unwrap();
}

async fn text(db: &DatabaseConnection, sql: &str, values: Vec<sea_orm::Value>) -> Option<String> {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        sql,
        values,
    ))
    .await
    .unwrap()
    .and_then(|r| r.try_get_by_index::<Option<String>>(0).unwrap())
}

async fn save(
    app: &axum::Router,
    token: &str,
    calculation: Uuid,
    code: &str,
    units: &str,
    take_over: &[&str],
) -> (u16, serde_json::Value) {
    crate::common::post_json_parse_with_token(
        app,
        &format!("/api/tool_scripts/{calculation}/formulas"),
        &json!({
            "formulas": [{ "code": code, "units": units, "formula": "Dissolved_O2 * 2", "ordinal": 1 }],
            "take_over": take_over,
        }),
        token,
    )
    .await
}

async fn output_of(db: &DatabaseConnection, calculation: Uuid, code: &str) -> Option<String> {
    text(
        db,
        "SELECT output_parameter_id::text FROM calculation_formulas WHERE tool_script_id = $1 AND code = $2",
        vec![calculation.into(), code.into()],
    )
    .await
}

async fn takeover_audit(db: &DatabaseConnection, parameter: Uuid) -> Option<String> {
    text(
        db,
        "SELECT new_value::text FROM change_audit WHERE subject = $1 AND change = 'parameter_takeover'",
        vec![format!("parameter:{parameter}").into()],
    )
    .await
}

#[tokio::test]
#[serial]
async fn test_a_portal_computed_column_is_taken_over_once_confirmed() {
    let f = crate::common::seeded_app().await;
    let stamp = Uuid::new_v4().simple().to_string()[..8].to_string();
    let code = format!("CO2_HS_Um_{stamp}");
    let parameter = Uuid::new_v4();
    exec(&f.db, &format!(
        "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES ('{parameter}', '{code}', 'CO2 headspace', 'umol/L', 'measurement');
         INSERT INTO parameter_groups (id, code, label, ordinal) VALUES ('{g}', 'pco2_{stamp}', 'pCO2 {stamp}', 1);
         INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal, source_calculation) \
         VALUES (gen_random_uuid(), '{g}', '{parameter}', 1, '{{\"function\": \"calcCO2\", \"inputs\": [\"lab_co2_co2ppm\"]}}')",
        g = Uuid::new_v4(),
    )).await;
    let calculation =
        crate::common::seed_formula_calculation(&f.db, &format!("pco2real_{stamp}")).await;

    let (status, body) = save(&f.app, &f.token, calculation, &code, "umol/L", &[]).await;
    assert_eq!(status, 409, "{body}");
    let offered = &body["detail"]["take_over"][0];
    assert_eq!(offered["code"], code.as_str(), "{body}");
    assert_eq!(offered["kind"], "portal");
    assert_eq!(offered["computed_by"], "calcCO2");
    assert_eq!(offered["readings"], 0);
    assert!(
        output_of(&f.db, calculation, &code).await.is_none(),
        "a refused save writes nothing"
    );

    let (status, body) = save(&f.app, &f.token, calculation, &code, "ppm", &[&code]).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body.to_string().contains("umol/L") && body.to_string().contains("ppm"),
        "{body}"
    );

    let (status, body) = save(&f.app, &f.token, calculation, &code, "umol/L", &[&code]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["taken_over"], json!([code]));
    assert_eq!(
        output_of(&f.db, calculation, &code).await,
        Some(parameter.to_string())
    );
    let audit = takeover_audit(&f.db, parameter)
        .await
        .expect("the takeover is audited");
    assert!(audit.contains(&format!("pco2real_{stamp}")), "{audit}");
    let name = text(
        &f.db,
        "SELECT name FROM parameters WHERE id = $1",
        vec![parameter.into()],
    )
    .await;
    assert_eq!(
        name.as_deref(),
        Some("CO2 headspace"),
        "the catalog row keeps its name"
    );

    // The portal's value before the takeover and a later one both say the series changed hands.
    let stream = Uuid::new_v4();
    exec(
        &f.db,
        &format!(
            "INSERT INTO data_streams (id, source_system, source_key, is_active) \
             VALUES ('{stream}', 'cnet', 'takeover_{stamp}', true);
             INSERT INTO readings (stream_id, parameter_id, time, replicate_index, raw_value, measurement_type) \
             VALUES ('{stream}', '{parameter}', '2021-03-02T10:00:00Z', 0, 12.5, 'spot'),
                    ('{stream}', '{parameter}', '2027-03-02T10:00:00Z', 0, 13.5, 'spot')"
        ),
    )
    .await;
    for time in ["2021-03-02T10:00:00Z", "2027-03-02T10:00:00Z"] {
        let (status, body) = crate::common::get_json_with_token(
            &f.app,
            &format!("/api/readings/provenance?stream_id={stream}&time={time}"),
            &f.token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let takeover = &body["takeovers"][0];
        assert_eq!(
            takeover["calculation"],
            format!("pco2real_{stamp}"),
            "{time}: {body}"
        );
        assert_eq!(takeover["kind"], "portal");
        assert_eq!(takeover["computed_by"], "calcCO2");
    }
}

#[tokio::test]
#[serial]
async fn test_a_decommissioned_calculation_s_column_is_taken_over_once_confirmed() {
    let f = crate::common::seeded_app().await;
    let stamp = Uuid::new_v4().simple().to_string()[..8].to_string();
    let code = format!("pco2_out_{stamp}");
    let old = crate::common::seed_formula_calculation(&f.db, &format!("pco2_old_{stamp}")).await;
    let (status, body) = save(&f.app, &f.token, old, &code, "uatm", &[]).await;
    assert_eq!(status, 200, "{body}");
    let parameter: Uuid = output_of(&f.db, old, &code).await.unwrap().parse().unwrap();

    let live = crate::common::seed_formula_calculation(&f.db, &format!("pco2_live_{stamp}")).await;
    let (status, body) = save(&f.app, &f.token, live, &code, "uatm", &[&code]).await;
    assert_eq!(
        status, 400,
        "a live calculation's output is refused, confirmed or not: {body}"
    );

    exec(&f.db, &format!("UPDATE tool_scripts SET decommissioned_at = now(), decommissioned_by = 'admin', decommission_reason = 'Replaced' WHERE id = '{old}'")).await;
    let (status, body) = save(&f.app, &f.token, live, &code, "uatm", &[]).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["detail"]["take_over"][0]["kind"], "decommissioned");
    assert_eq!(
        body["detail"]["take_over"][0]["computed_by"],
        format!("pco2_old_{stamp}")
    );

    let (status, body) = save(&f.app, &f.token, live, &code, "uatm", &[&code]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        output_of(&f.db, live, &code).await,
        Some(parameter.to_string())
    );
    let kept = text(
        &f.db,
        "SELECT code FROM calculation_formulas WHERE tool_script_id = $1",
        vec![old.into()],
    )
    .await
    .unwrap();
    assert!(kept.starts_with(&format!("{code}~taken-over-")), "{kept}");
    assert!(takeover_audit(&f.db, parameter).await.is_some());
}

#[tokio::test]
#[serial]
async fn test_a_typed_column_is_never_taken_over() {
    let f = crate::common::seeded_app().await;
    let stamp = Uuid::new_v4().simple().to_string()[..8].to_string();
    let code = format!("typed_{stamp}");
    exec(
        &f.db,
        &format!(
            "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES (gen_random_uuid(), '{code}', 'Typed', 'mg/L', 'measurement')"
        ),
    )
    .await;
    let calculation =
        crate::common::seed_formula_calculation(&f.db, &format!("typed_calc_{stamp}")).await;
    let (status, body) = save(&f.app, &f.token, calculation, &code, "mg/L", &[&code]).await;
    assert!(status == 409 || status == 400, "refused ({status}): {body}");
    assert!(
        body["detail"]["take_over"].is_null(),
        "no takeover is offered: {body}"
    );
}

/// A catalog column the portal averaged with `calcMean`, bound at site 1 to a family stream whose
/// repeats were computed by `member_calculations`, or typed when it is null.
async fn seed_family(db: &DatabaseConnection, code: &str, member_calculations: &str) {
    let (parameter, group, slot) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    exec(db, &format!(
        "INSERT INTO parameters (id, code, name, default_units, category) \
         VALUES ('{parameter}', '{code}', 'Family mean', 'umol/L', 'measurement');
         INSERT INTO parameter_groups (id, code, label, ordinal) VALUES ('{group}', 'g_{code}', 'g {code}', 1);
         INSERT INTO parameter_group_members (id, group_id, parameter_id, ordinal, source_calculation) \
         VALUES (gen_random_uuid(), '{group}', '{parameter}', 1, '{{\"function\": \"calcMean\"}}');
         INSERT INTO site_parameters (id, site_id, parameter_id, name) \
         VALUES ('{slot}', '{site}', '{parameter}', '{code}');
         INSERT INTO data_streams (id, source_system, source_key, is_active, site_parameter_id, metadata) \
         VALUES (gen_random_uuid(), 'cnet', 'S1:{code}:reps', true, '{slot}', \
           '{{\"replicates\": {{\"source_columns\": [\"{code}_A\", \"{code}_B\"]}}, \
              \"replicate_family\": {{\"members\": [\"{code}_A\", \"{code}_B\"], \
                                     \"member_calculations\": {member_calculations}}}}}')",
        site = crate::common::SITE1_ID,
    )).await;
}

#[tokio::test]
#[serial]
async fn test_a_replicate_family_is_taken_over_only_when_its_repeats_were_computed() {
    let f = crate::common::seeded_app().await;
    let stamp = Uuid::new_v4().simple().to_string()[..8].to_string();
    let computed = format!("CO2_HS_Um_{stamp}");
    seed_family(
        &f.db,
        &computed,
        r#"[{"function": "calcCO2"}, {"function": "calcCO2"}]"#,
    )
    .await;
    let typed = format!("Reach_depth_{stamp}");
    seed_family(&f.db, &typed, "null").await;
    let calculation =
        crate::common::seed_formula_calculation(&f.db, &format!("family_calc_{stamp}")).await;

    let (status, body) = save(&f.app, &f.token, calculation, &computed, "umol/L", &[]).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["detail"]["take_over"][0]["kind"], "portal");
    assert_eq!(
        body["detail"]["take_over"][0]["computed_by"], "calcCO2",
        "the repeats' calculation, not the mean's: {body}"
    );

    let (status, body) = save(&f.app, &f.token, calculation, &typed, "umol/L", &[&typed]).await;
    assert!(status == 409 || status == 400, "refused ({status}): {body}");
    assert!(
        body["detail"]["take_over"].is_null(),
        "typed repeats are never offered: {body}"
    );
}
