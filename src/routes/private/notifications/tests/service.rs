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
    assert!(msg.body.contains("View: https://dash.example/alarms"));
}

#[test]
fn test_render_resolved_without_dashboard_has_no_link() {
    let events = vec![event("Verbier", "Dissolved_O2", 2, 5.0)];
    let msg = render_resolved(&events, None);
    assert_eq!(msg.kind, "alarm_resolved");
    assert!(msg.body.contains("Verbier / Dissolved_O2 is back in range"));
    assert!(!msg.body.contains("View:"));
}
