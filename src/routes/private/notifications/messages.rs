//! Rendering of alarm notifications into channel-agnostic text.

use std::fmt::Write as _;

use super::OutgoingMessage;

/// One breach/resolution to describe in a message, resolved to human-readable names.
#[derive(Clone, Debug)]
pub struct PendingEvent {
    pub site_name: String,
    pub parameter_name: String,
    pub units: Option<String>,
    pub severity: i16,
    pub value: f64,
}

#[must_use]
pub fn severity_label(severity: i16) -> &'static str {
    match severity {
        2 => "ALARM",
        1 => "WARNING",
        _ => "INFO",
    }
}

fn unit_suffix(units: Option<&str>) -> String {
    match units {
        Some(u) if !u.is_empty() => format!(" {u}"),
        _ => String::new(),
    }
}

#[must_use]
pub fn render_opened(events: &[PendingEvent], dashboard_base: Option<&str>) -> OutgoingMessage {
    let subject = format!("River Data alarm: {} active", events.len());
    let mut body = format!("🔴 Alarm, {} active\n", events.len());
    for e in events {
        let _ = writeln!(
            body,
            "{} / {}: {:.2}{} ({})",
            e.site_name,
            e.parameter_name,
            e.value,
            unit_suffix(e.units.as_deref()),
            severity_label(e.severity)
        );
    }
    if let Some(base) = dashboard_base {
        let _ = write!(body, "View: {}/alarms", base.trim_end_matches('/'));
    }
    OutgoingMessage {
        kind: "alarm_opened",
        subject,
        body,
        slot: None,
    }
}

#[must_use]
pub fn render_resolved(events: &[PendingEvent], dashboard_base: Option<&str>) -> OutgoingMessage {
    let subject = format!("River Data resolved: {}", events.len());
    let mut body = format!("✅ Resolved, {}\n", events.len());
    for e in events {
        let _ = writeln!(body, "{} / {} is back in range", e.site_name, e.parameter_name);
    }
    if let Some(base) = dashboard_base {
        let _ = write!(body, "View: {}/alarms", base.trim_end_matches('/'));
    }
    OutgoingMessage {
        kind: "alarm_resolved",
        subject,
        body,
        slot: None,
    }
}

#[cfg(test)]
mod tests {
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
        let events = vec![event("Martigny", "Depth", 2, 2150.0), event("Saxon", "CDOM", 1, 140.0)];
        let msg = render_opened(&events, Some("https://dash.example/"));
        assert_eq!(msg.kind, "alarm_opened");
        assert_eq!(msg.subject, "River Data alarm: 2 active");
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
}
