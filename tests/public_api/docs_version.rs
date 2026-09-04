//! The version a partner reads on `GET /api/public/{code}/docs` is the serving contract the code
//! implements, unless the project pins one. The pin overrides what the docs advertise; it never
//! changes what is served, so the spec carries the contract under `x-serving-contract` beside
//! `info.version` and names the pin under `x-project-version` when one is set.
//!
//! Run: cargo test --test public_api docs_version -- --test-threads=1

use serial_test::serial;

use river_db::routes::public::service::SERVING_CONTRACT_VERSION;

const DOCS_URI: &str = "/api/public/test-river/docs";

async fn setup(pinned_version: Option<&str>) -> axum::Router {
    let db = crate::common::setup_test_db().await;
    crate::common::cleanup_test_db(&db).await;
    crate::common::seed_test_data(&db).await;

    let pin = pinned_version.map_or("NULL".to_string(), |v| format!("'{v}'"));
    crate::common::exec(
        &db,
        &format!(
            "UPDATE projects SET is_public = true, public_code = 'test-river', \
             public_api_version = {pin} WHERE id = '{}'",
            crate::common::PROJECT_ID
        ),
    )
    .await;
    crate::common::build_test_app(db)
}

/// The OpenAPI document embedded in the served docs page.
fn spec_of(html: &str) -> serde_json::Value {
    let start_tag = "<script id=\"api-reference\" type=\"application/json\">";
    let start = html
        .find(start_tag)
        .unwrap_or_else(|| panic!("spec script missing: {html}"))
        + start_tag.len();
    let end = html[start..]
        .find("</script>")
        .unwrap_or_else(|| panic!("spec script unterminated: {html}"));
    serde_json::from_str(&html[start..start + end])
        .unwrap_or_else(|e| panic!("spec is not JSON: {e}"))
}

#[tokio::test]
#[serial]
async fn docs_report_the_serving_contract_when_the_project_pins_nothing() {
    let app = setup(None).await;
    let (status, html) = crate::common::get(&app, DOCS_URI).await;
    assert_eq!(status, 200, "{html}");
    let spec = spec_of(&html);

    assert_eq!(spec["info"]["version"], SERVING_CONTRACT_VERSION);
    assert_eq!(spec["info"]["x-serving-contract"], SERVING_CONTRACT_VERSION);
    assert!(
        spec["info"].get("x-project-version").is_none(),
        "no pin, no project version: {}",
        spec["info"]
    );
}

#[tokio::test]
#[serial]
async fn docs_carry_the_contract_and_the_project_pin_distinctly() {
    let app = setup(Some("1.1.0")).await;
    let (status, html) = crate::common::get(&app, DOCS_URI).await;
    assert_eq!(status, 200, "{html}");
    let spec = spec_of(&html);

    assert_eq!(
        spec["info"]["version"], "1.1.0",
        "the pin is what the docs advertise"
    );
    assert_eq!(spec["info"]["x-project-version"], "1.1.0");
    assert_eq!(
        spec["info"]["x-serving-contract"], SERVING_CONTRACT_VERSION,
        "the pin does not move the contract the code serves"
    );
    assert_ne!(SERVING_CONTRACT_VERSION, "1.1.0");
}
