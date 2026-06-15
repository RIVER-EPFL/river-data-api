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
    let mut body = format!("🔴 Alarm — {} active\n", events.len());
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
    }
}

#[must_use]
pub fn render_resolved(events: &[PendingEvent], dashboard_base: Option<&str>) -> OutgoingMessage {
    let subject = format!("River Data resolved: {}", events.len());
    let mut body = format!("✅ Resolved — {}\n", events.len());
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
    }
}
