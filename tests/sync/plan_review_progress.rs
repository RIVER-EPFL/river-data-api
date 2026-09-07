//! What the review counts as decided.
//!
//! Expected behaviour: a fully matched entry with no warning stands on its own evidence and waits
//! on nobody; an unmatched entry or one carrying a warning waits on a person; a tick settles one
//! entry and only the entries it names. The plan's summary carries the three counts, so the review
//! can say what share of a 1891-entry plan still wants attention.

use serial_test::serial;
use uuid::Uuid;

const SOURCE: &str = "reviewsrc";

async fn setup() -> (axum::Router, String, sea_orm::DatabaseConnection) {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;
    let token = crate::common::seed_api_token(&db, crate::common::full_permissions(), None).await;
    let app = crate::common::build_test_app(db.clone());
    (app, token, db)
}

fn entry_for(plan: &serde_json::Value, stream_id: Uuid) -> serde_json::Value {
    plan["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["stream_id"] == serde_json::json!(stream_id))
        .cloned()
        .unwrap_or_else(|| panic!("no entry for {stream_id}"))
}

#[tokio::test]
#[serial]
async fn a_tick_settles_the_entries_it_names_and_no_others() {
    let (app, token, db) = setup().await;

    let ticked = Uuid::new_v4();
    let untouched = Uuid::new_v4();
    for (id, key) in [(ticked, "rev-a"), (untouched, "rev-b")] {
        crate::common::seed_unpaired_stream_with_hierarchy(
            &db,
            &id.to_string(),
            SOURCE,
            key,
            "Brand New Project",
            "Brand New Station",
            "brand_new_parameter",
            "ppb",
            None,
            0,
        )
        .await;
    }

    let (status, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": SOURCE }),
        &token,
    )
    .await;
    assert_eq!(status, 200, "create plan ({status}): {plan}");
    let plan_id = plan["id"].as_str().expect("plan id").to_string();
    assert_eq!(
        plan["summary"]["needs_checking"],
        serde_json::json!(2),
        "neither entry resolves, so both wait on a person: {}",
        plan["summary"]
    );
    assert_eq!(plan["summary"]["acknowledged"], serde_json::json!(0));

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "expected_version": plan["version"],
            "updates": [{ "stream_id": ticked, "acknowledged": true }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "tick ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).expect("plan json");

    assert_eq!(
        entry_for(&plan, ticked)["acknowledged"],
        serde_json::json!(true)
    );
    assert_eq!(
        entry_for(&plan, untouched)["acknowledged"],
        serde_json::json!(false),
        "a tick on one entry decides nothing about its neighbour"
    );
    assert_eq!(plan["summary"]["acknowledged"], serde_json::json!(1));
    assert_eq!(plan["summary"]["needs_checking"], serde_json::json!(1));

    // Taking it back is the same control, so a mis-click is undoable.
    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "expected_version": plan["version"],
            "updates": [{ "stream_id": ticked, "acknowledged": false }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "untick ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).expect("plan json");
    assert_eq!(plan["summary"]["acknowledged"], serde_json::json!(0));
    assert_eq!(plan["summary"]["needs_checking"], serde_json::json!(2));

    crate::common::cleanup_test_db(&db).await;
}

/// An edit is not a decision: changing what an entry does leaves it as undecided as it was.
#[tokio::test]
#[serial]
async fn renaming_an_entry_does_not_decide_it() {
    let (app, token, db) = setup().await;

    let stream = Uuid::new_v4();
    crate::common::seed_unpaired_stream_with_hierarchy(
        &db,
        &stream.to_string(),
        SOURCE,
        "rev-c",
        "Brand New Project",
        "Brand New Station",
        "brand_new_parameter",
        "ppb",
        None,
        0,
    )
    .await;

    let (_, plan) = crate::common::post_json_parse_with_token(
        &app,
        "/api/sync/pairing-plans",
        &serde_json::json!({ "source_system": SOURCE }),
        &token,
    )
    .await;
    let plan_id = plan["id"].as_str().expect("plan id").to_string();

    let (status, body) = crate::common::patch_plan_with_token(
        &app,
        &plan_id,
        &serde_json::json!({
            "expected_version": plan["version"],
            "updates": [{ "stream_id": stream, "site_name": "Another Station" }],
        }),
        &token,
    )
    .await;
    assert!((200..300).contains(&status), "rename ({status}): {body}");
    let plan: serde_json::Value = serde_json::from_str(&body).expect("plan json");
    assert_eq!(
        entry_for(&plan, stream)["acknowledged"],
        serde_json::json!(false),
        "the entry was edited, not agreed with"
    );
    assert_eq!(plan["summary"]["acknowledged"], serde_json::json!(0));

    crate::common::cleanup_test_db(&db).await;
}
