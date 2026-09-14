use super::{InstrumentKind, source_instrument_name};

#[test]
fn test_source_instrument_name_source_parameter_carries_no_site() {
    let name = source_instrument_name(
        InstrumentKind::SourceParameter,
        "DOC_avg_ppb",
        "cnet",
        Some("FP1 DOC_avg_ppb"),
    );
    assert_eq!(name, "DOC_avg_ppb (cnet)");
}

#[test]
fn test_source_instrument_name_lab_carries_no_site() {
    let name = source_instrument_name(InstrumentKind::Lab, "DOC", "cnet", Some("FP1 DOC"));
    assert_eq!(name, "DOC (cnet)");
}

#[test]
fn test_source_instrument_name_entry_channel_keeps_slot_name() {
    let name = source_instrument_name(
        InstrumentKind::EntryChannel,
        "Depth",
        "grab_sample",
        Some("Martigny Depth (grab_sample)"),
    );
    assert_eq!(name, "Martigny Depth (grab_sample)");
}

#[test]
fn test_source_instrument_name_falls_back_without_a_hint() {
    let name = source_instrument_name(InstrumentKind::EntryChannel, "Depth", "api", None);
    assert_eq!(name, "Depth (api)");
}

use super::*;

#[test]
fn claims_a_free_serial() {
    assert_eq!(
        serial_to_claim(Some("919402"), None),
        Some("919402".to_string())
    );
}

#[test]
fn leaves_a_held_serial_alone() {
    assert_eq!(serial_to_claim(Some("919402"), Some(Uuid::nil())), None);
}

#[test]
fn treats_an_absent_or_blank_serial_as_none() {
    assert_eq!(serial_to_claim(None, None), None);
    assert_eq!(serial_to_claim(Some("   "), None), None);
}

#[test]
fn trims_the_claimed_serial() {
    assert_eq!(
        serial_to_claim(Some(" 4000138 "), None),
        Some("4000138".to_string())
    );
}

