//! Scenario: the shared history of a computed value, walked along every path that can move one of
//! its sources (I118, the audit of group `shared-history`).
//!
//! Expected behaviour: a record names what its calculation consumed and marks each input
//! `changed`, `unchanged` or `unknown` against the revision the source stands at now. A source
//! that moved and moved back is still `changed`, because the revision advanced; a row computed
//! before the capture existed is `unknown` and is not confused with one captured at its arrival
//! state, whose revision is null on both sides.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde_json::json;
use serial_test::serial;
use uuid::Uuid;

use crate::common::{GLOBAL_PARAM_DO_ID, SITE1_ID};

const POLL_SECS: u64 = 30;

struct Fixture {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
    /// The state the router shares, so a test can mint a sync credential the way the control
    /// plane's own admin route does.
    state: river_db::common::AppState,
    /// The formula, the calculation that owns it, its output parameter, and the instant the
    /// visit sits at.
    code: String,
    definition: String,
    calculation: String,
    output: String,
    at: DateTime<Utc>,
}

/// A calculation reading `Dissolved_O2`, its output assigned at site 1, and one input reading.
///
/// `constants` are created first: a formula variable resolves against the catalog at create, so a
/// constant the formula names has to exist before it.
async fn computed_slot(formula: &str, constants: &[(&str, f64)]) -> Fixture {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_token_full(&db).await;
    let (app, state) = crate::common::build_test_app_with_state(db.clone());
    let f = Prepared {
        db,
        app,
        token,
        state,
    };
    for (name, value) in constants {
        let (status, body) = crate::common::post_json_with_token(
            &f.app,
            "/api/constants",
            &json!({ "name": name, "value": value, "description": "I118" }),
            &f.token,
        )
        .await;
        assert!((200..300).contains(&status), "constant ({status}): {body}");
    }
    let code = format!("i118_{}", Uuid::new_v4().simple());
    let calculation = crate::common::seed_formula_calculation(&f.db, &format!("{code}_set")).await;
    let (status, def) = crate::common::post_json_parse_with_token(
        &f.app,
        "/api/derived_parameters",
        &json!({ "code": code, "name": "I118 fixture", "units": "mg/L", "formula": formula,
                 "tool_script_id": calculation }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "create ({status}): {def}");
    crate::common::commit_calculation(&f.db, calculation).await;
    let output = def["output_parameter_id"]
        .as_str()
        .expect("output")
        .to_string();
    let definition = def["id"].as_str().expect("id").to_string();

    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        "/api/site_parameters",
        &json!({
            "site_id": SITE1_ID,
            "parameter_id": output,
            "name": code,
            "sensor_type": "derived",
            "entry_mode": "tool",
        }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "assign ({status}): {body}");

    let at = Utc::now() - Duration::hours(6);
    let at = at - Duration::nanoseconds(i64::from(at.timestamp_subsec_nanos()));
    Fixture {
        db: f.db,
        app: f.app,
        token: f.token,
        state: f.state,
        code,
        definition,
        calculation: calculation.to_string(),
        output,
        at,
    }
}

/// The seeded application before the calculation is declared on it.
struct Prepared {
    db: DatabaseConnection,
    app: axum::Router,
    token: String,
    state: river_db::common::AppState,
}

impl Fixture {
    async fn write_input(&self, value: f64, conflict: Option<&str>) {
        let mut body = json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "time": self.at.to_rfc3339(),
                "raw_value": value,
            }]
        });
        if let Some(mode) = conflict {
            body["conflict"] = json!(mode);
        }
        let (status, out) = crate::common::post_json_with_token(
            &self.app,
            "/api/readings/batch",
            &body,
            &self.token,
        )
        .await;
        assert!((200..300).contains(&status), "write ({status}): {out}");
    }

    fn uri(&self) -> String {
        format!(
            "/api/readings/provenance?site_id={SITE1_ID}&parameter_id={}&time={}",
            self.output,
            self.at.to_rfc3339().replace('+', "%2B")
        )
    }

    /// The output's record, polled until the compute that captures its inputs has landed.
    async fn consumed(&self) -> Vec<serde_json::Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_SECS);
        loop {
            let (status, body) =
                crate::common::get_json_with_token(&self.app, &self.uri(), &self.token).await;
            let set = body["records"][0]["consumed"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if !set.is_empty() || std::time::Instant::now() >= deadline {
                assert_eq!(status, 200, "{body}");
                return set;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// The record's consumed set as it stands, without waiting for anything.
    async fn consumed_now(&self) -> Vec<serde_json::Value> {
        let (status, body) =
            crate::common::get_json_with_token(&self.app, &self.uri(), &self.token).await;
        assert_eq!(status, 200, "{body}");
        body["records"][0]["consumed"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The ledger row the record names as behind its capture.
    async fn captured_by(&self) -> serde_json::Value {
        let (status, body) =
            crate::common::get_json_with_token(&self.app, &self.uri(), &self.token).await;
        assert_eq!(status, 200, "{body}");
        body["records"][0]["captured_by"].clone()
    }

    /// The newest computation the ledger holds at the output's key, as the record serves one.
    async fn newest_computation(&self) -> serde_json::Value {
        let row = self
            .db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT id, seq, kind, job_id FROM reading_decisions \
                   WHERE stream_id = $1 AND time = $2 \
                     AND kind IN ('formula_transition', 'derived_computed') \
                   ORDER BY seq DESC LIMIT 1",
                [self.output_stream().await.into(), self.at.into()],
            ))
            .await
            .expect("the query runs")
            .expect("a computation on the ledger");
        json!({
            "id": row.try_get::<Uuid>("", "id").expect("id"),
            "seq": row.try_get::<i64>("", "seq").expect("seq"),
            "kind": row.try_get::<String>("", "kind").expect("kind"),
            "job_id": row.try_get::<Option<Uuid>>("", "job_id").expect("job_id"),
        })
    }

    /// The stream the output is stored on.
    async fn output_stream(&self) -> Uuid {
        self.db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT stream_id FROM readings WHERE time = $1 AND measurement_type = 'derived' \
                   ORDER BY stream_id LIMIT 1",
                [self.at.into()],
            ))
            .await
            .expect("the query runs")
            .expect("a derived row")
            .try_get::<Uuid>("", "stream_id")
            .expect("stream_id")
    }

    async fn stored_output(&self) -> Option<f64> {
        self.db
            .query_one_raw(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Postgres,
                "SELECT raw_value FROM readings WHERE parameter_id = $1::uuid AND time = $2",
                [self.output.clone().into(), self.at.into()],
            ))
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<Option<f64>>("", "raw_value").ok())
            .flatten()
    }

    /// Wait until the output holds `value`, so a recompute's effect is read rather than raced.
    async fn wait_for_output(&self, value: f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_SECS);
        while std::time::Instant::now() < deadline {
            if self.stored_output().await == Some(value) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        panic!(
            "the output never reached {value}; it holds {:?}",
            self.stored_output().await
        );
    }
}

