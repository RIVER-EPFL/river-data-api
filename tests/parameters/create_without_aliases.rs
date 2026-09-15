//! A create body that names no alias: the catalogue accepts it and stores an empty list.

use serde_json::json;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn a_parameter_created_without_aliases_gets_an_empty_list() {
    let f = crate::common::seeded_app().await;

    let (status, body) = crate::common::post_json_with_token(
        &f.app,
        "/api/parameters",
        &json!({
            "code": "hs_co2_ppm",
            "name": "Headspace CO2",
            "default_units": "ppm",
        }),
        &f.token,
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");

    let created: serde_json::Value = serde_json::from_str(&body).expect("valid json");
    assert_eq!(
        created["aliases"],
        json!([]),
        "no alias named means no alias, not a refusal: {created}"
    );
}
