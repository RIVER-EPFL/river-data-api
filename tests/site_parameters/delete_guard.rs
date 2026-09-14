//! Scenario: a site stops measuring a parameter. Retirement is the Active toggle; Remove is for a
//! slot paired by mistake. Expected behaviour: deleting a slot that holds readings is refused with
//! the count, because the delete unattributes every reading the slot carried (Q160).

use serial_test::serial;

#[tokio::test]
#[serial]
async fn a_slot_that_holds_readings_cannot_be_deleted() {
    let f = crate::common::seeded_app().await;

    let (status, text) = crate::common::delete_with_token(
        &f.app,
        &format!("/api/site_parameters/{}", crate::common::PARAM_S1_DEPTH_ID),
        &f.token,
    )
    .await;

    assert_eq!(status, 400, "a measured slot is refused: {text}");
    assert!(
        text.contains("Active"),
        "the refusal points at the toggle that retires: {text}"
    );

    let (gstatus, _) = crate::common::get_with_token(
        &f.app,
        &format!("/api/site_parameters/{}", crate::common::PARAM_S1_DEPTH_ID),
        &f.token,
    )
    .await;
    assert_eq!(gstatus, 200, "the slot is still there");
}

#[tokio::test]
#[serial]
async fn a_slot_that_measured_nothing_still_deletes() {
    let f = crate::common::seeded_app().await;

    let body = serde_json::json!({
        "site_id": crate::common::SITE2_ID,
        "parameter_id": crate::common::GLOBAL_PARAM_DEPTH_ID,
    });
    let (status, text) =
        crate::common::post_json_with_token(&f.app, "/api/site_parameters", &body, &f.token).await;
    assert!((200..300).contains(&status), "create: {status} {text}");
    let created: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let id = created["id"].as_str().expect("id");

    let (dstatus, dtext) =
        crate::common::delete_with_token(&f.app, &format!("/api/site_parameters/{id}"), &f.token)
            .await;
    assert!(
        (200..300).contains(&dstatus),
        "an unmeasured slot deletes: {dstatus} {dtext}"
    );
}