impl Fixture {
    /// A sync session token, the only credential `/ingest {overwrite: true}` accepts. Minted the
    /// way the control plane's admin route mints one, then enrolled over the router.
    async fn session_token(&self) -> String {
        use axum::Json;
        use axum::extract::State;
        use river_db::routes::private::sync::models::CreateCredentialRequest;
        use river_db::routes::private::sync::views::create_credential;

        let Json(minted) = create_credential(
            State(self.state.clone()),
            Json(CreateCredentialRequest {
                service_type: "cnet".to_string(),
            }),
        )
        .await
        .expect("mint an enrolment credential");
        let (status, body) = crate::common::post_json(
            &self.app,
            "/api/sync/enroll",
            &json!({
                "client_id": minted.client_id,
                "client_secret": minted.client_secret,
                "instance_id": "i118",
            }),
        )
        .await;
        assert!((200..300).contains(&status), "enroll ({status}): {body}");
        serde_json::from_str::<serde_json::Value>(&body)
            .expect("the enrolment answers json")["session_token"]
            .as_str()
            .expect("a session token")
            .to_string()
    }
}

fn entry<'a>(set: &'a [serde_json::Value], variable: &str) -> &'a serde_json::Value {
    set.iter()
        .find(|c| c["variable"] == variable)
        .unwrap_or_else(|| panic!("{variable} was consumed: {set:?}"))
}

