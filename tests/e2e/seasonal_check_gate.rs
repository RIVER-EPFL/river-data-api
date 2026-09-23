//! S3, the seasonal check gate (story catalog: ../archived-documentation/PLAN.md).
//!
//! Scenario: a member enters a value that is an outlier for the season. Check screens it against
//! the site's multi-year distribution (entry month ±2 across all years, replicates pooled,
//! min/Q10/Q90/max) and answers with an advisory warning and the distribution payload. The save
//! proceeds (the warning gates nothing by force), but it is held to the check it names: values
//! edited after the check are refused until re-checked. Authorization follows the documented
//! layers: an intern may check and may enter a field measurement, which lands unverified, but may
//! not replace a stored one (Q21, M44).
//!
//! The quantile arithmetic, the cyclic month window and the fixed above-max classification are
//! pinned in `tests/readings/seasonal_check.rs`; this story runs the workflow end to end against
//! real Keycloak identities.

use serde_json::json;
use serial_test::serial;

use crate::common::e2e;
use crate::common::keycloak as kc;
use crate::common::tracks;

#[tokio::test]
#[serial]
async fn an_outlier_warns_and_the_save_is_held_to_its_check() {
    if !crate::common::profile::Service::Keycloak
        .require("an_outlier_warns_and_the_save_is_held_to_its_check")
        .await
    {
        return;
    }
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    let app = kc::build_test_app_with_keycloak(db.clone()).await;
    let admin = kc::get_keycloak_jwt("admin", "admin").await;

    let track = tracks::onboard_grab_track(&app, &admin).await;
    let parameter_id = track.parameter_id("TrkGrabDoc").to_string();

    for (user, role) in [
        ("intern1", "riverdata-intern"),
        ("river1", "riverdata-river"),
    ] {
        kc::ensure_realm_user(user, user, &[role]).await;
        kc::grant_project(&db, &kc::keycloak_user_id(user).await, &track.project_id).await;
    }
    let intern = kc::get_keycloak_jwt("intern1", "intern1").await;
    let river = kc::get_keycloak_jwt("river1", "river1").await;

    // Multi-year June history, replicate plates pooled.
    for (at, values) in [
        ("2022-06-10T10:00:00Z", &[100.0, 104.0, 102.0][..]),
        ("2023-06-12T10:00:00Z", &[98.0, 101.0][..]),
        ("2024-07-01T10:00:00Z", &[105.0, 99.0][..]),
    ] {
        let readings: Vec<serde_json::Value> = values
            .iter()
            .map(|v| json!({ "parameter_id": parameter_id, "value": v, "time": at }))
            .collect();
        let (status, body) = crate::common::post_json_with_token(
            &app,
            "/api/grab_samples",
            &json!({ "site_id": track.site_id, "readings": readings }),
            &river,
        )
        .await;
        assert_eq!(status, 200, "seed {at}: {body}");
    }

    // The intern screens the entry: reading the distribution is open to any member.
    let (status, check) = crate::common::post_json_parse_with_token(
        &app,
        "/api/readings/seasonal_check",
        &json!({
            "site_id": track.site_id,
            "time": "2025-06-20T09:30:00Z",
            "values": [{ "parameter_id": parameter_id, "value": 240.0 }],
        }),
        &intern,
    )
    .await;
    assert_eq!(status, 200, "check ({status}): {check}");
    let finding = &check["findings"][0];
    assert_eq!(finding["class"], "above_max", "{check}");
    assert_eq!(finding["warning"], true);
    assert_eq!(finding["n"], 7, "the pooled seasonal population: {check}");
    assert!(
        finding["distribution"]
            .as_array()
            .is_some_and(|d| d.len() == 7),
        "the distribution payload feeds the plot: {check}"
    );
    let check_id = check["check_id"].as_str().expect("check id").to_string();

    let save = json!({
        "site_id": track.site_id,
        "check_id": check_id,
        "readings": [{ "parameter_id": parameter_id, "value": 240.0, "time": "2025-06-20T09:30:00Z" }],
    });

    // An intern enters a measurement of their own, at their own instant, and it lands unverified
    // (Q21, M44). A value inside the season's range needs no check to name.
    let intern_entry = json!({
        "site_id": track.site_id,
        "readings": [{ "parameter_id": parameter_id, "value": 12.0, "time": "2025-06-20T10:30:00Z" }],
    });
    let (status, entered) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &intern_entry, &intern)
            .await;
    assert_eq!(status, 200, "an intern enters a measurement: {entered}");
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS n FROM readings \
                 WHERE site_id = '{}' AND time = '2025-06-20T10:30:00Z' AND unverified",
                track.site_id
            ),
        )
        .await,
        1,
        "and it lands unverified, which is what nothing publishes"
    );

    // What an intern may not do is rewrite a stored value.
    let mut rewrite = intern_entry.clone();
    rewrite["mode"] = json!("replace");
    rewrite["readings"][0]["value"] = json!(12.5);
    let (status, refused) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &rewrite, &intern).await;
    assert_eq!(
        status, 403,
        "an intern's entry cannot replace stored values: {refused}"
    );

    // A second repeat of the same measurement is an entry, not a rewrite (B295). The grid posts
    // the whole replicate group, so the stored repeat travels with it at the number it already
    // holds and the intern's new one lands beside it.
    let mut second_repeat = intern_entry.clone();
    second_repeat["mode"] = json!("replace");
    second_repeat["readings"] = json!([
        { "parameter_id": parameter_id, "value": 12.0, "time": "2025-06-20T10:30:00Z" },
        { "parameter_id": parameter_id, "value": 13.5, "time": "2025-06-20T10:30:00Z" },
    ]);
    let (status, added) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &second_repeat, &intern)
            .await;
    assert_eq!(status, 200, "an intern enters a second repeat: {added}");
    assert_eq!(
        e2e::count(
            &db,
            &format!(
                "SELECT COUNT(*)::bigint AS n FROM readings \
                 WHERE site_id = '{}' AND time = '2025-06-20T10:30:00Z' AND unverified",
                track.site_id
            ),
        )
        .await,
        2,
        "both repeats are the intern's, so both are unverified"
    );

    // An edit after the check must re-check: the gate.
    let mut edited = save.clone();
    edited["readings"][0]["value"] = json!(260.0);
    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &edited, &river).await;
    assert_eq!(status, 409, "edited after checking: {body}");

    let (status, body) =
        crate::common::post_json_with_token(&app, "/api/grab_samples", &save, &river).await;
    assert_eq!(status, 200, "the checked outlier saves (advisory): {body}");

    // The saved visit is a collection event, so the outlier stays addressable.
    let n = e2e::count(
        &db,
        &format!(
            "SELECT COUNT(*)::bigint AS n FROM readings r \
             JOIN collection_events ce ON ce.id = r.collection_event_id \
             WHERE ce.site_id = '{}' AND r.time = '2025-06-20T09:30:00Z'",
            track.site_id
        ),
    )
    .await;
    assert_eq!(n, 1);
}
