//! Scenario: a calculation reads one input from a stream and one the lab measures at a visit. The
//! second is declared held, so the binder takes the last value measured at or before the instant
//! rather than one at it (Q230).
//!
//! Expected behaviour: a source is written reading its input at the instant; a person declares one
//! held through the source's own route; the declaration survives the next save of the formula,
//! which rewrites every source row; and no value but the two the rule names may be stored.
//!
//! What the declaration then does is the binder's, tested where the statement is built.
//!
//! Run with: cargo test --test derived_parameters source_alignment

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serial_test::serial;
use uuid::Uuid;

async fn setup() -> (DatabaseConnection, axum::Router, String) {
    let f = crate::common::seeded_app().await;
    (f.db, f.app, f.token)
}

/// A one-formula calculation reading the seeded oxygen series, and the source row it wrote.
async fn define(app: &axum::Router, db: &DatabaseConnection, token: &str) -> (Uuid, Uuid) {
    let code = format!("align_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(db, &format!("{code}_set")).await;
    let (status, body) = crate::common::post_json_parse_with_token(
        app,
        "/api/derived_parameters",
        &serde_json::json!({
            "code": code,
            "name": "Alignment test mg/L",
            "units": "mg/L",
            "formula": "Dissolved_O2 * 0.032",
            "tool_script_id": calculation,
        }),
        token,
    )
    .await;
    assert!((200..300).contains(&status), "{body}");
    let definition = Uuid::parse_str(body["id"].as_str().expect("an id")).expect("a uuid");
    let source = body["sources"]
        .as_array()
        .and_then(|s| s.first())
        .cloned()
        .expect("the save wrote the source it resolved");
    assert_eq!(
        source["alignment"], "exact",
        "a source reads its input at the instant until somebody says otherwise: {source}"
    );
    let source_id = Uuid::parse_str(source["id"].as_str().expect("an id")).expect("a uuid");
    (definition, source_id)
}

async fn alignment_of(db: &DatabaseConnection, definition: Uuid) -> String {
    db.query_one_raw(Statement::from_sql_and_values(
        sea_orm::DatabaseBackend::Postgres,
        "SELECT alignment FROM derived_parameter_sources WHERE derived_definition_id = $1",
        [definition.into()],
    ))
    .await
    .expect("the row reads")
    .expect("the definition has a source")
    .try_get::<String>("", "alignment")
    .expect("the column reads")
}

#[tokio::test]
#[serial]
async fn a_source_declared_held_keeps_it_when_the_formula_is_saved_again() {
    let (db, app, token) = setup().await;
    let (definition, source_id) = define(&app, &db, &token).await;

    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/derived_parameter_sources/{source_id}"),
        &serde_json::json!({ "alignment": "hold" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
    assert_eq!(alignment_of(&db, definition).await, "hold");

    // The save rewrites every source row of the definition, and the declaration is about the
    // input rather than about the text that reads it.
    let (status, body) = crate::common::put_json_with_token(
        &app,
        &format!("/api/derived_parameters/{definition}"),
        &serde_json::json!({ "formula": "Dissolved_O2 * 0.064" }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
    assert_eq!(
        alignment_of(&db, definition).await,
        "hold",
        "the rewrite carried the declaration onto the variable's new row"
    );
}

#[tokio::test]
#[serial]
async fn nothing_but_the_two_rules_may_be_stored() {
    let (db, app, token) = setup().await;
    let (definition, _) = define(&app, &db, &token).await;

    let refused = db
        .execute_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "UPDATE derived_parameter_sources SET alignment = 'nearest' \
             WHERE derived_definition_id = $1",
            [definition.into()],
        ))
        .await;
    assert!(
        refused.is_err(),
        "a rule the binder cannot carry out is not a state the table may hold"
    );
}

/// Scenario: a calculation on a site's stream reads the oxygen series exactly and one value the
/// lab measured at a visit, declared held (Q230).
///
/// Expected behaviour: the output stands at the stream's own instants, carrying the held value at
/// each of them, and the visit's instant mints no output of its own: a held source stands between
/// visits, so counting its instants would put a pulse off the stream's grid holding the numbers of
/// the pulse beside it.
mod at_a_site {
    use chrono::{DateTime, Duration, Utc};
    use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
    use serial_test::serial;
    use uuid::Uuid;

    const DEADLINE_SECS: u64 = 30;
    const OXYGEN: f64 = 250.0;
    const ALKALINITY: f64 = 7.0;

    async fn value_at(
        db: &DatabaseConnection,
        parameter_id: Uuid,
        time: DateTime<Utc>,
    ) -> Option<f64> {
        db.query_one_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT COALESCE(calibrated_value, raw_value) AS value FROM readings \
             WHERE site_id = $1 AND parameter_id = $2 AND time = $3 LIMIT 1",
            [
                Uuid::parse_str(crate::common::SITE1_ID)
                    .expect("a uuid")
                    .into(),
                parameter_id.into(),
                time.into(),
            ],
        ))
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<f64>("", "value").ok())
    }

    async fn poll_for(
        db: &DatabaseConnection,
        parameter_id: Uuid,
        time: DateTime<Utc>,
    ) -> Option<f64> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(DEADLINE_SECS);
        while std::time::Instant::now() < deadline {
            if let Some(value) = value_at(db, parameter_id, time).await {
                return Some(value);
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        None
    }

    /// An instant with no fractional seconds, which is what a stored reading key compares as.
    fn whole(at: DateTime<Utc>) -> DateTime<Utc> {
        at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()))
    }

    /// A calculation on the site's stream reading the oxygen series at the instant and a lab value
    /// held from the last visit, and the site's slots for both. Returns the lab parameter and the
    /// output.
    async fn held_calculation(
        db: &DatabaseConnection,
        app: &axum::Router,
        token: &str,
        lab_code: &str,
        stamp: &str,
    ) -> (String, Uuid) {
        // The value the lab measures at a visit, and the slot it is recorded at: the visit arm.
        let (status, lab) = crate::common::post_json_parse_with_token(
            app,
            "/api/parameters",
            &serde_json::json!({
                "code": lab_code,
                "name": lab_code,
                "category": "measurement",
                "aliases": [],
            }),
            token,
        )
        .await;
        assert!((200..300).contains(&status), "{lab}");
        let lab_id = lab["id"].as_str().expect("an id").to_string();
        let (status, text) = crate::common::post_json_with_token(
            app,
            "/api/site_parameters",
            &serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "parameter_id": lab_id,
                "name": lab_code,
                "cadence": "low",
            }),
            token,
        )
        .await;
        assert!((200..300).contains(&status), "{text}");

        // The calculation: the oxygen series read at the instant, the lab value held.
        let code = format!("held_out_{stamp}");
        let calculation = crate::common::seed_formula_calculation(db, &format!("{code}_set")).await;
        let (status, definition) = crate::common::post_json_parse_with_token(
            app,
            "/api/derived_parameters",
            &serde_json::json!({
                "code": code,
                "name": "Held source test",
                "units": "mg/L",
                "formula": format!("Dissolved_O2 * 0.032 + {lab_code}"),
                "tool_script_id": calculation,
            }),
            token,
        )
        .await;
        assert!((200..300).contains(&status), "{definition}");
        let output_id = definition["output_parameter_id"]
            .as_str()
            .expect("an output");
        let output = Uuid::parse_str(output_id).expect("a uuid");

        let source = definition["sources"]
            .as_array()
            .expect("the sources it resolved")
            .iter()
            .find(|s| s["variable_name"] == lab_code)
            .expect("the lab value is one of them")["id"]
            .as_str()
            .expect("an id")
            .to_string();
        let (status, text) = crate::common::put_json_with_token(
            app,
            &format!("/api/derived_parameter_sources/{source}"),
            &serde_json::json!({ "alignment": "hold" }),
            token,
        )
        .await;
        assert!((200..300).contains(&status), "{status}: {text}");

        // The output is the stream's to compute, which is the slot's own declaration.
        let (status, text) = crate::common::post_json_with_token(
            app,
            "/api/site_parameters",
            &serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "parameter_id": output_id,
                "name": code,
                "sensor_type": "derived",
                "entry_mode": "tool",
                "cadence": "high",
            }),
            token,
        )
        .await;
        assert!((200..300).contains(&status), "{text}");

        (lab_id, output)
    }

    #[tokio::test]
    #[serial]
    async fn a_held_source_stands_at_the_stream_s_instants_and_mints_none_of_its_own() {
        let f = crate::common::seeded_app().await;
        let (db, app, token) = (f.db, f.app, f.token);
        let stamp = Uuid::new_v4().simple().to_string();
        let lab_code = format!("alk_{stamp}");
        let (lab_id, output) = held_calculation(&db, &app, &token, &lab_code, &stamp).await;

        // The visit, four hours before the pulse.
        let visit = whole(Utc::now() - Duration::hours(40));
        let (status, text) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "mode": "replace",
                "readings": [{
                    "parameter_id": lab_id,
                    "value": ALKALINITY,
                    "time": visit.to_rfc3339(),
                    "replicate_index": 0,
                }],
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "{text}");

        // The pulse.
        let pulse = whole(Utc::now() - Duration::hours(36));
        let (status, text) = crate::common::post_json_with_token(
            &app,
            "/api/readings/batch",
            &serde_json::json!({
                "readings": [{
                    "site_id": crate::common::SITE1_ID,
                    "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                    "time": pulse.to_rfc3339(),
                    "raw_value": OXYGEN,
                }],
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "{text}");

        let expected = OXYGEN * 0.032 + ALKALINITY;
        let at_pulse = poll_for(&db, output, pulse)
            .await
            .unwrap_or_else(|| panic!("no output at the pulse within {DEADLINE_SECS}s"));
        assert!(
            (at_pulse - expected).abs() < 1e-6,
            "the pulse carries the value the visit measured: expected {expected}, got {at_pulse}"
        );

        // The record says which rule reached the held reading, and names the visit's instant
        // rather than the pulse's, so a reader can tell a held value from a mis-stamped one.
        let uri = format!(
            "/api/readings/provenance?site_id={}&parameter_id={output}&time={}",
            crate::common::SITE1_ID,
            pulse.to_rfc3339().replace('+', "%2B")
        );
        let (status, body) = crate::common::get_json_with_token(&app, &uri, &token).await;
        assert_eq!(status, 200, "{body}");
        let inputs = body["records"][0]["consumed"]
            .as_array()
            .unwrap_or_else(|| panic!("the record names what it consumed: {body}"));
        let alkalinity = inputs
            .iter()
            .find(|i| i["variable"] == lab_code.as_str())
            .unwrap_or_else(|| panic!("the held input is one of them: {body}"));
        assert_eq!(alkalinity["alignment"], "hold", "{body}");
        assert_eq!(
            alkalinity["members"][0]["time"],
            serde_json::json!(visit),
            "the member stands at the visit that measured it: {body}"
        );
        let oxygen = inputs
            .iter()
            .find(|i| i["variable"] == "Dissolved_O2")
            .unwrap_or_else(|| panic!("the stream input is one of them: {body}"));
        assert!(
            oxygen.get("alignment").is_none(),
            "an input read at the instant says nothing: {body}"
        );

        assert!(
            value_at(&db, output, visit).await.is_none(),
            "the visit's own instant is no pulse of the stream, so the set computes nothing there"
        );
        let findings = db
            .query_all_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT group_time FROM replicate_audit_holds \
                 WHERE kind = 'skipped_output' AND parameter_id = $1",
                [output.into()],
            ))
            .await
            .expect("the queue reads");
        assert!(
            findings.is_empty(),
            "the visit's instant was never the set's to compute, so nothing is held for review"
        );
    }
    /// The output's pending state at `time`, and the decisions of `kind` on it.
    async fn output_state(
        db: &DatabaseConnection,
        output: Uuid,
        time: DateTime<Utc>,
        kind: &str,
    ) -> (bool, i64) {
        let row = db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT r.unverified, (SELECT count(*) FROM reading_decisions d \
                   WHERE d.stream_id = r.stream_id AND d.time = r.time AND d.kind = $3) AS n \
                 FROM readings r WHERE r.parameter_id = $1 AND r.time = $2",
                [output.into(), time.into(), kind.into()],
            ))
            .await
            .expect("the row reads")
            .expect("the output stands");
        (
            row.try_get("", "unverified").expect("unverified"),
            row.try_get("", "n").expect("count"),
        )
    }

    /// Scenario: the lab value a continuous calculation holds was entered by an intern and awaits
    /// verification.
    ///
    /// Expected behaviour: the output is computed and held pending, on the record, so nothing
    /// serves it; a manager's verify of the entry releases it (Q257).
    #[tokio::test]
    #[serial]
    async fn a_value_held_from_a_pending_entry_is_pending_until_the_entry_is_verified() {
        let f = crate::common::seeded_app().await;
        let (db, app, token) = (f.db, f.app, f.token);
        let stamp = Uuid::new_v4().simple().to_string();
        let lab_code = format!("alk_{stamp}");
        let (lab_id, output) = held_calculation(&db, &app, &token, &lab_code, &stamp).await;

        let visit = whole(Utc::now() - Duration::hours(40));
        let (status, text) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &serde_json::json!({
                "site_id": crate::common::SITE1_ID,
                "mode": "replace",
                "readings": [{
                    "parameter_id": lab_id,
                    "value": ALKALINITY,
                    "time": visit.to_rfc3339(),
                    "replicate_index": 0,
                }],
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "{text}");
        // The entry lands pending, as an intern's does.
        crate::common::exec(
            &db,
            &format!(
                "UPDATE readings SET unverified = true \
                 WHERE parameter_id = '{lab_id}' AND time = '{}'",
                visit.to_rfc3339()
            ),
        )
        .await;

        let pulse = whole(Utc::now() - Duration::hours(36));
        let (status, text) = crate::common::post_json_with_token(
            &app,
            "/api/readings/batch",
            &serde_json::json!({
                "readings": [{
                    "site_id": crate::common::SITE1_ID,
                    "parameter_id": crate::common::GLOBAL_PARAM_DO_ID,
                    "time": pulse.to_rfc3339(),
                    "raw_value": OXYGEN,
                }],
            }),
            &token,
        )
        .await;
        assert!((200..300).contains(&status), "{text}");
        poll_for(&db, output, pulse)
            .await
            .unwrap_or_else(|| panic!("no output at the pulse within {DEADLINE_SECS}s"));
        assert_eq!(
            output_state(&db, output, pulse, "unverified_entry").await,
            (true, 1),
            "the output computed from a pending entry is pending itself, on the record"
        );

        let hold = Uuid::new_v4();
        crate::common::exec(
            &db,
            &format!(
                "INSERT INTO replicate_audit_holds (id, site_id, parameter_id, group_time, kind, \
                     expected, computed, delta, status) \
                 VALUES ('{hold}', '{}', '{lab_id}', '{}', 'unverified_entry', '{{}}', '{{}}', \
                         '{{}}', 'pending')",
                crate::common::SITE1_ID,
                visit.to_rfc3339()
            ),
        )
        .await;
        let (status, body) = crate::common::post_json_with_token(
            &app,
            &format!("/api/sync/replicate_audit_holds/{hold}/resolve"),
            &serde_json::json!({ "mode": "verify" }),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            output_state(&db, output, pulse, "verify").await,
            (false, 1),
            "the verify of its only pending input releases the output, on the record"
        );
    }
}