#[tokio::test]
#[serial]
async fn a_correction_recomputes_the_output_and_the_record_follows_the_new_reading() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    let input = entry(&set, "Dissolved_O2");
    assert_eq!(input["state"], json!("unchanged"), "{input}");
    assert!(
        input["revision"].is_null(),
        "arrival state on both sides: {input}"
    );
    assert_eq!(input["members"][0]["value"].as_f64(), Some(10.0));

    // Correcting an input is a change to the readings, so the chain recomputes the output and
    // captures again. The record then names the corrected reading, not the one it first read:
    // the value on display was computed from 11, and saying `changed` would be wrong.
    f.write_input(11.0, Some("overwrite")).await;
    f.wait_for_output(22.0).await;
    let after = f.consumed_now().await;
    let input = entry(&after, "Dissolved_O2");
    assert_eq!(input["members"][0]["value"].as_f64(), Some(11.0), "{input}");
    assert_eq!(input["state"], json!("unchanged"), "{input}");
    assert!(
        input["members"][0]["revision"].as_i64().is_some(),
        "the correction is a decision, so the reading now has a revision: {input}"
    );
}

/// Scenario: an output is computed, then recomputed after its input is corrected.
///
/// Expected behaviour: the record names the ledger row behind the capture it shows, once beside
/// it: the first computation, then the recompute that moved the value (Q251).
#[tokio::test]
#[serial]
async fn the_record_names_the_ledger_row_behind_its_capture() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    f.consumed().await;
    let first = f.captured_by().await;
    assert_eq!(first, f.newest_computation().await);

    f.write_input(11.0, Some("overwrite")).await;
    f.wait_for_output(22.0).await;
    let moved = f.captured_by().await;
    assert_eq!(moved, f.newest_computation().await);
    assert!(
        moved["seq"].as_i64() > first["seq"].as_i64(),
        "the recompute's row, not the first: {first} then {moved}"
    );
}