/// Scenario: the instrument enrichment reads are built rather than written out.
/// Expected behaviour: each keeps the shape that makes it cheap, so a renamed column fails the
/// build and a lost window or `DISTINCT ON` fails here.
#[test]
fn test_holders_of_instrument_asks_all_four_tables() {
    let sql = holders_of_instrument(Uuid::nil()).sql;

    for table in [
        "readings",
        "standard_curves",
        "sensor_calibrations",
        "sensor_deployments",
    ] {
        assert!(
            sql.contains(&format!(
                r#"EXISTS(SELECT $1 FROM "{table}" WHERE "sensor_id" = $"#
            )) || sql.contains(&format!(r#"FROM "{table}""#)),
            "{table} is probed: {sql}"
        );
    }
}

#[test]
fn test_recent_spot_counts_stays_inside_the_ninety_day_window() {
    let sql = recent_spot_counts(&[Uuid::nil()]).sql;

    assert!(sql.contains(r#"FROM "readings""#), "{sql}");
    assert!(sql.contains("time > now() - INTERVAL '90 days'"), "{sql}");
    assert!(sql.contains("is_flagged IS NOT TRUE"), "{sql}");
    assert!(sql.contains(r#"GROUP BY "sensor_id""#), "{sql}");
}

#[test]
fn test_newest_value_per_instrument_takes_one_row_per_instrument() {
    let recent = newest_value_per_instrument(&[Uuid::nil()], None).sql;
    assert!(recent.contains(r#"DISTINCT ON ("sensor_id")"#), "{recent}");
    assert!(
        recent.contains(r#"ORDER BY "sensor_id" ASC, "time" DESC"#),
        "{recent}"
    );
    assert!(
        recent.contains("time > now() - INTERVAL '90 days'"),
        "{recent}"
    );
    assert!(
        recent.contains(r#"COALESCE("calibrated_value", "raw_value")"#),
        "{recent}"
    );

    let now = chrono::Utc::now();
    let windowed = newest_value_per_instrument(&[Uuid::nil()], Some((now, now))).sql;
    assert!(
        windowed.contains(r#"DISTINCT ON ("sensor_id")"#),
        "{windowed}"
    );
    assert!(
        !windowed.contains("INTERVAL '90 days'"),
        "a given window replaces the default one: {windowed}"
    );
}

#[test]
fn test_curve_use_per_instrument_counts_curves_and_their_newest_use() {
    let sql = curve_use_per_instrument(&[Uuid::nil()]).sql;

    assert!(sql.contains(r#"FROM "standard_curves" AS "sc""#), "{sql}");
    assert!(
        sql.contains(r#"LEFT JOIN "readings" AS "r" ON "r"."standard_curve_id" = "sc"."id""#),
        "{sql}"
    );
    assert!(sql.contains(r#"COUNT(DISTINCT "sc"."id")"#), "{sql}");
    assert!(sql.contains(r#"MAX("r"."time")"#), "{sql}");
    assert!(sql.contains(r#"GROUP BY "sc"."sensor_id""#), "{sql}");
}

// Scenario: a write names an instrument by id. Expected behaviour: a bookkeeping row and a
// retired row are both refused, and the message says which of the two it is.
mod instrument_refusal {
    use super::super::{InstrumentKind, instrument_refusal};

    #[test]
    fn test_instrument_refusal_accepts_an_active_device() {
        assert!(
            instrument_refusal(
                InstrumentKind::Device,
                Some(true),
                "miniDOT 7392",
                "deployed"
            )
            .is_none()
        );
    }

    #[test]
    fn test_instrument_refusal_accepts_an_active_lab_instrument() {
        assert!(
            instrument_refusal(InstrumentKind::Lab, Some(true), "Shimadzu TOC", "deployed")
                .is_none()
        );
    }

    #[test]
    fn test_instrument_refusal_reads_an_unset_flag_as_active() {
        assert!(
            instrument_refusal(InstrumentKind::Device, None, "miniDOT 7392", "deployed").is_none()
        );
    }

    #[test]
    fn test_instrument_refusal_rejects_a_retired_device() {
        let message = instrument_refusal(
            InstrumentKind::Device,
            Some(false),
            "miniDOT 7392",
            "deployed to a site",
        )
        .expect("a retired instrument is refused");
        assert!(message.contains("miniDOT 7392"), "{message}");
        assert!(message.contains("retired"), "{message}");
        assert!(message.contains("deployed to a site"), "{message}");
    }

    #[test]
    fn test_instrument_refusal_rejects_a_retired_lab_instrument() {
        assert!(
            instrument_refusal(InstrumentKind::Lab, Some(false), "Shimadzu TOC", "deployed")
                .is_some()
        );
    }

    #[test]
    fn test_instrument_refusal_rejects_an_active_source_parameter_row() {
        let message = instrument_refusal(
            InstrumentKind::SourceParameter,
            Some(true),
            "DOC (cnet)",
            "deployed",
        )
        .expect("a bookkeeping row is refused");
        assert!(message.contains("nothing was declared"), "{message}");
    }

    #[test]
    fn test_instrument_refusal_rejects_an_active_entry_channel_row() {
        assert!(
            instrument_refusal(
                InstrumentKind::EntryChannel,
                Some(true),
                "Martigny Depth (grab_sample)",
                "deployed",
            )
            .is_some()
        );
    }

    // A row that is both is refused as bookkeeping: that is the fact about the row itself, and
    // retiring one changes nothing about what it stands for.
    #[test]
    fn test_instrument_refusal_names_bookkeeping_before_retirement() {
        let message = instrument_refusal(
            InstrumentKind::EntryChannel,
            Some(false),
            "Martigny Depth (grab_sample)",
            "deployed",
        )
        .expect("refused");
        assert!(message.contains("nothing was declared"), "{message}");
    }
}
