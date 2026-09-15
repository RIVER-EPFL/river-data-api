use super::*;

/// Every kind the triggers emit has a channel to decline it on. A kind no channel answers
/// for reaches every enabled recipient with no way to decline, which is what M89 closed and
/// M163 kept when the group became the kind.
#[test]
fn test_every_emitted_kind_has_a_channel() {
    for kind in emitted_kinds() {
        assert!(
            channel(kind).is_some(),
            "'{kind}' is emitted and belongs to no channel, so it is delivered to everyone"
        );
    }
    assert!(
        on_by_default("alarm_opened"),
        "the audience nobody opted into"
    );
    assert!(on_by_default("alarm_resolved"));
    for kind in [
        "battery_forecast",
        "stale_data",
        "sync_stale",
        "sync_failure",
        "streams_unpaired",
        "holds_open",
        "job_failed",
        "changes_pending",
        "curve_drift",
        "derived_computed",
        "steps_skipped",
    ] {
        assert!(!on_by_default(kind), "'{kind}' is asked for, not assumed");
    }
    assert!(
        channel("test").is_none(),
        "the test send addresses whoever asked"
    );
}

/// The list above is only as good as its completeness, so it is held to the sources: every
/// `kind:` a message is built with is either an emitted kind or the unaddressed test send.
/// A new kind added to a trigger fails here rather than reaching everyone silently.
#[test]
fn test_every_kind_a_message_carries_is_listed() {
    const SOURCES: [&str; 3] = [
        include_str!("../flows.rs"),
        include_str!("../service.rs"),
        include_str!("../views.rs"),
    ];
    for source in SOURCES {
        for tail in source.split("kind: \"").skip(1) {
            let kind = tail.split('"').next().unwrap_or_default();
            assert!(
                kind == UNADDRESSED_KIND || emitted_kinds().contains(&kind),
                "a message carries kind '{kind}', which is on neither a channel nor the \
                 unaddressed test send, so nothing maps it to an audience"
            );
        }
    }
}

#[test]
fn test_channel_names_are_the_kinds_and_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for c in CHANNELS {
        assert!(seen.insert(c.kind), "'{}' is declared twice", c.kind);
        assert_eq!(channel(c.kind), Some(&c));
        assert!(!c.label.is_empty() && !c.description.is_empty());
    }
}