#[tokio::test]
#[serial]
async fn a_group_scoped_flag_marks_the_input_and_then_takes_the_output_off_its_slot() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    assert!(!f.consumed().await.is_empty(), "the capture landed");
    let stream = f.output_stream().await;

    // A key with no replicate index is the whole group. The revision expression reads such a
    // decision at every index of the key, so the input it covers reads changed.
    let (status, body) = crate::common::patch_json_with_token(
        &f.app,
        "/api/readings/flag",
        &json!({
            "readings": [{
                "site_id": SITE1_ID,
                "parameter_id": GLOBAL_PARAM_DO_ID,
                "time": f.at.to_rfc3339(),
                "measurement_type": "continuous",
            }],
            "reason": "I118",
        }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "flag ({status}): {body}");

    let by_stream = format!(
        "/api/readings/provenance?stream_id={stream}&time={}",
        f.at.to_rfc3339().replace('+', "%2B")
    );
    let (status, body) = crate::common::get_json_with_token(&f.app, &by_stream, &f.token).await;
    assert_eq!(status, 200, "{body}");
    let set = body["records"][0]["consumed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let input = entry(&set, "Dissolved_O2");
    assert_eq!(
        input["state"],
        json!("changed"),
        "the flag is a decision on the key it read: {input}"
    );
    assert!(
        input["members"][0]["current_revision"].as_i64().is_some(),
        "a group decision is the key's revision: {input}"
    );

    // The flagged input no longer resolves, so the chain takes the output off the slot. The row
    // and its captured history stand; only the slot stops serving it, and the record is then
    // reachable by the stream alone.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(POLL_SECS);
    loop {
        let (status, _) = crate::common::get_json_with_token(&f.app, &f.uri(), &f.token).await;
        if status == 404 || std::time::Instant::now() >= deadline {
            assert_eq!(
                status, 404,
                "the slot stops serving an output whose input was flagged"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let (status, body) = crate::common::get_json_with_token(&f.app, &by_stream, &f.token).await;
    assert_eq!(
        status, 200,
        "the record is still there, by the stream it is on: {body}"
    );
    assert!(
        !body["records"][0]["consumed"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "and it still names what it consumed: {body}"
    );
}

#[tokio::test]
#[serial]
async fn an_edited_step_reads_as_changed_and_an_unmoved_value_keeps_its_record() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    let step = entry(&set, &f.code);
    assert_eq!(step["kind"], json!("step"), "{step}");
    let before = step["revision"]
        .as_i64()
        .expect("the formula has a revision");
    assert_eq!(step["state"], json!("unchanged"));

    // Edit the formula so the value moves, then recompute: the move is captured against the new
    // step revision, so the record that follows reads unchanged again.
    let (status, body) = crate::common::put_json_with_token(
        &f.app,
        &format!("/api/derived_parameters/{}", f.definition),
        &json!({ "formula": "Dissolved_O2 * 3" }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "edit ({status}): {body}");
    crate::common::commit_calculation(&f.db, f.calculation.parse().expect("a uuid")).await;
    let uri = format!(
        "/api/actions/derived_parameters/{}/recompute",
        f.calculation
    );
    let (status, body) =
        crate::common::post_json_with_token(&f.app, &uri, &json!({}), &f.token).await;
    assert!((200..300).contains(&status), "recompute ({status}): {body}");
    f.wait_for_output(30.0).await;

    let after = f.consumed_now().await;
    let step = entry(&after, &f.code);
    assert!(
        step["revision"].as_i64().expect("a revision") > before,
        "the edit advanced the step's revision: {step}"
    );
    assert_eq!(
        step["state"],
        json!("unchanged"),
        "captured against the edited step: {step}"
    );

    // A second recompute moves nothing, so it records nothing and the record above stands.
    let (status, body) =
        crate::common::post_json_with_token(&f.app, &uri, &json!({}), &f.token).await;
    assert!((200..300).contains(&status), "recompute ({status}): {body}");
    let again = f.consumed_now().await;
    assert_eq!(
        entry(&again, &f.code)["revision"],
        step["revision"],
        "a pass that moves no value leaves the captured set alone"
    );
}

#[tokio::test]
#[serial]
async fn a_row_computed_before_the_capture_says_its_inputs_are_unknown() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    assert!(!set.is_empty(), "the capture landed");

    // Strip what the capture wrote, which is the state of every row computed before it existed.
    crate::common::exec(
        &f.db,
        "UPDATE reading_decisions SET new = new - 'consumed' \
           WHERE kind IN ('derived_computed', 'formula_transition')",
    )
    .await;
    assert!(
        f.consumed_now().await.is_empty(),
        "an uncaptured computation names no input, rather than guessing one"
    );
}

#[tokio::test]
#[serial]
async fn an_edited_constant_reads_as_changed_beside_the_value_it_now_holds() {
    // A name of its own, because `cleanup_test_db` keeps the seeded constants and a shared name
    // would carry one run's edits into the next.
    let name = format!("i118_k_{}", Uuid::new_v4().simple());
    let f = computed_slot(&format!("Dissolved_O2 * {name}"), &[(&name, 2.0)]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    let k = entry(&set, &name);
    assert_eq!(k["kind"], json!("constant"), "{k}");
    assert_eq!(k["value"].as_f64(), Some(2.0));
    assert_eq!(k["state"], json!("unchanged"), "{k}");
    let subject = k["subject"].as_str().expect("a subject").to_string();
    let id = subject
        .strip_prefix("constant:")
        .expect("constant:<id>")
        .to_string();

    let (status, body) = crate::common::put_json_with_token(
        &f.app,
        &format!("/api/constants/{id}"),
        &json!({ "value": 3.0 }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "edit ({status}): {body}");

    let after = f.consumed_now().await;
    let k = entry(&after, &name);
    assert_eq!(k["state"], json!("changed"), "{k}");
    assert_eq!(k["value"].as_f64(), Some(2.0), "what was read");
    assert_eq!(
        k["current_value"].as_f64(),
        Some(3.0),
        "what the catalog holds"
    );
    let stream_repairs = crate::common::e2e::count(
        &f.db,
        &format!(
            "SELECT COUNT(*) FROM reprocessing_jobs WHERE trigger_type = 'derived_recompute' \
             AND params->>'calculation_id' = '{}'",
            f.calculation
        ),
    )
    .await;
    assert_eq!(
        stream_repairs, 1,
        "the stream values computed with the old value are recomputed too"
    );
}

#[tokio::test]
#[serial]
async fn an_edited_site_property_reads_as_changed_against_the_column_it_was_read_from() {
    let f = computed_slot("Dissolved_O2 * latitude", &[]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    let property = entry(&set, "latitude");
    assert_eq!(property["kind"], json!("site"), "{property}");
    assert_eq!(property["property"], json!("latitude"), "{property}");
    let read = property["value"].as_f64().expect("the latitude it read");

    let (status, body) = crate::common::put_json_with_token(
        &f.app,
        &format!("/api/sites/{SITE1_ID}"),
        &json!({ "latitude": read + 1.0 }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "edit ({status}): {body}");

    let after = f.consumed_now().await;
    let property = entry(&after, "latitude");
    assert_eq!(property["state"], json!("changed"), "{property}");
    assert_eq!(property["value"].as_f64(), Some(read), "what was read");
    assert_eq!(
        property["current_value"].as_f64(),
        Some(read + 1.0),
        "what the site holds"
    );
}

/// Scenario: two decisions land on one reading key inside a single transaction.
///
/// Expected behaviour: the later `seq` is the key's revision, so a capture taken after both
/// compares against the second and not whichever shared `now()` sorted first (M284).
#[tokio::test]
#[serial]
async fn two_decisions_in_one_transaction_order_by_sequence() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    let member = &entry(&set, "Dissolved_O2")["members"][0];
    let stream = member["stream_id"].as_str().expect("a stream").to_string();

    crate::common::exec(
        &f.db,
        &format!(
            "INSERT INTO reading_decisions (id, stream_id, time, replicate_index, kind, old, new, \
                 actor, at, origin) \
             SELECT gen_random_uuid(), '{stream}'::uuid, '{at}'::timestamptz, 0, k, '{{}}'::jsonb, \
                 '{{}}'::jsonb, 'i118', now(), 'system' \
             FROM unnest(ARRAY['flag', 'unflag']) AS k",
            at = f.at.to_rfc3339()
        ),
    )
    .await;

    let rows =
        f.db.query_all_raw(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT kind, seq FROM reading_decisions \
               WHERE stream_id = $1::uuid AND time = $2 AND actor = 'i118' ORDER BY seq",
            [stream.clone().into(), f.at.into()],
        ))
        .await
        .expect("the two rows");
    let kinds: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<String>("", "kind").unwrap_or_default())
        .collect();
    assert_eq!(
        kinds,
        vec!["flag", "unflag"],
        "the sequence is insertion order"
    );

    let newest = rows
        .last()
        .and_then(|r| r.try_get::<i64>("", "seq").ok())
        .expect("the later sequence");
    let after = f.consumed_now().await;
    assert_eq!(
        entry(&after, "Dissolved_O2")["members"][0]["current_revision"].as_i64(),
        Some(newest),
        "the key's revision is the later decision, not the earlier one"
    );
}

/// Scenario: a source re-asserts a changed value through the windowed ingest, the write path a
/// sync session token gates.
///
/// Expected behaviour: the derived output is recomputed and captures again, so its record names
/// the overwritten reading at `unchanged`, exactly as the batch arm does (T131).
#[tokio::test]
#[serial]
async fn an_overwrite_through_ingest_is_recaptured_like_one_through_batch() {
    let f = computed_slot("Dissolved_O2 * 2", &[]).await;
    f.write_input(10.0, None).await;
    let set = f.consumed().await;
    let stream = entry(&set, "Dissolved_O2")["members"][0]["stream_id"]
        .as_str()
        .expect("a stream")
        .to_string();

    let session = f.session_token().await;
    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        "/api/ingest",
        &json!({
            "stream_id": stream,
            "overwrite": true,
            "readings": [{ "time": f.at.to_rfc3339(), "raw_value": 12.0 }],
        }),
        &session,
    )
    .await;
    assert!((200..300).contains(&status), "ingest ({status}): {body}");
    f.wait_for_output(24.0).await;

    let after = f.consumed_now().await;
    let input = entry(&after, "Dissolved_O2");
    assert_eq!(input["members"][0]["value"].as_f64(), Some(12.0), "{input}");
    assert_eq!(input["state"], json!("unchanged"), "{input}");
}
