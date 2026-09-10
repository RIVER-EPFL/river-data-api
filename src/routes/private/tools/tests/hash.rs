use crate::routes::private::tools::service::version_content_hash;

#[test]
fn an_equivalent_manifest_hashes_equal_whatever_order_it_was_written_in() {
    let one = serde_json::json!({ "label": "T", "constants": ["a", "b"], "params": [] });
    let other = serde_json::json!({ "params": [], "label": "T", "constants": ["a", "b"] });
    let cases = serde_json::json!({});
    assert_eq!(
        version_content_hash("s", "tool", &one, &cases),
        version_content_hash("s", "tool", &other, &cases)
    );
    let relabelled = serde_json::json!({ "label": "U", "constants": ["a", "b"], "params": [] });
    assert_ne!(
        version_content_hash("s", "tool", &one, &cases),
        version_content_hash("s", "tool", &relabelled, &cases)
    );
    assert_ne!(
        version_content_hash("s", "tool", &one, &cases),
        version_content_hash("s", "tool", &one, &serde_json::json!({ "cases": [] }))
    );
}
