use super::*;

fn event(site: &str, param: &str, severity: i16, value: f64) -> PendingEvent {
    PendingEvent {
        site_name: site.to_string(),
        parameter_name: param.to_string(),
        units: Some("mm".to_string()),
        severity,
        value,
    }
}

#[test]
fn test_severity_label_maps_known_levels() {
    assert_eq!(severity_label(2), "ALARM");
    assert_eq!(severity_label(1), "WARNING");
    assert_eq!(severity_label(0), "INFO");
}

#[test]
fn test_render_opened_lists_each_event_and_links() {
    let events = vec![
        event("Martigny", "Depth", 2, 2150.0),
        event("Saxon", "CDOM", 1, 140.0),
    ];
    let msg = render_opened(&events, Some("https://dash.example/"));
    assert_eq!(msg.kind, "alarm_opened");
    assert_eq!(msg.subject, "RIVER Data alarm: 2 active");
    assert!(msg.body.contains("Martigny / Depth: 2150.00 mm (ALARM)"));
    assert!(msg.body.contains("Saxon / CDOM: 140.00 mm (WARNING)"));
    // Trailing slash on the base is normalized.
    assert!(msg.body.contains("View: https://dash.example/admin/alarms"));
}

#[test]
fn test_deep_link_url_is_under_the_ui_base() {
    let site = Uuid::from_u128(1);
    let parameter = Uuid::from_u128(2);
    let slot = Some(Slot {
        project_id: None,
        site_id: site,
        parameter_id: parameter,
    });
    assert_eq!(
        deep_link_url(Some("https://dash.example/"), &slot).as_deref(),
        Some(format!("https://dash.example/admin/sites/{site}?focus={parameter}").as_str())
    );
    assert_eq!(
        deep_link_url(Some("https://dash.example"), &None).as_deref(),
        Some("https://dash.example/admin/alarms")
    );
    assert_eq!(deep_link_url(None, &slot), None);
}

#[test]
fn test_render_resolved_links_under_the_ui_base() {
    let events = vec![event("Verbier", "Dissolved_O2", 2, 5.0)];
    let msg = render_resolved(&events, Some("https://dash.example"));
    assert!(msg.body.contains("View: https://dash.example/admin/alarms"));
}

#[test]
fn test_render_resolved_without_dashboard_has_no_link() {
    let events = vec![event("Verbier", "Dissolved_O2", 2, 5.0)];
    let msg = render_resolved(&events, None);
    assert_eq!(msg.kind, "alarm_resolved");
    assert!(msg.body.contains("Verbier / Dissolved_O2 is back in range"));
    assert!(!msg.body.contains("View:"));
}

#[test]
fn test_render_import_tags_groups_by_source_and_kind() {
    let counts = vec![
        (Some("cnet".to_string()), "replicate_stats".to_string(), 12),
        (
            Some("metalp".to_string()),
            "curve_claim_stripped".to_string(),
            1,
        ),
        (None, "replicate_stats".to_string(), 2),
    ];
    let msg = render_import_tags(&counts).expect("tags were recorded");
    assert_eq!(msg.kind, "import_tags");
    // 12 + 1 + 2
    assert_eq!(
        msg.subject,
        "RIVER Data: 15 discrepancy tag(s) recorded at import"
    );
    assert!(
        msg.body.contains("12 replicate_stats from cnet"),
        "{}",
        msg.body
    );
    assert!(
        msg.body.contains("1 curve_claim_stripped from metalp"),
        "{}",
        msg.body
    );
    assert!(
        msg.body.contains("2 replicate_stats from no stream"),
        "{}",
        msg.body
    );
    assert!(msg.slot.is_none());
}

#[test]
fn test_render_import_tags_empty_sends_nothing() {
    assert!(render_import_tags(&[]).is_none());
}
