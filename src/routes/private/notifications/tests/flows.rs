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
