use super::*;

/// Scenario: the stale-data probe is built rather than written out.
/// Expected behaviour: it still walks the slot index backwards for each cadence and measures the
/// gap over the last five spot instants, so a renamed column fails the build, not the sweep.
#[test]
fn test_stale_slots_query_walks_each_cadence_at_the_slot() {
    let statement = stale_slots_query();
    let sql = statement.sql.clone();

    assert!(sql.contains(r#"FROM "site_parameters" AS "sp""#), "{sql}");
    assert!(
        sql.contains(r#"JOIN "sites" AS "s" ON "s"."id" = "sp"."site_id""#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"JOIN "parameters" AS "p" ON "p"."id" = "sp"."parameter_id""#),
        "{sql}"
    );
    assert!(sql.contains("LEFT JOIN LATERAL"), "{sql}");
    assert!(
        sql.contains(r#""r"."measurement_type" IS DISTINCT FROM 'spot'"#),
        "a spot row is excluded by IS DISTINCT FROM, not by inequality: {sql}"
    );
    assert!(sql.contains(r#""r"."withdrawn_at" IS NULL"#), "{sql}");
    assert!(
        sql.contains(r#"LAG("s"."t") OVER (ORDER BY "s"."t")"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"EXTRACT(EPOCH FROM MAX("g"."gap"))::float8"#),
        "{sql}"
    );
    assert!(sql.contains(r#""sp"."is_active""#), "{sql}");
    // The gap is measured over the five newest instants; every literal is bound, LIMIT included.
    let bound = format!("{:?}", statement.values);
    assert!(bound.contains('5'), "{bound}");
}

/// Scenario: the battery-trend probe is built rather than written out.
/// Expected behaviour: the latest value and the slope are still two correlated subqueries over the
/// site's own continuous readings, and the slope still measures only the quiet hours.
#[test]
fn test_battery_trend_query_reads_the_night_window_only() {
    let statement = battery_trend_query(uuid::Uuid::nil());
    let sql = statement.sql.clone();

    assert!(sql.contains(r#"FROM "sites" AS "s""#), "{sql}");
    assert!(
        sql.contains(r#"COALESCE("r2"."calibrated_value", "r2"."raw_value")"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"regr_slope(COALESCE("r3"."calibrated_value", "r3"."raw_value"), EXTRACT(EPOCH FROM "r3"."time") / 86400.0)"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""r3"."time" > NOW() - INTERVAL '7 days'"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#"EXTRACT(HOUR FROM "r3"."time") BETWEEN 2 AND 4"#),
        "{sql}"
    );
    assert!(
        sql.contains(r#""r2"."measurement_type" IS DISTINCT FROM 'spot'"#),
        "{sql}"
    );
}

/// The claim and its release both interpolate the column's name, so the name the enum renders has
/// to be the table's own, and each notice has to claim its own marker.
mod claim_column {
    use super::super::claim_column;
    use crate::routes::private::alarms::models::alarm_event;

    #[test]
    fn test_claim_column_is_per_notice() {
        assert!(matches!(
            claim_column(true),
            alarm_event::Column::NotifiedAt
        ));
        assert!(matches!(
            claim_column(false),
            alarm_event::Column::ResolutionNotifiedAt
        ));
    }

    #[test]
    fn test_claim_column_renders_the_table_s_own_name() {
        assert_eq!(sea_orm::Iden::to_string(&claim_column(true)), "notified_at");
        assert_eq!(
            sea_orm::Iden::to_string(&claim_column(false)),
            "resolution_notified_at"
        );
    }
}

mod judged_against_tests {
    use super::super::judged_against;
    use chrono::{Duration, Utc};

    const BASE_HOURS: i64 = 6;

    fn base() -> Duration {
        Duration::hours(BASE_HOURS)
    }

    #[test]
    fn test_a_cadence_that_never_produced_data_is_not_judged() {
        assert!(judged_against(None, false, None, base()).is_none());
        assert!(judged_against(None, true, Some(90_000.0), base()).is_none());
    }

    #[test]
    fn test_a_continuous_cadence_is_judged_against_the_configured_threshold() {
        let last = Utc::now() - Duration::hours(1);
        assert_eq!(
            judged_against(Some(last), false, None, base()),
            Some((last, base()))
        );
    }

    /// Two samples on one field day describe the campaign, not the cadence, so a slot whose widest
    /// recent gap is under the logger threshold is not late against anything.
    #[test]
    fn test_a_grab_cadence_with_no_visible_rhythm_is_not_judged() {
        let last = Utc::now() - Duration::days(400);
        let within_a_day = (base() - Duration::hours(1)).num_seconds() as f64;
        assert!(judged_against(Some(last), true, Some(within_a_day), base()).is_none());
        assert!(judged_against(Some(last), true, None, base()).is_none());
    }

    /// A monthly campaign is late at three months, not at six hours.
    #[test]
    fn test_a_grab_cadence_is_judged_against_three_of_its_widest_recent_gaps() {
        let last = Utc::now() - Duration::days(400);
        let monthly = Duration::days(30);
        assert_eq!(
            judged_against(Some(last), true, Some(monthly.num_seconds() as f64), base()),
            Some((last, monthly * 3))
        );
    }
}

/// Scenario: open holds of every kind, some a person owes an action on and some not.
/// Expected behaviour: one line per place the work is done, and no line for a kind nobody owes.
#[test]
fn test_owed_holds_message_names_where_each_is_worked() {
    use crate::routes::private::sync::models::HoldKind;
    let counts = [
        (HoldKind::ReplicateStats, 157),
        (HoldKind::SourceModified, 4),
        (HoldKind::SkippedOutput, 2),
        (HoldKind::UnverifiedVisit, 1),
        (HoldKind::UnverifiedEntry, 3),
        (HoldKind::StaleOutput, 5),
        (HoldKind::BrakeFired, 1),
    ];
    let (subject, body) = owed_holds_message(&counts).unwrap();
    // 1 + 3 + 5 + 1
    assert_eq!(subject, "RIVER Data: 10 item(s) waiting for a manager");
    assert!(
        body.contains("4 field day entries to verify on Visits"),
        "{body}"
    );
    assert!(
        body.contains("5 calculation outputs to recompute on the Toolbox"),
        "{body}"
    );
    assert!(
        body.contains("1 fired brake or device identity change on Streams"),
        "{body}"
    );
    assert!(!body.contains("157"), "{body}");
    assert!(!body.contains("Audits"), "{body}");
}

#[test]
fn test_owed_holds_message_is_none_when_nothing_is_owed() {
    use crate::routes::private::sync::models::HoldKind;
    assert!(owed_holds_message(&[]).is_none());
    assert!(owed_holds_message(&[(HoldKind::ReplicateStats, 157)]).is_none());
}
