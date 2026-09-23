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

fn message(kind: &'static str, slot: Option<Slot>, key: Option<&str>) -> OutgoingMessage {
    OutgoingMessage {
        kind,
        key: key.map(str::to_string),
        subject: String::new(),
        body: String::new(),
        slot,
    }
}

fn slot(site: u128, parameter: u128) -> Option<Slot> {
    Some(Slot {
        project_id: None,
        site_id: Uuid::from_u128(site),
        parameter_id: Uuid::from_u128(parameter),
    })
}

#[test]
fn test_push_tag_keeps_two_slots_apart() {
    let martigny = push_tag(&message("alarm_opened", slot(1, 2), None));
    let saxon = push_tag(&message("alarm_opened", slot(3, 4), None));
    assert_ne!(martigny, saxon);
    assert_eq!(
        martigny,
        push_tag(&message("alarm_opened", slot(1, 2), None)),
        "a repeat for the same slot replaces the earlier one"
    );
    assert_ne!(
        push_tag(&message("stale_data", slot(1, 2), None)),
        push_tag(&message("stale_data", slot(3, 2), None))
    );
}

#[test]
fn test_push_tag_keeps_a_system_wide_kind_apart_by_its_key() {
    assert_ne!(
        push_tag(&message("job_failed", None, Some("csv_import"))),
        push_tag(&message("job_failed", None, Some("reprocess")))
    );
    assert_eq!(
        push_tag(&message("holds_open", None, None)),
        "holds_open",
        "a digest with no key replaces the previous digest of its kind"
    );
}

const VAPID_PEM: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIFIDkW07GbdXLEk+WYBSLCxOPqERyJhe5GaQ0l5+cHVroAoGCCqGSM49
AwEHoUQDQgAEs7xIexoeDMgSdTnUZo2llWmbVprDGe3oaTDOqUHVIjXGirsD1LP7
6Dg2DoqE4mu64mwJX3FFt3usr4fIr+HpLg==
-----END EC PRIVATE KEY-----
";
const VAPID_PUBLIC: &str =
    "BLO8SHsaHgzIEnU51GaNpZVpm1aawxnt6GkwzqlB1SI1xoq7A9Sz--g4Ng6KhOJruuJsCV9xRbd7rK-HyK_h6S4";
const OTHER_PUBLIC: &str =
    "BOepCtGG1gIV-EseUVZcq785P7H5A2XbFxKHr62ijdyz0pTlZGimNjg3pQ65BR213VQGgV8hi4g5Lw4kvbHbX5k";

fn vapid_config(pem: &str, public_key: &str) -> Config {
    Config {
        vapid_private_key_pem: Some(pem.to_string()),
        vapid_public_key: Some(public_key.to_string()),
        vapid_subject: Some("mailto:river@example.org".to_string()),
        ..Config::for_openapi_document()
    }
}

#[test]
fn test_vapid_key_accepts_the_public_half_of_its_pem() {
    assert!(vapid_key(VAPID_PEM, VAPID_PUBLIC).is_ok());
    assert!(
        vapid_key(VAPID_PEM, &format!("{VAPID_PUBLIC}=")).is_ok(),
        "padding is not part of the key"
    );
}

#[test]
fn test_vapid_key_refuses_a_public_key_from_another_keypair() {
    let err = vapid_key(VAPID_PEM, OTHER_PUBLIC)
        .err()
        .expect("mismatch refused");
    assert!(err.contains("VAPID_PUBLIC_KEY"), "{err}");
}

#[test]
fn test_vapid_key_refuses_a_mangled_pem() {
    let mangled = VAPID_PEM.replace('\n', "\\n");
    let err = vapid_key(&mangled, VAPID_PUBLIC)
        .err()
        .expect("mangled PEM refused");
    assert!(err.contains("VAPID_PRIVATE_KEY_PEM"), "{err}");
}

#[tokio::test]
async fn test_web_push_health_reports_an_unusable_keypair() {
    let healthy = WebPushChannel::new(&vapid_config(VAPID_PEM, VAPID_PUBLIC)).expect("configured");
    assert!(healthy.check_health().await.is_ok());
    let mismatched =
        WebPushChannel::new(&vapid_config(VAPID_PEM, OTHER_PUBLIC)).expect("configured");
    let err = mismatched.check_health().await.expect_err("unhealthy");
    assert!(err.contains("VAPID_PUBLIC_KEY"), "{err}");
}
