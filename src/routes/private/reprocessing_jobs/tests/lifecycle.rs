use super::JobReport;

#[test]
fn a_report_serialises_to_scope_and_counts_with_numeric_counts() {
    let value = JobReport::new()
        .scope("full_refresh", true)
        .scope("source_system", "cnet")
        .scope_opt("site_id", None::<String>)
        .count("pruned", 3i64)
        .count("recomposed", 7usize)
        .to_value();

    assert_eq!(
        value.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["counts", "scope"]
    );
    assert_eq!(value["scope"]["full_refresh"], serde_json::json!(true));
    assert_eq!(value["scope"]["source_system"], serde_json::json!("cnet"));
    assert!(value["scope"].get("site_id").is_none());
    for (key, count) in value["counts"].as_object().unwrap() {
        assert!(count.is_number(), "{key} is not a number: {count}");
    }
    assert_eq!(value["counts"]["pruned"], serde_json::json!(3));
    assert_eq!(value["counts"]["recomposed"], serde_json::json!(7));
}
